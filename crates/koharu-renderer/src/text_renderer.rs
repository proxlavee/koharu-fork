//! Text layout and portable glyph recording.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use koharu_rasterizer::{
    PreparedGlyph, PreparedGlyphRun, PreparedResource, PreparedScene, PreparedSceneCommand,
    ResourceId,
};
use vello::kurbo::Affine;

use crate::{
    Error, FontStyle, HyphenationPolicy, LayoutRun, RenderBounds, RenderDiagnostic,
    Result as RenderResult, TextAlign, TextLayout, WritingMode,
    artwork::{ArtworkContext, ArtworkField},
    bubble::LayoutBox,
    fonts::{Fonts, font_key},
    free_text::FreeTextCandidate,
    images::DecodedImage,
    layout::CompositionQuality,
    script::is_chinese_or_japanese_text,
};

// Detected outlines are measured at the source font size, while translated
// text may auto-fit smaller. Preserve that ratio but bound unusually broad
// detected bands; explicitly authored widths remain absolute.
const MAX_GENERATED_STROKE_FONT_RATIO: f32 = 0.12;
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TextNodeDescriptor {
    pub(crate) entity: koharu_scene::EntityId,
    pub(crate) text: String,
    pub(crate) text_role: Option<String>,
    pub(crate) typography_origin: Option<koharu_scene::Origin>,
    pub(crate) language: Option<koharu_scene::LanguageTag>,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) balloon_contour: Option<Vec<(f32, f32)>>,
    pub(crate) flow_contours: Vec<Vec<(f32, f32)>>,
    pub(crate) preferred_center: Option<(f32, f32)>,
    pub(crate) free_text_candidates: Vec<FreeTextCandidate>,
    pub(crate) artwork: Option<ArtworkContext>,
    pub(crate) preferred_line_count: Option<usize>,
    pub(crate) automatic_free_text: bool,
    pub(crate) preferred_font: Option<String>,
    pub(crate) font_families: Vec<String>,
    pub(crate) font_weight: Option<u16>,
    pub(crate) font_style: Option<FontStyle>,
    pub(crate) font_size: Option<f32>,
    pub(crate) reviewed_font_size: Option<f32>,
    pub(crate) source_font_size: Option<f32>,
    pub(crate) font_size_reference: Option<FontSizeReference>,
    pub(crate) minimum_font_size: f32,
    pub(crate) auto_fit: bool,
    pub(crate) alignment: TextAlign,
    pub(crate) writing_mode: WritingMode,
    pub(crate) foreground_color: [u8; 4],
    pub(crate) stroke: Option<StrokeOptions>,
    pub(crate) allow_discretionary_hyphenation: Option<bool>,
    pub(crate) placement_offset: (f32, f32),
    pub(crate) line_height: f32,
    pub(crate) letter_spacing: f32,
    pub(crate) word_spacing: f32,
    pub(crate) text_inset: [f32; 4],
    pub(crate) point_text: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FontSizeReference {
    CrossLine(f32),
    InkArea(f32),
}

impl FontSizeReference {
    fn resolve(self, template: &TextLayout<'_>, text: &str) -> Result<f32> {
        match self {
            Self::CrossLine(size) => Ok(size),
            Self::InkArea(area) => {
                anyhow::ensure!(area.is_finite() && area > 0.0, "invalid source ink area");
                // A narrow symbol's cross-line thickness is not an em size.
                // Preserve its two-dimensional visual scale without inheriting
                // the source writing axis or constraining the target rectangle.
                // Skrifa quantizes a TrueType metrics probe to 26.6 before
                // scaling. An integer probe avoids amplifying that truncation
                // when a thin glyph's measured area is converted back to ems.
                let probe_size = area.sqrt().ceil();
                let probe = template.clone().with_font_size(probe_size).run(text)?;
                let measured = template
                    .ink_bounds(probe_size, &probe.lines)
                    .map_or(0.0, |(left, top, right, bottom)| {
                        (right - left) * (bottom - top)
                    });
                anyhow::ensure!(
                    measured.is_finite() && measured > 0.0,
                    "symbol has no measurable ink"
                );
                let size = probe_size * (area / measured).sqrt();
                anyhow::ensure!(
                    size.is_finite() && size > 0.0,
                    "invalid symbol size reference"
                );
                Ok(size)
            }
        }
    }
}

pub(crate) struct RenderedTextNode {
    pub(crate) scene: Arc<PreparedScene>,
    pub(crate) resources: Arc<[PreparedResource]>,
    pub(crate) local_bounds: RenderBounds,
    pub(crate) metadata: RenderedTextMetadata,
    pub(crate) diagnostics: Vec<RenderDiagnostic>,
}

pub(crate) struct RenderedTextMetadata {
    pub(crate) rendered_bounds: RenderBounds,
    pub(crate) layout_bounds: RenderBounds,
    pub(crate) post_script_fonts: Vec<String>,
    pub(crate) font_size: f32,
    pub(crate) color: [u8; 4],
    pub(crate) stroke_color: Option<[u8; 4]>,
    pub(crate) stroke_width: f32,
    pub(crate) line_ranges: Vec<(usize, usize)>,
    pub(crate) discretionary_hyphens: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StrokeOptions {
    pub color: [u8; 4],
    pub width: StrokeWidth,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StrokeWidth {
    Absolute(f32),
    Generated {
        width_px: f32,
        reference_font_size: Option<f32>,
    },
    FontRelative(f32),
}

impl StrokeOptions {
    pub(crate) fn for_font_size(self, font_size: f32) -> ResolvedStrokeOptions {
        let width_px = match self.width {
            StrokeWidth::Absolute(width_px) => width_px,
            StrokeWidth::Generated {
                width_px,
                reference_font_size,
            } => reference_font_size
                .filter(|reference| reference.is_finite() && *reference > 0.0)
                .map_or(width_px, |reference| width_px * font_size / reference)
                .min((font_size * MAX_GENERATED_STROKE_FONT_RATIO).max(0.0)),
            StrokeWidth::FontRelative(ratio) => {
                (font_size * ratio).min((font_size * MAX_GENERATED_STROKE_FONT_RATIO).max(0.0))
            }
        };
        ResolvedStrokeOptions {
            color: self.color,
            width_px: if self.color[3] == 0 { 0.0 } else { width_px },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ResolvedStrokeOptions {
    pub color: [u8; 4],
    pub width_px: f32,
}

/// Paint options used when recording one laid-out text run into a Vello scene.
#[derive(Debug, Clone)]
pub(crate) struct TextRenderOptions {
    pub color: [u8; 4],
    pub hint_glyphs: bool,
    pub padding: f32,
    pub baseline_shift: f32,
    pub stroke: Option<ResolvedStrokeOptions>,
}

impl Default for TextRenderOptions {
    fn default() -> Self {
        Self {
            color: [0, 0, 0, 255],
            hint_glyphs: true,
            padding: 0.0,
            baseline_shift: 0.0,
            stroke: None,
        }
    }
}

/// Shapes text and records the resulting glyphs into vector scenes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TextRenderer;

impl TextRenderer {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    pub(crate) fn layout<'a>(&self, builder: &TextLayout<'a>, text: &str) -> Result<LayoutRun<'a>> {
        builder.run(text)
    }

    pub(crate) fn render(
        &self,
        scene: &mut PreparedScene,
        resources: &mut Vec<PreparedResource>,
        layout: &LayoutRun<'_>,
        writing_mode: WritingMode,
        options: &TextRenderOptions,
        transform: Affine,
    ) {
        // The border is drawn first as an outward-only dilation of the glyph outline (see
        // `draw_layout`), then the ordinary fill is drawn on top in the foreground color. Unlike a
        // centered stroke, a dilation never grows inward, so it can't punch a hole through small
        // features (e.g. dots) once the border width exceeds their radius.
        if let Some(stroke) = options
            .stroke
            .filter(|stroke| stroke.width_px > 0.0 && stroke.color[3] > 0)
        {
            draw_layout(
                scene,
                resources,
                layout,
                writing_mode,
                options,
                transform,
                GlyphPaint {
                    color: stroke.color,
                    dilation_px: stroke.width_px,
                },
            );
        }
        draw_layout(
            scene,
            resources,
            layout,
            writing_mode,
            options,
            transform,
            GlyphPaint {
                color: options.color,
                dilation_px: 0.0,
            },
        );
    }

    pub(crate) fn render_descriptor(
        &self,
        descriptor: &TextNodeDescriptor,
        fonts: &Fonts,
        images: &HashMap<koharu_scene::BlobId, Arc<DecodedImage>>,
    ) -> RenderResult<RenderedTextNode> {
        let is_bubble_text = descriptor.balloon_contour.is_some();
        let frame = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: descriptor.width,
            height: descriptor.height,
        };
        let ordinary_bounds = if is_bubble_text {
            inset(frame, descriptor.text_inset)
        } else {
            frame
        };
        if ordinary_bounds.width <= 0.0 || ordinary_bounds.height <= 0.0 {
            return Err(Error::invalid(format!(
                "text inset leaves no layout area for entity {}",
                descriptor.entity
            )));
        }
        let fonts = fonts
            .resolve(
                descriptor.preferred_font.as_deref(),
                descriptor.font_weight,
                descriptor.font_style,
                &descriptor.font_families,
                &descriptor.text,
                descriptor
                    .language
                    .as_ref()
                    .map(koharu_scene::LanguageTag::as_str),
            )
            .map_err(|source| Error::Font {
                entity: descriptor.entity,
                source,
            })?;
        let readability_minimum = readability_minimum(descriptor);
        let mut template = TextLayout::new(&fonts[0])
            .with_fallback_fonts(&fonts[1..])
            .with_writing_mode(descriptor.writing_mode)
            .with_alignment(descriptor.alignment)
            .with_line_height(descriptor.line_height)
            .with_spacing(descriptor.letter_spacing, descriptor.word_spacing)
            .with_cjk_punctuation_layout(
                is_chinese_or_japanese_text(&descriptor.text)
                    || descriptor
                        .language
                        .as_ref()
                        .is_some_and(|language| is_chinese_or_japanese_language(language.as_str())),
            );
        if let Some(language) = &descriptor.language {
            template = template.with_language_tag(language.as_str());
        }
        if (is_bubble_text || descriptor.automatic_free_text)
            && descriptor.writing_mode == WritingMode::Horizontal
        {
            template = template.with_hyphenation_policy(HyphenationPolicy::LastResort);
        }
        if descriptor.allow_discretionary_hyphenation == Some(false) {
            template = template.with_hyphenation_policy(HyphenationPolicy::Disabled);
        }
        if descriptor.automatic_free_text && descriptor.writing_mode == WritingMode::Horizontal {
            template = template.with_natural_line_breaks();
        }
        let reference = if descriptor.auto_fit
            && !descriptor.point_text
            && descriptor.font_size.is_none()
            && descriptor.reviewed_font_size.is_none()
        {
            descriptor
                .font_size_reference
                .map(|reference| reference.resolve(&template, &descriptor.text))
                .transpose()
                .map_err(|source| Error::Layout {
                    entity: descriptor.entity,
                    source,
                })?
        } else {
            None
        };
        if let Some(reference) = reference {
            template = template.with_preferred_font_size(reference);
        }

        if let Some(contour) = &descriptor.balloon_contour {
            let [top, _, _, left] = descriptor.text_inset;
            let contour = contour.iter().map(|&(x, y)| (x - left, y - top)).collect();
            if descriptor.flow_contours.is_empty() {
                template = template.with_comic_balloon(
                    ordinary_bounds.width,
                    ordinary_bounds.height,
                    contour,
                    descriptor.text_inset.into_iter().fold(0.0, f32::max),
                );
            } else {
                let mut contours = Vec::with_capacity(descriptor.flow_contours.len() + 1);
                contours.push(contour);
                contours.extend(descriptor.flow_contours.iter().map(|flow_contour| {
                    flow_contour
                        .iter()
                        .map(|&(x, y)| (x - left, y - top))
                        .collect()
                }));
                template = template.with_comic_balloon_constraints(
                    ordinary_bounds.width,
                    ordinary_bounds.height,
                    contours,
                    descriptor.text_inset.into_iter().fold(0.0, f32::max),
                );
            }
            if let Some(center) = descriptor.preferred_center {
                // Automatic flow text prefers the usable body's center. Explicit
                // review proposals can move within the same hard contour limits.
                let strict_centering = !descriptor.flow_contours.is_empty()
                    && descriptor.reviewed_font_size.is_none()
                    && descriptor.preferred_line_count.is_none();
                template = template.with_comic_preferred_center(
                    (center.0 - left, center.1 - top),
                    strict_centering,
                );
            }
            if let Some(line_count) = descriptor.preferred_line_count {
                template = template.with_comic_preferred_line_count(line_count);
            }
        }
        let selected = if !descriptor.free_text_candidates.is_empty()
            && descriptor.auto_fit
            && !descriptor.point_text
            && !is_bubble_text
        {
            self.select_free_text_layout(descriptor, &template, reference, images)?
        } else {
            let (minimum, maximum) = font_size_limits(descriptor, ordinary_bounds);
            let mut builder = template;
            if !descriptor.point_text {
                builder = builder
                    .with_max_width(ordinary_bounds.width)
                    .with_max_height(ordinary_bounds.height);
            }
            builder = if descriptor.auto_fit && !descriptor.point_text {
                builder
                    .with_max_font_size(maximum)
                    .with_min_font_size(minimum)
            } else {
                builder.with_font_size(descriptor.font_size.unwrap_or(maximum))
            };
            SelectedTextLayout {
                layout: builder
                    .run_with_ink_outset(&descriptor.text, |size| {
                        descriptor
                            .stroke
                            .map_or(0.0, |stroke| stroke.for_font_size(size).width_px)
                    })
                    .map_err(|source| Error::Layout {
                        entity: descriptor.entity,
                        source,
                    })?,
                bounds: ordinary_bounds,
                preferred_center: None,
                maximum_visual_area: None,
            }
        };
        let SelectedTextLayout {
            layout,
            bounds,
            preferred_center,
            maximum_visual_area,
        } = selected;
        let resolved_stroke = descriptor
            .stroke
            .map(|stroke| stroke.for_font_size(layout.font_size));
        let stroke_padding = resolved_stroke.map_or(0.0, |stroke| stroke.width_px.max(0.0));
        let placement_bounds = if maximum_visual_area.is_some() {
            inset(bounds, [stroke_padding; 4])
        } else {
            bounds
        };
        let (mut x, mut y) = if descriptor.point_text {
            (bounds.x, bounds.y)
        } else if !is_bubble_text && let Some(center) = preferred_center {
            anchored_placement(placement_bounds, layout.width, layout.height, center)
        } else {
            placement(bounds, layout.width, layout.height)
        };
        x += layout.placement_offset_x();
        y += layout.placement_offset_y();
        x += descriptor.placement_offset.0;
        y += descriptor.placement_offset.1;
        let transform = Affine::translate((f64::from(x), f64::from(y)));
        let color = descriptor.foreground_color;
        let mut options = TextRenderOptions {
            color,
            stroke: None,
            ..TextRenderOptions::default()
        };
        let mut scene = PreparedScene::default();
        let mut resources = Vec::new();
        if let Some(stroke) = resolved_stroke {
            options.stroke = Some(stroke);
        }
        self.render(
            &mut scene,
            &mut resources,
            &layout,
            descriptor.writing_mode,
            &options,
            transform,
        );
        let mut diagnostics = Vec::new();
        if layout.font_size + f32::EPSILON < readability_minimum {
            diagnostics.push(RenderDiagnostic::TextBelowReadableSize {
                entity: descriptor.entity,
                font_size: layout.font_size,
                minimum_font_size: readability_minimum,
            });
        }
        let visual_width = layout.width + stroke_padding * 2.0;
        let visual_height = layout.height + stroke_padding * 2.0;
        let exceeds_visual_area = maximum_visual_area.is_some_and(|area| {
            visual_width * visual_height > area + area.max(1.0) * f32::EPSILON * 16.0
        });
        if layout.overflowed()
            || visual_width > bounds.width + f32::EPSILON
            || visual_height > bounds.height + f32::EPSILON
            || exceeds_visual_area
        {
            diagnostics.push(RenderDiagnostic::TextOverflow {
                entity: descriptor.entity,
                available: bounds.into(),
                actual_width: visual_width,
                actual_height: visual_height,
                font_size: layout.font_size,
            });
        }
        let rendered_bounds = RenderBounds {
            x,
            y,
            width: layout.width,
            height: layout.height,
        };
        #[cfg(debug_assertions)]
        if tracing::enabled!(
            target: "koharu_typesetting_probe",
            tracing::Level::TRACE
        ) {
            let selected_hyphens = layout.discretionary_hyphen_count();
            let source_relative_shrink = descriptor.source_font_size.is_some_and(|source| {
                source.is_finite() && source > 0.0 && layout.font_size + f32::EPSILON < source * 0.8
            });
            let emergency_band = layout.font_size <= descriptor.minimum_font_size;
            if descriptor.auto_fit
                && (emergency_band || source_relative_shrink || descriptor.automatic_free_text)
            {
                let lines = layout
                    .lines
                    .iter()
                    .map(|line| {
                        descriptor
                            .text
                            .get(line.range.clone())
                            .unwrap_or("<invalid>")
                    })
                    .collect::<Vec<_>>();
                let contour = descriptor
                    .balloon_contour
                    .as_deref()
                    .and_then(contour_debug_summary);
                let flow_contours = descriptor
                    .flow_contours
                    .iter()
                    .filter_map(|contour| contour_debug_summary(contour))
                    .collect::<Vec<_>>();
                tracing::trace!(
                    target: "koharu_typesetting_probe",
                    marker = "text_result",
                    entity = %descriptor.entity,
                    text = descriptor.text.as_str(),
                    role = ?descriptor.text_role.as_deref(),
                    typography_origin = ?descriptor.typography_origin.as_ref(),
                    auto_fit = descriptor.auto_fit,
                    automatic_free_text = descriptor.automatic_free_text,
                    point_text = descriptor.point_text,
                    writing_mode = ?descriptor.writing_mode,
                    source_font_size = ?descriptor.source_font_size,
                    explicit_font_size = ?descriptor.font_size,
                    minimum_font_size = descriptor.minimum_font_size,
                    frame_width = descriptor.width,
                    frame_height = descriptor.height,
                    layout_bounds = ?bounds,
                    balloon_contour = ?contour,
                    flow_contours = ?flow_contours,
                    preferred_center = ?descriptor.preferred_center,
                    free_text_candidate_count = descriptor.free_text_candidates.len(),
                    preferred_line_count = ?descriptor.preferred_line_count,
                    selected_size = layout.font_size,
                    selected_width = layout.width,
                    selected_height = layout.height,
                    selected_hyphens,
                    selected_overflowed = layout.overflowed(),
                    placement_x = x,
                    placement_y = y,
                    stroke = ?resolved_stroke,
                    maximum_visual_area = ?maximum_visual_area,
                    source_relative_shrink,
                    emergency_band,
                    ?lines,
                );
            }
        }
        Ok(RenderedTextNode {
            scene: Arc::new(scene),
            resources: resources.into(),
            local_bounds: RenderBounds {
                x: rendered_bounds.x - stroke_padding,
                y: rendered_bounds.y - stroke_padding,
                width: rendered_bounds.width + stroke_padding * 2.0,
                height: rendered_bounds.height + stroke_padding * 2.0,
            },
            metadata: RenderedTextMetadata {
                rendered_bounds,
                layout_bounds: if descriptor.point_text {
                    rendered_bounds
                } else {
                    bounds.into()
                },
                post_script_fonts: fonts
                    .iter()
                    .map(|font| font.post_script_name().to_owned())
                    .collect(),
                font_size: layout.font_size,
                color,
                stroke_color: resolved_stroke.map(|stroke| stroke.color),
                stroke_width: resolved_stroke.map_or(0.0, |stroke| stroke.width_px),
                line_ranges: layout
                    .lines
                    .iter()
                    .map(|line| (line.range.start, line.range.end))
                    .collect(),
                discretionary_hyphens: layout.discretionary_hyphen_count(),
            },
            diagnostics,
        })
    }

    fn select_free_text_layout<'a>(
        &self,
        descriptor: &TextNodeDescriptor,
        template: &TextLayout<'a>,
        reference: Option<f32>,
        images: &HashMap<koharu_scene::BlobId, Arc<DecodedImage>>,
    ) -> RenderResult<SelectedTextLayout<'a>> {
        let candidates = descriptor
            .free_text_candidates
            .iter()
            .copied()
            .filter(|candidate| {
                candidate.bounds.width.is_finite()
                    && candidate.bounds.width > 0.0
                    && candidate.bounds.height.is_finite()
                    && candidate.bounds.height > 0.0
                    && candidate.maximum_visual_area.is_finite()
                    && candidate.maximum_visual_area > 0.0
                    && candidate.translation_visual_area_scale.is_finite()
                    && candidate.translation_visual_area_scale > 0.0
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err(Error::invalid(format!(
                "free-text candidate set is invalid for entity {}",
                descriptor.entity,
            )));
        }

        let artwork = descriptor
            .artwork
            .as_ref()
            .map(|context| {
                let source_em = descriptor.source_font_size.ok_or_else(|| {
                    Error::invalid("artwork-aware lettering requires its source scale")
                })?;
                ArtworkField::build(context, images, &candidates, source_em)
            })
            .transpose()?;

        let reference = if let Some(reference) = reference {
            reference
        } else {
            // Missing source typography gets one shared reference from the source
            // footprint. Each expanded rectangle must not invent a larger scale.
            let source_family = candidates
                .iter()
                .map(|candidate| candidate.translation_visual_area_scale)
                .min_by(f32::total_cmp)
                .expect("validated candidates are nonempty");
            let mut reference: Option<f32> = None;
            for candidate in candidates
                .iter()
                .filter(|candidate| candidate.translation_visual_area_scale == source_family)
            {
                let (minimum, maximum) = font_size_limits(descriptor, candidate.bounds);
                if let Some(layout) = template
                    .run_fitting_with_bounds(
                        &descriptor.text,
                        minimum,
                        maximum,
                        |size| free_text_content_bounds(descriptor, candidate, size),
                        |layout| free_text_layout_fits(descriptor, candidate, layout),
                        |layout, reference| CompositionQuality {
                            cost: layout.composition_cost(&descriptor.text, reference),
                            ..CompositionQuality::default()
                        },
                    )
                    .map_err(|source| Error::Layout {
                        entity: descriptor.entity,
                        source,
                    })?
                {
                    reference =
                        Some(reference.map_or(layout.font_size, |size| size.max(layout.font_size)));
                }
            }
            reference.unwrap_or_else(|| {
                descriptor
                    .source_font_size
                    .unwrap_or_else(|| {
                        automatic_maximum(
                            descriptor,
                            LayoutBox {
                                x: 0.0,
                                y: 0.0,
                                width: descriptor.width,
                                height: descriptor.height,
                            },
                        )
                    })
                    .max(descriptor.minimum_font_size)
            })
        };
        let builder = template.clone().with_preferred_font_size(reference);
        let mut measured = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let (minimum, maximum) = font_size_limits(descriptor, candidate.bounds);
            let layout = match builder
                .run_fitting_with_bounds(
                    &descriptor.text,
                    minimum,
                    maximum,
                    |size| free_text_content_bounds(descriptor, &candidate, size),
                    |layout| free_text_layout_fits(descriptor, &candidate, layout),
                    |layout, reference| {
                        free_text_composition_quality(
                            descriptor,
                            &candidate,
                            layout,
                            reference,
                            artwork.as_ref(),
                        )
                    },
                )
                .map_err(|source| Error::Layout {
                    entity: descriptor.entity,
                    source,
                })? {
                Some(layout) => layout,
                None => self.layout_free_text_at(descriptor, &candidate, &builder, minimum)?,
            };
            measured.push(MeasuredFreeTextLayout {
                fits: free_text_layout_fits(descriptor, &candidate, &layout),
                quality: free_text_composition_quality(
                    descriptor,
                    &candidate,
                    &layout,
                    reference,
                    artwork.as_ref(),
                ),
                layout,
                candidate,
            });
        }
        let selected_index = measured
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                right
                    .fits
                    .cmp(&left.fits)
                    .then_with(|| left.quality.compare(right.quality))
                    .then_with(|| right.layout.font_size.total_cmp(&left.layout.font_size))
                    .then_with(|| {
                        left.candidate
                            .translation_visual_area_scale
                            .total_cmp(&right.candidate.translation_visual_area_scale)
                    })
            })
            .map(|(index, _)| index)
            .expect("validated candidates are nonempty");
        #[cfg(debug_assertions)]
        self.log_free_text_selection(descriptor, &measured, selected_index, "composition");
        Ok(measured.swap_remove(selected_index).into_selected())
    }

    #[cfg(debug_assertions)]
    fn log_free_text_selection(
        &self,
        descriptor: &TextNodeDescriptor,
        measured: &[MeasuredFreeTextLayout<'_>],
        selected_index: usize,
        decision: &'static str,
    ) {
        if !tracing::enabled!(
            target: "koharu_typesetting_probe",
            tracing::Level::TRACE
        ) {
            return;
        }
        let trace = measured
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                (
                    index == selected_index,
                    candidate.layout.font_size,
                    candidate.layout.lines.len(),
                    candidate.layout.discretionary_hyphen_count(),
                    candidate.quality,
                    candidate.fits,
                    (
                        candidate.candidate.bounds,
                        candidate.candidate.maximum_visual_area,
                        candidate.candidate.translation_visual_area_scale,
                    ),
                    candidate
                        .layout
                        .lines
                        .iter()
                        .map(|line| {
                            descriptor
                                .text
                                .get(line.range.clone())
                                .unwrap_or("<invalid>")
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        tracing::trace!(
            target: "koharu_typesetting_probe",
            marker = "free_text_quality",
            entity = %descriptor.entity,
            text = descriptor.text.as_str(),
            role = ?descriptor.text_role.as_deref(),
            typography_origin = ?descriptor.typography_origin.as_ref(),
            source_font_size = ?descriptor.source_font_size,
            frame_width = descriptor.width,
            frame_height = descriptor.height,
            preferred_line_count = ?descriptor.preferred_line_count,
            decision,
            selected_index,
            ?trace,
        );
    }

    fn layout_free_text_at<'a>(
        &self,
        descriptor: &TextNodeDescriptor,
        candidate: &FreeTextCandidate,
        template: &TextLayout<'a>,
        font_size: f32,
    ) -> RenderResult<LayoutRun<'a>> {
        let (width, height) = free_text_content_bounds(descriptor, candidate, font_size);
        self.layout(
            &template
                .clone()
                .with_font_size(font_size)
                .with_max_width(width.max(0.5))
                .with_max_height(height.max(0.5)),
            &descriptor.text,
        )
        .map_err(|source| Error::Layout {
            entity: descriptor.entity,
            source,
        })
    }
}

#[cfg(debug_assertions)]
fn contour_debug_summary(points: &[(f32, f32)]) -> Option<(LayoutBox, f32)> {
    let &(first_x, first_y) = points.first()?;
    if !first_x.is_finite() || !first_y.is_finite() {
        return None;
    }
    let (mut left, mut right, mut top, mut bottom) = (first_x, first_x, first_y, first_y);
    let mut twice_area = 0.0;
    for index in 0..points.len() {
        let (x, y) = points[index];
        let (next_x, next_y) = points[(index + 1) % points.len()];
        if !x.is_finite() || !y.is_finite() || !next_x.is_finite() || !next_y.is_finite() {
            return None;
        }
        left = left.min(x);
        right = right.max(x);
        top = top.min(y);
        bottom = bottom.max(y);
        twice_area += x * next_y - next_x * y;
    }
    Some((
        LayoutBox {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        },
        twice_area.abs() * 0.5,
    ))
}

struct SelectedTextLayout<'a> {
    layout: LayoutRun<'a>,
    bounds: LayoutBox,
    preferred_center: Option<(f32, f32)>,
    maximum_visual_area: Option<f32>,
}

struct MeasuredFreeTextLayout<'a> {
    layout: LayoutRun<'a>,
    candidate: FreeTextCandidate,
    fits: bool,
    quality: CompositionQuality,
}

fn free_text_composition_quality(
    descriptor: &TextNodeDescriptor,
    candidate: &FreeTextCandidate,
    layout: &LayoutRun<'_>,
    reference: f32,
    artwork: Option<&ArtworkField>,
) -> CompositionQuality {
    let stroke = descriptor
        .stroke
        .map_or(0.0, |stroke| {
            stroke.for_font_size(layout.font_size).width_px
        })
        .max(0.0);
    let source_area = candidate.maximum_visual_area / candidate.translation_visual_area_scale;
    let painted_area = (layout.width + 2.0 * stroke) * (layout.height + 2.0 * stroke);
    let excess_area = (painted_area / source_area).max(1.0).ln();
    let (x, y) = anchored_placement(
        inset(candidate.bounds, [stroke; 4]),
        layout.width,
        layout.height,
        candidate.preferred_center,
    );
    let center = (
        x + layout.placement_offset_x() + layout.width * 0.5,
        y + layout.placement_offset_y() + layout.height * 0.5,
    );
    let displacement = (center.0 - candidate.preferred_center.0)
        .hypot(center.1 - candidate.preferred_center.1)
        / source_area.sqrt();
    CompositionQuality {
        requested_lines_error: descriptor
            .preferred_line_count
            .map_or(0, |requested| layout.lines.len().abs_diff(requested)),
        cost: layout.composition_cost(&descriptor.text, reference)
            + displacement.powi(2)
            + excess_area.powi(2) * 0.25
            + artwork.map_or(0.0, |field| {
                field.cost(LayoutBox {
                    x: x + layout.placement_offset_x() - stroke,
                    y: y + layout.placement_offset_y() - stroke,
                    width: layout.width + stroke * 2.0,
                    height: layout.height + stroke * 2.0,
                })
            }),
        ..CompositionQuality::default()
    }
}

impl<'a> MeasuredFreeTextLayout<'a> {
    fn into_selected(self) -> SelectedTextLayout<'a> {
        SelectedTextLayout {
            layout: self.layout,
            bounds: self.candidate.bounds,
            preferred_center: Some(self.candidate.preferred_center),
            maximum_visual_area: Some(self.candidate.maximum_visual_area),
        }
    }
}

fn free_text_layout_fits(
    descriptor: &TextNodeDescriptor,
    candidate: &FreeTextCandidate,
    layout: &LayoutRun<'_>,
) -> bool {
    let dilation = descriptor
        .stroke
        .map_or(0.0, |stroke| {
            stroke.for_font_size(layout.font_size).width_px
        })
        .max(0.0);
    let visual_width = layout.width + dilation * 2.0;
    let visual_height = layout.height + dilation * 2.0;
    let area_tolerance = candidate.maximum_visual_area.max(1.0) * f32::EPSILON * 16.0;
    !layout.overflowed()
        && visual_width <= candidate.bounds.width + f32::EPSILON
        && visual_height <= candidate.bounds.height + f32::EPSILON
        && visual_width * visual_height <= candidate.maximum_visual_area + area_tolerance
}

fn free_text_content_bounds(
    descriptor: &TextNodeDescriptor,
    candidate: &FreeTextCandidate,
    font_size: f32,
) -> (f32, f32) {
    let dilation = descriptor
        .stroke
        .map_or(0.0, |stroke| stroke.for_font_size(font_size).width_px)
        .max(0.0);
    (
        candidate.bounds.width - dilation * 2.0,
        candidate.bounds.height - dilation * 2.0,
    )
}

fn automatic_maximum(descriptor: &TextNodeDescriptor, bounds: LayoutBox) -> f32 {
    if descriptor.point_text {
        24.0
    } else if descriptor.writing_mode.is_vertical() {
        bounds.height
    } else {
        bounds.width
    }
}

fn readability_minimum(descriptor: &TextNodeDescriptor) -> f32 {
    descriptor
        .source_font_size
        .map_or(descriptor.minimum_font_size, |size| {
            descriptor
                .minimum_font_size
                .max(size * crate::MINIMUM_SOURCE_FONT_RATIO)
        })
}

fn font_size_limits(descriptor: &TextNodeDescriptor, bounds: LayoutBox) -> (f32, f32) {
    if let Some(size) = descriptor.reviewed_font_size {
        return (size, size);
    }
    if descriptor.auto_fit {
        let maximum = descriptor
            .font_size
            .unwrap_or_else(|| automatic_maximum(descriptor, bounds));
        // The source size is useful as a diagnostic readability target, but it is
        // not a geometric constraint after translation changes the script or text
        // length. Keep the configured absolute floor hard and report source-relative
        // shrinkage through `TextBelowReadableSize` instead of rendering overflow.
        (descriptor.minimum_font_size.min(maximum), maximum)
    } else {
        let size = descriptor
            .font_size
            .unwrap_or_else(|| automatic_maximum(descriptor, bounds));
        (size, size)
    }
}

fn is_chinese_or_japanese_language(language: &str) -> bool {
    language
        .split(['-', '_'])
        .next()
        .is_some_and(|primary| matches!(primary.to_ascii_lowercase().as_str(), "ja" | "zh"))
}

fn inset(rect: LayoutBox, [top, right, bottom, left]: [f32; 4]) -> LayoutBox {
    LayoutBox {
        x: rect.x + left,
        y: rect.y + top,
        width: (rect.width - left - right).max(0.0),
        height: (rect.height - top - bottom).max(0.0),
    }
}

fn placement(rect: LayoutBox, width: f32, height: f32) -> (f32, f32) {
    let x = rect.x + (rect.width - width) * 0.5;
    let remaining = rect.height - height;
    let y = rect.y + remaining * 0.5;
    (x, y)
}

fn anchored_placement(rect: LayoutBox, width: f32, height: f32, center: (f32, f32)) -> (f32, f32) {
    let maximum_x = (rect.x + rect.width - width).max(rect.x);
    let maximum_y = (rect.y + rect.height - height).max(rect.y);
    (
        (center.0 - width * 0.5).clamp(rect.x, maximum_x),
        (center.1 - height * 0.5).clamp(rect.y, maximum_y),
    )
}

/// Color and outward dilation for one glyph draw pass (border or ordinary fill).
struct GlyphPaint {
    color: [u8; 4],
    dilation_px: f32,
}

fn draw_layout(
    scene: &mut PreparedScene,
    resources: &mut Vec<PreparedResource>,
    layout: &LayoutRun<'_>,
    writing_mode: WritingMode,
    options: &TextRenderOptions,
    transform: Affine,
    paint: GlyphPaint,
) {
    for line in &layout.lines {
        let (baseline_x, baseline_y) = match writing_mode {
            WritingMode::Horizontal | WritingMode::VerticalRl => line.baseline,
        };
        let mut pen_x = 0.0;
        let mut pen_y = 0.0;
        let mut start = 0;

        while start < line.glyphs.len() {
            let font = line.glyphs[start].font;
            let key = font_key(font);
            let mut end = start + 1;
            while end < line.glyphs.len() && font_key(line.glyphs[end].font) == key {
                end += 1;
            }

            let mut glyphs = Vec::with_capacity(end - start);
            for glyph in &line.glyphs[start..end] {
                glyphs.push(PreparedGlyph {
                    id: glyph.glyph_id,
                    x: options.padding + baseline_x + pen_x + glyph.x_offset,
                    y: options.padding + baseline_y + pen_y
                        - glyph.y_offset
                        - options.baseline_shift,
                });
                pen_x += glyph.x_advance;
                pen_y -= glyph.y_advance;
            }

            let font_id = ResourceId::for_font(font.bytes());
            if !resources.iter().any(|candidate| candidate.id() == font_id) {
                resources.push(PreparedResource::font_shared(font.shared_bytes()));
            }
            let glyph_transform = font
                .synthetic_skew()
                .map(|angle| Affine::skew(-(angle.to_radians().tan() as f64), 0.0).as_coeffs());
            let synthetic_bold = if font.synthetic_bold() { 1.0 } else { 0.0 };
            scene
                .commands
                .push(PreparedSceneCommand::GlyphRun(PreparedGlyphRun {
                    font: font_id,
                    font_index: font.index(),
                    font_size: layout.font_size,
                    normalized_coords: font.normalized_coords().to_vec(),
                    transform: transform.as_coeffs(),
                    glyph_transform,
                    // Hinting folds a uniform zoom into `font_size` for crisp fill text, but a
                    // border needs the real transform so its dilation amount stays proportional
                    // to that same zoom instead of being rendered at a fixed pixel size.
                    hint: options.hint_glyphs && paint.dilation_px == 0.0,
                    embolden: [synthetic_bold + paint.dilation_px; 2],
                    color: paint.color,
                    glyphs,
                }));
            start = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use koharu_rasterizer::{
        Bounds, CompositionCommand, LayerId, LayerKind as PreparedLayerKind, Point,
        PreparedContent, PreparedFrame, PreparedFrameBundle, PreparedFrameManifest, PreparedLayer,
        PreparedResourcePacket, PreparedResourceStore, Presentation, Revision,
    };
    use koharu_scene::EntityId;

    use super::*;
    use crate::fonts::{FontRequest, FontSystem, Fonts};

    #[test]
    fn symbol_size_reference_measures_ink_across_fonts_and_writing_axes() -> Result<()> {
        let mut system = FontSystem::new();
        let mut checked = 0;
        for family in [
            "DejaVu Sans",
            "DejaVu Serif",
            "Liberation Mono",
            "Arial",
            "Times New Roman",
        ] {
            let Ok(font) = system.query_family(family) else {
                continue;
            };
            for mode in [WritingMode::Horizontal, WritingMode::VerticalRl] {
                let template = TextLayout::new(&font).with_writing_mode(mode);
                for text in ["!", "?!", "..."] {
                    for size in [18.0, 18.5, 36.25, 72.125] {
                        let original = template.clone().with_font_size(size).run(text)?;
                        let (left, top, right, bottom) =
                            template.ink_bounds(size, &original.lines).unwrap();
                        let area = (right - left) * (bottom - top);
                        let reference =
                            FontSizeReference::InkArea(area).resolve(&template, text)?;
                        assert!(
                            (reference - size).abs() < 0.01,
                            "{family} {mode:?} {text:?}: {reference} != {size}"
                        );
                        let doubled =
                            FontSizeReference::InkArea(area * 4.0).resolve(&template, text)?;
                        assert!((doubled - size * 2.0).abs() < 0.02);
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked >= 18, "no supported system font was exercised");
        Ok(())
    }

    #[tokio::test]
    async fn paragraph_fitting_reserves_the_painted_outline() -> Result<()> {
        let font = FontSystem::new().first_font().unwrap();
        let family = font.family_name().to_owned();
        let fonts = Fonts::new();
        fonts
            .prepare(&[FontRequest {
                family: family.clone(),
                weight: None,
                style: None,
            }])
            .await?;
        for mode in [WritingMode::Horizontal, WritingMode::VerticalRl] {
            let probe = TextLayout::new(&font)
                .with_writing_mode(mode)
                .with_font_size(32.0)
                .run("Lettering")?;
            let (width, height) = if mode.is_vertical() {
                (probe.width + 80.0, probe.height + 8.0)
            } else {
                (probe.width + 8.0, probe.height + 80.0)
            };
            for bubble in [false, true] {
                let descriptor = TextNodeDescriptor {
                    entity: EntityId::new(),
                    text: "Lettering".to_owned(),
                    text_role: None,
                    typography_origin: None,
                    language: None,
                    width,
                    height,
                    balloon_contour: bubble
                        .then(|| vec![(0.0, 0.0), (width, 0.0), (width, height), (0.0, height)]),
                    flow_contours: Vec::new(),
                    preferred_center: Some((width * 0.5, height * 0.5)),
                    free_text_candidates: Vec::new(),
                    artwork: None,
                    preferred_line_count: None,
                    automatic_free_text: false,
                    preferred_font: Some(family.clone()),
                    font_families: vec![family.clone()],
                    font_weight: None,
                    font_style: None,
                    font_size: None,
                    reviewed_font_size: None,
                    source_font_size: Some(32.0),
                    font_size_reference: Some(FontSizeReference::CrossLine(32.0)),
                    minimum_font_size: 12.0,
                    auto_fit: true,
                    alignment: TextAlign::Center,
                    writing_mode: mode,
                    foreground_color: [0, 0, 0, 255],
                    stroke: Some(StrokeOptions {
                        color: [255; 4],
                        width: StrokeWidth::Absolute(10.0),
                    }),
                    allow_discretionary_hyphenation: Some(false),
                    placement_offset: (0.0, 0.0),
                    line_height: 1.2,
                    letter_spacing: 0.0,
                    word_spacing: 0.0,
                    text_inset: [0.0; 4],
                    point_text: false,
                };
                for stroke in [
                    StrokeWidth::Absolute(10.0),
                    StrokeWidth::FontRelative(0.12),
                    StrokeWidth::Generated {
                        width_px: 20.0,
                        reference_font_size: None,
                    },
                ] {
                    let mut outlined = descriptor.clone();
                    outlined.stroke.as_mut().unwrap().width = stroke;
                    let rendered = TextRenderer::new().render_descriptor(
                        &outlined,
                        &fonts,
                        &HashMap::new(),
                    )?;
                    assert!(
                        rendered.diagnostics.is_empty(),
                        "{mode:?} bubble={bubble} {stroke:?}: {:?}",
                        rendered.diagnostics
                    );
                    let bounds = rendered.local_bounds;
                    assert!(bounds.x >= 0.0 && bounds.y >= 0.0, "{bounds:?}");
                    assert!(
                        bounds.x + bounds.width <= width && bounds.y + bounds.height <= height,
                        "{bounds:?}"
                    );
                }

                let mut unoutlined = descriptor.clone();
                unoutlined.stroke = None;
                let plain =
                    TextRenderer::new().render_descriptor(&unoutlined, &fonts, &HashMap::new())?;
                unoutlined.stroke = Some(StrokeOptions {
                    color: [255, 255, 255, 0],
                    width: StrokeWidth::Absolute(10.0),
                });
                let transparent =
                    TextRenderer::new().render_descriptor(&unoutlined, &fonts, &HashMap::new())?;
                assert_eq!(transparent.metadata.font_size, plain.metadata.font_size);
                assert_eq!(transparent.local_bounds, plain.local_bounds);
                assert_eq!(transparent.metadata.line_ranges, plain.metadata.line_ranges);
                assert!(transparent.diagnostics.is_empty());

                let mut fixed = descriptor.clone();
                fixed.auto_fit = false;
                fixed.font_size = Some(32.0);
                let authored =
                    TextRenderer::new().render_descriptor(&fixed, &fonts, &HashMap::new())?;
                assert_eq!(authored.metadata.font_size, 32.0);
                assert!(
                    authored.diagnostics.iter().any(|diagnostic| matches!(
                        diagnostic,
                        RenderDiagnostic::TextOverflow { .. }
                    ))
                );
                fixed.auto_fit = true;
                fixed.font_size = None;
                fixed.reviewed_font_size = Some(32.0);
                let reviewed =
                    TextRenderer::new().render_descriptor(&fixed, &fonts, &HashMap::new())?;
                assert_eq!(reviewed.metadata.font_size, 32.0);
                assert!(
                    reviewed.diagnostics.iter().any(|diagnostic| matches!(
                        diagnostic,
                        RenderDiagnostic::TextOverflow { .. }
                    ))
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn automatic_paragraphs_use_frame_capacity_and_fixed_sizes_stay_exact() {
        let descriptor = TextNodeDescriptor {
            entity: EntityId::new(),
            text: "Hi".to_owned(),
            text_role: None,
            typography_origin: None,
            language: None,
            width: 240.0,
            height: 120.0,
            balloon_contour: Some(vec![(0.0, 0.0), (240.0, 0.0), (240.0, 120.0), (0.0, 120.0)]),
            flow_contours: Vec::new(),
            preferred_center: None,
            free_text_candidates: Vec::new(),
            artwork: None,
            preferred_line_count: None,
            automatic_free_text: false,
            preferred_font: Some("Arial".to_owned()),
            font_families: vec!["Arial".to_owned()],
            font_weight: None,
            font_style: None,
            font_size: None,
            reviewed_font_size: None,
            source_font_size: Some(6.0),
            font_size_reference: None,
            minimum_font_size: crate::MINIMUM_READABLE_FONT_SIZE,
            auto_fit: true,
            alignment: TextAlign::Center,
            writing_mode: WritingMode::Horizontal,
            foreground_color: [0, 0, 0, 255],
            stroke: None,
            allow_discretionary_hyphenation: None,
            placement_offset: (0.0, 0.0),
            line_height: 1.2,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            text_inset: [0.0; 4],
            point_text: false,
        };
        let bounds = LayoutBox {
            x: 0.0,
            y: 0.0,
            width: 240.0,
            height: 120.0,
        };

        assert_eq!(automatic_maximum(&descriptor, bounds), 240.0);
        assert_eq!(
            font_size_limits(&descriptor, bounds),
            (crate::MINIMUM_READABLE_FONT_SIZE, 240.0)
        );

        let mut large_source = descriptor.clone();
        large_source.source_font_size = Some(30.0);
        assert_eq!(readability_minimum(&large_source), 15.0);
        assert_eq!(font_size_limits(&large_source, bounds), (12.0, 240.0));

        let mut generated_free_text = large_source.clone();
        generated_free_text.balloon_contour = None;
        assert_eq!(
            font_size_limits(&generated_free_text, bounds),
            (12.0, 240.0)
        );

        let mut authored_maximum = descriptor.clone();
        authored_maximum.font_size = Some(30.0);
        authored_maximum.source_font_size = None;
        assert_eq!(font_size_limits(&authored_maximum, bounds), (12.0, 30.0));

        authored_maximum.font_size = Some(6.0);
        assert_eq!(font_size_limits(&authored_maximum, bounds), (6.0, 6.0));

        let mut reviewed = descriptor.clone();
        reviewed.reviewed_font_size = Some(22.5);
        assert_eq!(font_size_limits(&reviewed, bounds), (22.5, 22.5));

        let mut free_paragraph = descriptor.clone();
        free_paragraph.font_size = None;
        assert_eq!(
            font_size_limits(&free_paragraph, bounds),
            (crate::MINIMUM_READABLE_FONT_SIZE, 240.0)
        );

        let mut vertical_paragraph = free_paragraph.clone();
        vertical_paragraph.writing_mode = WritingMode::VerticalRl;
        assert_eq!(
            font_size_limits(&vertical_paragraph, bounds),
            (crate::MINIMUM_READABLE_FONT_SIZE, 120.0)
        );

        let family = FontSystem::new()
            .first_font()
            .unwrap()
            .family_name()
            .to_owned();
        let fonts = Fonts::new();
        fonts
            .prepare(&[FontRequest {
                family: family.clone(),
                weight: None,
                style: None,
            }])
            .await
            .unwrap();
        let mut render_descriptor = descriptor.clone();
        render_descriptor.preferred_font = Some(family.clone());
        render_descriptor.font_families = vec![family];
        let rendered = TextRenderer::new()
            .render_descriptor(&render_descriptor, &fonts, &HashMap::new())
            .unwrap();
        assert!(rendered.metadata.font_size > descriptor.minimum_font_size);
        assert!(rendered.diagnostics.is_empty());

        let baseline_size = rendered.metadata.font_size;
        let mut symbol = render_descriptor.clone();
        symbol.text = "!".to_owned();
        symbol.source_font_size = Some(5.0);
        symbol.font_size_reference = Some(FontSizeReference::InkArea(400.0));
        let symbol_rendered = TextRenderer::new()
            .render_descriptor(&symbol, &fonts, &HashMap::new())
            .unwrap();
        assert!(symbol_rendered.metadata.font_size > crate::MINIMUM_READABLE_FONT_SIZE);
        assert!(symbol_rendered.diagnostics.is_empty());
        symbol.source_font_size = Some(10.0);
        let same_ink = TextRenderer::new()
            .render_descriptor(&symbol, &fonts, &HashMap::new())
            .unwrap();
        assert_eq!(
            same_ink.metadata.font_size,
            symbol_rendered.metadata.font_size
        );
        symbol.reviewed_font_size = Some(18.5);
        assert_eq!(
            TextRenderer::new()
                .render_descriptor(&symbol, &fonts, &HashMap::new())
                .unwrap()
                .metadata
                .font_size,
            18.5
        );
        symbol.reviewed_font_size = None;
        symbol.auto_fit = false;
        symbol.font_size = Some(21.0);
        assert_eq!(
            TextRenderer::new()
                .render_descriptor(&symbol, &fonts, &HashMap::new())
                .unwrap()
                .metadata
                .font_size,
            21.0
        );

        let mut source_guided = render_descriptor.clone();
        source_guided.font_size_reference = Some(FontSizeReference::CrossLine(30.0));
        let guided = TextRenderer::new()
            .render_descriptor(&source_guided, &fonts, &HashMap::new())
            .unwrap();
        assert!(baseline_size - guided.metadata.font_size > 4.0);
        assert!((guided.metadata.font_size - 30.0).abs() < 1.0);
        assert!(guided.diagnostics.is_empty());

        let mut flow_guided = source_guided.clone();
        flow_guided.flow_contours =
            vec![vec![(0.0, 0.0), (240.0, 0.0), (240.0, 120.0), (0.0, 120.0)]];
        flow_guided.preferred_center = Some((120.0, 60.0));
        let guided_flow = TextRenderer::new()
            .render_descriptor(&flow_guided, &fonts, &HashMap::new())
            .unwrap();
        assert_eq!(guided_flow.metadata.font_size, guided.metadata.font_size);

        source_guided.reviewed_font_size = Some(22.5);
        let reviewed = TextRenderer::new()
            .render_descriptor(&source_guided, &fonts, &HashMap::new())
            .unwrap();
        assert_eq!(reviewed.metadata.font_size, 22.5);
        source_guided.reviewed_font_size = None;
        source_guided.auto_fit = false;
        source_guided.font_size = Some(40.0);
        let authored = TextRenderer::new()
            .render_descriptor(&source_guided, &fonts, &HashMap::new())
            .unwrap();
        assert_eq!(authored.metadata.font_size, 40.0);

        let mut expanded_translation = render_descriptor.clone();
        expanded_translation.text =
            "This translated free text must remain inside its detected source region.".to_owned();
        expanded_translation.width = 120.0;
        expanded_translation.height = 240.0;
        expanded_translation.balloon_contour = None;
        expanded_translation.source_font_size = Some(100.0);
        let rendered = TextRenderer::new()
            .render_descriptor(&expanded_translation, &fonts, &HashMap::new())
            .unwrap();
        assert!(rendered.metadata.font_size < readability_minimum(&expanded_translation));
        assert!(rendered.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            RenderDiagnostic::TextBelowReadableSize {
                minimum_font_size,
                ..
            } if (*minimum_font_size - 50.0).abs() < f32::EPSILON
        )));
        assert!(
            !rendered
                .diagnostics
                .iter()
                .any(|diagnostic| matches!(diagnostic, RenderDiagnostic::TextOverflow { .. }))
        );

        let mut point_text = free_paragraph;
        point_text.point_text = true;
        assert_eq!(automatic_maximum(&point_text, bounds), 24.0);

        let mut explicit = descriptor;
        explicit.auto_fit = false;
        explicit.font_size = Some(6.0);
        explicit.source_font_size = None;
        assert_eq!(font_size_limits(&explicit, bounds), (6.0, 6.0));
    }

    #[tokio::test]
    async fn free_text_composition_compares_all_footprints_at_one_contextual_scale() {
        let family = FontSystem::new()
            .first_font()
            .unwrap()
            .family_name()
            .to_owned();
        let fonts = Fonts::new();
        fonts
            .prepare(&[FontRequest {
                family: family.clone(),
                weight: None,
                style: None,
            }])
            .await
            .unwrap();
        let candidate = |bounds: LayoutBox, maximum_visual_area, translation_visual_area_scale| {
            FreeTextCandidate {
                preferred_center: (
                    bounds.x + bounds.width * 0.5,
                    bounds.y + bounds.height * 0.5,
                ),
                bounds,
                maximum_visual_area,
                translation_visual_area_scale,
            }
        };
        let base = candidate(
            LayoutBox {
                x: 0.0,
                y: 0.0,
                width: 80.0,
                height: 40.0,
            },
            80.0 * 40.0,
            1.0,
        );
        let expanded = candidate(
            LayoutBox {
                x: -40.0,
                y: -20.0,
                width: 160.0,
                height: 80.0,
            },
            160.0 * 80.0,
            4.0,
        );
        let mut descriptor = TextNodeDescriptor {
            entity: EntityId::new(),
            text: "Hi".to_owned(),
            text_role: None,
            typography_origin: None,
            language: None,
            width: 80.0,
            height: 40.0,
            balloon_contour: None,
            flow_contours: Vec::new(),
            preferred_center: None,
            free_text_candidates: vec![base],
            artwork: None,
            preferred_line_count: None,
            automatic_free_text: true,
            preferred_font: Some(family.clone()),
            font_families: vec![family],
            font_weight: None,
            font_style: None,
            font_size: None,
            reviewed_font_size: None,
            source_font_size: None,
            font_size_reference: None,
            minimum_font_size: crate::MINIMUM_READABLE_FONT_SIZE,
            auto_fit: true,
            alignment: TextAlign::Center,
            writing_mode: WritingMode::Horizontal,
            foreground_color: [0, 0, 0, 255],
            stroke: None,
            allow_discretionary_hyphenation: None,
            placement_offset: (0.0, 0.0),
            line_height: 1.2,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            text_inset: [0.0; 4],
            point_text: false,
        };
        let renderer = TextRenderer::new();
        let base_only = renderer
            .render_descriptor(&descriptor, &fonts, &HashMap::new())
            .unwrap();
        descriptor.free_text_candidates.push(expanded);
        let tiered = renderer
            .render_descriptor(&descriptor, &fonts, &HashMap::new())
            .unwrap();
        descriptor.free_text_candidates = vec![expanded];
        let expanded_only = renderer
            .render_descriptor(&descriptor, &fonts, &HashMap::new())
            .unwrap();

        assert_eq!(tiered.metadata.font_size, base_only.metadata.font_size);
        assert!(expanded_only.metadata.font_size > tiered.metadata.font_size);

        descriptor.font_size_reference = Some(FontSizeReference::CrossLine(24.0));
        descriptor.text = "Moderately longer target".to_owned();
        descriptor.free_text_candidates = vec![base];
        let emergency_base_only = renderer
            .render_descriptor(&descriptor, &fonts, &HashMap::new())
            .unwrap();
        assert!(emergency_base_only.metadata.font_size < 24.0);
        descriptor.free_text_candidates.push(expanded);
        let emergency_expanded = renderer
            .render_descriptor(&descriptor, &fonts, &HashMap::new())
            .unwrap();
        assert!(emergency_expanded.metadata.font_size > emergency_base_only.metadata.font_size);

        descriptor.text = "This target cannot fit inside the tiny source footprint.".to_owned();
        descriptor.free_text_candidates = vec![
            candidate(
                LayoutBox {
                    x: 0.0,
                    y: 0.0,
                    width: 20.0,
                    height: 10.0,
                },
                20.0 * 10.0,
                1.0,
            ),
            expanded,
        ];
        let fallback = renderer
            .render_descriptor(&descriptor, &fonts, &HashMap::new())
            .unwrap();
        descriptor.free_text_candidates = vec![expanded];
        let expanded_only = renderer
            .render_descriptor(&descriptor, &fonts, &HashMap::new())
            .unwrap();

        assert_eq!(
            fallback.metadata.font_size,
            expanded_only.metadata.font_size
        );
        assert!(
            !fallback
                .diagnostics
                .iter()
                .any(|diagnostic| matches!(diagnostic, RenderDiagnostic::TextOverflow { .. }))
        );

        let font = FontSystem::new().query_family(descriptor.preferred_font.as_deref().unwrap());
        let font = font.expect("prepared test font is available");
        descriptor.text = "Hi".to_owned();
        let layout = TextLayout::new(&font)
            .with_font_size(24.0)
            .run(&descriptor.text)
            .unwrap();
        let wide = candidate(
            LayoutBox {
                x: -60.0,
                y: -20.0,
                width: 120.0,
                height: 40.0,
            },
            4800.0,
            1.0,
        );
        let tall = candidate(
            LayoutBox {
                x: -30.0,
                y: -40.0,
                width: 60.0,
                height: 80.0,
            },
            4800.0,
            1.0,
        );
        assert!(free_text_layout_fits(&descriptor, &wide, &layout));
        assert!(free_text_layout_fits(&descriptor, &tall, &layout));
        assert_eq!(
            free_text_composition_quality(&descriptor, &wide, &layout, 24.0, None).cost,
            free_text_composition_quality(&descriptor, &tall, &layout, 24.0, None).cost,
            "identical painted text must not be penalized for the unused rectangle aspect ratio"
        );
    }

    #[test]
    fn generated_strokes_scale_and_cap_while_absolute_strokes_stay_fixed() {
        let generated = StrokeOptions {
            color: [255; 4],
            width: StrokeWidth::Generated {
                width_px: 2.0,
                reference_font_size: Some(40.0),
            },
        };
        let generated_without_reference = StrokeOptions {
            width: StrokeWidth::Generated {
                width_px: 10.0,
                reference_font_size: None,
            },
            ..generated
        };
        let absolute = StrokeOptions {
            width: StrokeWidth::Absolute(10.0),
            ..generated
        };
        let relative = StrokeOptions {
            width: StrokeWidth::FontRelative(0.08),
            ..generated
        };

        assert_eq!(generated.for_font_size(20.0).width_px, 1.0);
        assert!((generated_without_reference.for_font_size(12.0).width_px - 1.44).abs() < 0.001);
        assert_eq!(absolute.for_font_size(12.0).width_px, 10.0);
        assert!((relative.for_font_size(25.0).width_px - 2.0).abs() < 0.001);
    }

    #[test]
    fn rendered_text_survives_prepared_packet_round_trip() {
        let font = FontSystem::new().first_font().unwrap();
        let layout = TextLayout::new(&font)
            .with_font_size(24.0)
            .run("Koharu")
            .unwrap();
        let mut scene = PreparedScene::default();
        let mut resources = Vec::new();
        TextRenderer::new().render(
            &mut scene,
            &mut resources,
            &layout,
            WritingMode::Horizontal,
            &TextRenderOptions::default(),
            Affine::IDENTITY,
        );
        assert_eq!(resources.len(), 1);
        assert!(matches!(
            &resources[0],
            PreparedResource::Font { bytes, .. } if !bytes.is_empty()
        ));

        let layer = LayerId::from_bytes([2; 16]);
        let bundle = PreparedFrameBundle {
            frame: PreparedFrame {
                revision: Revision::new(7),
                page: LayerId::from_bytes([1; 16]),
                width: 160,
                height: 48,
                origin: (0, 0),
                normalization: Affine::IDENTITY.as_coeffs(),
                layers: vec![PreparedLayer {
                    id: layer,
                    geometry: vec![
                        Point { x: 0.0, y: 0.0 },
                        Point { x: 160.0, y: 0.0 },
                        Point { x: 160.0, y: 48.0 },
                        Point { x: 0.0, y: 48.0 },
                    ],
                    bounds: Bounds {
                        x: 0.0,
                        y: 0.0,
                        width: 160.0,
                        height: 48.0,
                    },
                    local_bounds: Bounds {
                        x: 0.0,
                        y: 0.0,
                        width: 160.0,
                        height: 48.0,
                    },
                    presentation: Presentation {
                        visible: true,
                        opacity: 1.0,
                    },
                    kind: PreparedLayerKind::Text,
                    placement: Affine::IDENTITY.as_coeffs(),
                    content: PreparedContent::Vector(scene),
                    element_frame: None,
                }],
            },
            resources,
        };

        let encoded = bundle.manifest().unwrap().encode().unwrap();
        let manifest = PreparedFrameManifest::decode(&encoded).unwrap();
        let mut resources = PreparedResourceStore::default();
        for reference in manifest.required_resources() {
            let encoded = bundle
                .resource_packet(reference.id)
                .unwrap()
                .encode()
                .unwrap();
            resources.insert(PreparedResourcePacket::decode(&encoded).unwrap());
        }
        let compiled = manifest.compile(&resources).unwrap();
        assert_eq!(compiled.revision(), Revision::new(7));
        assert_eq!(compiled.layers()[0].id(), layer);
        assert!(matches!(
            compiled.composition_commands(1).as_slice(),
            [CompositionCommand::Vector(_)]
        ));
    }
}
