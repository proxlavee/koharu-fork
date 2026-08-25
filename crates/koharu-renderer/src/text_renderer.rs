//! Text layout and portable glyph recording.

use std::sync::Arc;

use anyhow::Result;
use koharu_rasterizer::{
    PreparedGlyph, PreparedGlyphRun, PreparedResource, PreparedScene, PreparedSceneCommand,
    ResourceId,
};
use vello::kurbo::Affine;

use crate::{
    Error, FontStyle, HyphenationPolicy, LayoutRun, RenderBounds, RenderDiagnostic,
    Result as RenderResult, TextAlign, TextLayout, WritingMode,
    bubble::LayoutBox,
    fonts::{Fonts, font_key},
    free_text::FreeTextCandidate,
    layout::{
        fragmentation_quality_font_reduction, has_internal_natural_pause, quality_font_reduction,
        whole_word_quality_font_reduction,
    },
    script::is_chinese_or_japanese_text,
};

// Detected outlines are measured at the source font size, while translated
// text may auto-fit smaller. Preserve that ratio but bound unusually broad
// detected bands; explicitly authored widths remain absolute.
const MAX_GENERATED_STROKE_FONT_RATIO: f32 = 0.12;
const FREE_TEXT_PAUSE_FONT_REDUCTION_RATIO: f32 = 0.25;
const FREE_TEXT_PAUSE_FONT_REDUCTION_MAX: f32 = 6.0;
const FREE_TEXT_LOCALITY_FONT_REDUCTION_MAX: f32 = 1.0;

fn free_text_semantic_font_reduction(text: &str, font_size: f32) -> f32 {
    let ordinary = quality_font_reduction(font_size);
    if has_internal_natural_pause(text) {
        ordinary
            .max(font_size * FREE_TEXT_PAUSE_FONT_REDUCTION_RATIO)
            .min(FREE_TEXT_PAUSE_FONT_REDUCTION_MAX)
    } else {
        ordinary
    }
}

fn free_text_quality_font_reduction(
    text: &str,
    font_size: f32,
    discretionary_hyphens: usize,
) -> f32 {
    free_text_semantic_font_reduction(text, font_size).max(fragmentation_quality_font_reduction(
        text,
        font_size,
        discretionary_hyphens,
    ))
}

fn free_text_layout_quality_font_reduction(
    text: &str,
    largest_font_size: f32,
    layout: &LayoutRun<'_>,
) -> f32 {
    free_text_semantic_font_reduction(text, largest_font_size).max(
        whole_word_quality_font_reduction(text, largest_font_size, layout),
    )
}

fn fragmentation_trade_is_readable(
    initial_font_size: f32,
    minimum_font_size: f32,
    initial_hyphens: usize,
    candidate_font_size: f32,
    candidate_hyphens: usize,
) -> bool {
    // The configured minimum is an emergency fit boundary, not an aesthetic
    // budget for removing fragmentation. Mirror the comic-layout invariant so
    // generated free text cannot buy a cleaner word by becoming unreadably small.
    let first_visible_size_above_minimum = minimum_font_size.floor() + 1.0;
    candidate_hyphens >= initial_hyphens
        || candidate_font_size + f32::EPSILON >= first_visible_size_above_minimum
        || candidate_font_size + f32::EPSILON
            >= (initial_font_size - quality_font_reduction(initial_font_size))
                .max(minimum_font_size)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TextNodeDescriptor {
    pub(crate) entity: koharu_scene::EntityId,
    pub(crate) text: String,
    pub(crate) language: Option<koharu_scene::LanguageTag>,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) balloon_contour: Option<Vec<(f32, f32)>>,
    pub(crate) flow_contour: Option<Vec<(f32, f32)>>,
    pub(crate) preferred_block_center: Option<f32>,
    pub(crate) free_text_candidates: Vec<FreeTextCandidate>,
    pub(crate) automatic_free_text: bool,
    pub(crate) preferred_font: Option<String>,
    pub(crate) font_families: Vec<String>,
    pub(crate) font_weight: Option<u16>,
    pub(crate) font_style: Option<FontStyle>,
    pub(crate) font_size: Option<f32>,
    pub(crate) source_font_size: Option<f32>,
    pub(crate) minimum_font_size: f32,
    pub(crate) auto_fit: bool,
    pub(crate) alignment: TextAlign,
    pub(crate) writing_mode: WritingMode,
    pub(crate) foreground_color: [u8; 4],
    pub(crate) stroke: Option<StrokeOptions>,
    pub(crate) line_height: f32,
    pub(crate) letter_spacing: f32,
    pub(crate) word_spacing: f32,
    pub(crate) text_inset: [f32; 4],
    pub(crate) point_text: bool,
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
    fn for_font_size(self, font_size: f32) -> ResolvedStrokeOptions {
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
            width_px,
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
            template = template.with_hyphenation_language_tag(language.as_str());
        }
        if (is_bubble_text || descriptor.automatic_free_text)
            && descriptor.writing_mode == WritingMode::Horizontal
        {
            template = template.with_hyphenation_policy(HyphenationPolicy::LastResort);
        }
        if descriptor.automatic_free_text && descriptor.writing_mode == WritingMode::Horizontal {
            template = template.with_natural_line_breaks();
        }

        if let Some(contour) = &descriptor.balloon_contour {
            let [top, _, _, left] = descriptor.text_inset;
            let contour = contour.iter().map(|&(x, y)| (x - left, y - top)).collect();
            if let Some(flow_contour) = &descriptor.flow_contour {
                template = template.with_comic_balloon_constraints(
                    ordinary_bounds.width,
                    ordinary_bounds.height,
                    vec![
                        contour,
                        flow_contour
                            .iter()
                            .map(|&(x, y)| (x - left, y - top))
                            .collect(),
                    ],
                    descriptor.text_inset.into_iter().fold(0.0, f32::max),
                );
            } else {
                template = template.with_comic_balloon(
                    ordinary_bounds.width,
                    ordinary_bounds.height,
                    contour,
                    descriptor.text_inset.into_iter().fold(0.0, f32::max),
                );
            }
            if let Some(center) = descriptor.preferred_block_center {
                let block_inset = if descriptor.writing_mode.is_vertical() {
                    left
                } else {
                    top
                };
                template = template.with_comic_preferred_block_center(center - block_inset);
            }
        }
        let selected = if !descriptor.free_text_candidates.is_empty()
            && descriptor.auto_fit
            && !descriptor.point_text
            && !is_bubble_text
        {
            self.select_free_text_layout(descriptor, &template)?
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
                layout: self.layout(&builder, &descriptor.text).map_err(|source| {
                    Error::Layout {
                        entity: descriptor.entity,
                        source,
                    }
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
            },
            diagnostics,
        })
    }

    fn select_free_text_layout<'a>(
        &self,
        descriptor: &TextNodeDescriptor,
        template: &TextLayout<'a>,
    ) -> RenderResult<SelectedTextLayout<'a>> {
        let mut measured = Vec::with_capacity(descriptor.free_text_candidates.len());
        for candidate in &descriptor.free_text_candidates {
            if candidate.bounds.width <= 0.0
                || candidate.bounds.height <= 0.0
                || !candidate.maximum_visual_area.is_finite()
                || candidate.maximum_visual_area <= 0.0
            {
                continue;
            }
            let (minimum, maximum) = font_size_limits(descriptor, candidate.bounds);
            let builder = template.clone();
            let layout = match builder
                .run_largest_fitting_with_bounds(
                    &descriptor.text,
                    minimum,
                    maximum,
                    |font_size| free_text_content_bounds(descriptor, candidate, font_size),
                    |layout| free_text_layout_fits(descriptor, candidate, layout),
                )
                .map_err(|source| Error::Layout {
                    entity: descriptor.entity,
                    source,
                })? {
                Some(layout) => layout,
                None => {
                    let (width, height) = free_text_content_bounds(descriptor, candidate, minimum);
                    self.layout(
                        &builder
                            .clone()
                            .with_font_size(minimum)
                            .with_max_width(width.max(0.5))
                            .with_max_height(height.max(0.5)),
                        &descriptor.text,
                    )
                    .map_err(|source| Error::Layout {
                        entity: descriptor.entity,
                        source,
                    })?
                }
            };
            let quality_floor = (layout.font_size
                - free_text_layout_quality_font_reduction(
                    &descriptor.text,
                    layout.font_size,
                    &layout,
                ))
            .max(minimum);
            let layout = self.prefer_free_text_quality(
                descriptor,
                candidate,
                &builder,
                layout,
                minimum,
                quality_floor,
            )?;
            let fits = free_text_layout_fits(descriptor, candidate, &layout);
            let hyphens = layout.discretionary_hyphen_count(&descriptor.text);
            let natural_pauses = layout.natural_pause_break_count(&descriptor.text);
            let weak_breaks = layout.weak_line_break_count(&descriptor.text);
            let within_quality_window = layout.font_size + f32::EPSILON
                >= maximum - free_text_quality_font_reduction(&descriptor.text, maximum, hyphens);
            measured.push(MeasuredFreeTextLayout {
                layout,
                candidate: *candidate,
                fits,
                hyphens,
                natural_pauses,
                weak_breaks,
                quality_floor,
            });
            if candidate.source_distance <= f32::EPSILON
                && fits
                && hyphens == 0
                && within_quality_window
                && (weak_breaks == 0 || !has_internal_natural_pause(&descriptor.text))
            {
                let selected = measured.pop().expect("the source candidate was recorded");
                return Ok(selected.into_selected());
            }
        }
        if measured.is_empty() {
            return Err(Error::invalid(format!(
                "free-text candidate set is invalid for entity {}",
                descriptor.entity
            )));
        }

        let largest_candidate = measured
            .iter()
            .filter(|candidate| candidate.fits)
            .max_by(|left, right| left.layout.font_size.total_cmp(&right.layout.font_size));
        let selected_index = if let Some(largest_candidate) = largest_candidate {
            let largest = largest_candidate.layout.font_size;
            let ordinary_quality_floor =
                largest - free_text_semantic_font_reduction(&descriptor.text, largest);
            let extended_quality_floor = (largest
                - free_text_layout_quality_font_reduction(
                    &descriptor.text,
                    largest,
                    &largest_candidate.layout,
                ))
            .max(largest_candidate.quality_floor);
            let ordinary_minimum_hyphens = measured
                .iter()
                .filter(|candidate| {
                    candidate.fits
                        && candidate.layout.font_size + f32::EPSILON >= ordinary_quality_floor
                })
                .map(|candidate| candidate.hyphens)
                .min()
                .expect("the largest fitted candidate is ordinarily quality-eligible");
            let extended_minimum_hyphens = measured
                .iter()
                .filter(|candidate| {
                    candidate.fits
                        && candidate.layout.font_size + f32::EPSILON >= extended_quality_floor
                })
                .map(|candidate| candidate.hyphens)
                .min()
                .expect("the largest fitted candidate is extended-quality-eligible");
            let quality_floor = if extended_minimum_hyphens < ordinary_minimum_hyphens {
                extended_quality_floor
            } else {
                ordinary_quality_floor
            };
            let minimum_hyphens = measured
                .iter()
                .filter(|candidate| {
                    candidate.fits && candidate.layout.font_size + f32::EPSILON >= quality_floor
                })
                .map(|candidate| candidate.hyphens)
                .min()
                .expect("the largest fitted candidate is quality-eligible");
            let mut eligible = measured
                .iter()
                .enumerate()
                .filter(|(_, candidate)| {
                    candidate.fits
                        && candidate.layout.font_size + f32::EPSILON >= quality_floor
                        && candidate.hyphens == minimum_hyphens
                })
                .collect::<Vec<_>>();
            if has_internal_natural_pause(&descriptor.text) {
                let maximum_natural_pauses = eligible
                    .iter()
                    .map(|(_, candidate)| candidate.natural_pauses)
                    .max()
                    .expect("a fitted free-text quality candidate exists");
                eligible
                    .retain(|(_, candidate)| candidate.natural_pauses == maximum_natural_pauses);
                let minimum_weak_breaks = eligible
                    .iter()
                    .map(|(_, candidate)| candidate.weak_breaks)
                    .min()
                    .expect("a punctuation-aware free-text candidate exists");
                eligible.retain(|(_, candidate)| candidate.weak_breaks == minimum_weak_breaks);
            }
            let largest_eligible_font_size = eligible
                .iter()
                .map(|(_, candidate)| candidate.layout.font_size)
                .max_by(f32::total_cmp)
                .expect("a fitted semantic-quality candidate exists");
            let locality_floor = largest_eligible_font_size - FREE_TEXT_LOCALITY_FONT_REDUCTION_MAX;
            eligible.retain(|(_, candidate)| {
                candidate.layout.font_size + f32::EPSILON >= locality_floor
            });
            eligible
                .into_iter()
                .min_by(|(_, left), (_, right)| {
                    left.candidate
                        .source_distance
                        .total_cmp(&right.candidate.source_distance)
                        .then_with(|| right.layout.font_size.total_cmp(&left.layout.font_size))
                })
                .map(|(index, _)| index)
                .expect("a fitted quality candidate exists")
        } else {
            0
        };
        Ok(measured.swap_remove(selected_index).into_selected())
    }

    fn prefer_free_text_quality<'a>(
        &self,
        descriptor: &TextNodeDescriptor,
        candidate: &FreeTextCandidate,
        template: &TextLayout<'a>,
        mut selected: LayoutRun<'a>,
        minimum: f32,
        quality_floor: f32,
    ) -> RenderResult<LayoutRun<'a>> {
        let initial_font_size = selected.font_size;
        let mut selected_hyphens = selected.discretionary_hyphen_count(&descriptor.text);
        let initial_hyphens = selected_hyphens;
        let mut selected_pauses = selected.natural_pause_break_count(&descriptor.text);
        let mut selected_weak_breaks = selected.weak_line_break_count(&descriptor.text);
        let initial_pauses = selected_pauses;
        let initial_weak_breaks = selected_weak_breaks;
        if selected_hyphens == 0
            && (selected_weak_breaks == 0 || !has_internal_natural_pause(&descriptor.text))
        {
            return Ok(selected);
        }

        let mut next_size = selected.font_size.floor();
        if (next_size - selected.font_size).abs() <= f32::EPSILON {
            next_size -= 1.0;
        }
        let mut last_checked = selected.font_size;
        while next_size + f32::EPSILON >= quality_floor.ceil() {
            let layout = self.layout_free_text_at(descriptor, candidate, template, next_size)?;
            last_checked = next_size;
            let hyphens = layout.discretionary_hyphen_count(&descriptor.text);
            let pauses = layout.natural_pause_break_count(&descriptor.text);
            let weak_breaks = layout.weak_line_break_count(&descriptor.text);
            let semantic_improvements = initial_weak_breaks.saturating_sub(weak_breaks)
                + pauses.saturating_sub(initial_pauses) * 2;
            let semantic_budget = (semantic_improvements as f32 * 2.0).min(
                free_text_semantic_font_reduction(&descriptor.text, initial_font_size),
            );
            let semantic_trade_is_bounded =
                layout.font_size + f32::EPSILON >= initial_font_size - semantic_budget;
            if free_text_layout_fits(descriptor, candidate, &layout)
                && fragmentation_trade_is_readable(
                    initial_font_size,
                    minimum,
                    initial_hyphens,
                    layout.font_size,
                    hyphens,
                )
                && (hyphens < selected_hyphens
                    || (hyphens == selected_hyphens
                        && semantic_trade_is_bounded
                        && (weak_breaks < selected_weak_breaks
                            || (weak_breaks == selected_weak_breaks && pauses > selected_pauses))))
            {
                selected = layout;
                selected_hyphens = hyphens;
                selected_pauses = pauses;
                selected_weak_breaks = weak_breaks;
                if selected_hyphens == 0 && selected_weak_breaks == 0 {
                    return Ok(selected);
                }
            }
            next_size -= 1.0;
        }
        if (last_checked - quality_floor).abs() > f32::EPSILON {
            let layout =
                self.layout_free_text_at(descriptor, candidate, template, quality_floor)?;
            let hyphens = layout.discretionary_hyphen_count(&descriptor.text);
            let pauses = layout.natural_pause_break_count(&descriptor.text);
            let weak_breaks = layout.weak_line_break_count(&descriptor.text);
            let semantic_improvements = initial_weak_breaks.saturating_sub(weak_breaks)
                + pauses.saturating_sub(initial_pauses) * 2;
            let semantic_budget = (semantic_improvements as f32 * 2.0).min(
                free_text_semantic_font_reduction(&descriptor.text, initial_font_size),
            );
            let semantic_trade_is_bounded =
                layout.font_size + f32::EPSILON >= initial_font_size - semantic_budget;
            if free_text_layout_fits(descriptor, candidate, &layout)
                && fragmentation_trade_is_readable(
                    initial_font_size,
                    minimum,
                    initial_hyphens,
                    layout.font_size,
                    hyphens,
                )
                && (hyphens < selected_hyphens
                    || (hyphens == selected_hyphens
                        && semantic_trade_is_bounded
                        && (weak_breaks < selected_weak_breaks
                            || (weak_breaks == selected_weak_breaks && pauses > selected_pauses))))
            {
                selected = layout;
            }
        }
        Ok(selected)
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
    hyphens: usize,
    natural_pauses: usize,
    weak_breaks: usize,
    quality_floor: f32,
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
    fn natural_pause_extends_but_caps_the_free_text_quality_window() {
        assert_eq!(
            free_text_quality_font_reduction("Plain words", 20.0, 0),
            4.0
        );
        assert_eq!(
            free_text_quality_font_reduction("First... Second", 20.0, 0),
            5.0
        );
        assert_eq!(
            free_text_quality_font_reduction("First~ Second", 40.0, 0),
            6.0
        );
        assert_eq!(
            free_text_quality_font_reduction("Ne demek♡", 32.0, 1),
            32.0 / 3.0
        );
        assert_eq!(
            free_text_quality_font_reduction(
                "One split inside an otherwise much longer caption",
                40.0,
                1,
            ),
            4.0
        );
    }

    #[test]
    fn hard_minimum_does_not_buy_a_large_free_text_fragmentation_trade() {
        assert!(!fragmentation_trade_is_readable(19.0, 12.0, 1, 12.0, 0));
        assert!(!fragmentation_trade_is_readable(19.0, 12.0, 1, 12.75, 0));
        assert!(fragmentation_trade_is_readable(15.0, 12.0, 1, 12.0, 0));
        assert!(fragmentation_trade_is_readable(19.0, 12.0, 1, 13.0, 0));
        assert!(fragmentation_trade_is_readable(19.0, 12.0, 1, 12.0, 1));
    }

    #[tokio::test]
    async fn automatic_paragraphs_use_frame_capacity_and_fixed_sizes_stay_exact() {
        let descriptor = TextNodeDescriptor {
            entity: EntityId::new(),
            text: "Hi".to_owned(),
            language: None,
            width: 240.0,
            height: 120.0,
            balloon_contour: Some(vec![(0.0, 0.0), (240.0, 0.0), (240.0, 120.0), (0.0, 120.0)]),
            flow_contour: None,
            preferred_block_center: None,
            free_text_candidates: Vec::new(),
            automatic_free_text: false,
            preferred_font: Some("Arial".to_owned()),
            font_families: vec!["Arial".to_owned()],
            font_weight: None,
            font_style: None,
            font_size: None,
            source_font_size: Some(6.0),
            minimum_font_size: crate::MINIMUM_READABLE_FONT_SIZE,
            auto_fit: true,
            alignment: TextAlign::Center,
            writing_mode: WritingMode::Horizontal,
            foreground_color: [0, 0, 0, 255],
            stroke: None,
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
            .render_descriptor(&render_descriptor, &fonts)
            .unwrap();
        assert!(rendered.metadata.font_size > descriptor.minimum_font_size);
        assert!(rendered.diagnostics.is_empty());

        let mut expanded_translation = render_descriptor.clone();
        expanded_translation.text =
            "This translated free text must remain inside its detected source region.".to_owned();
        expanded_translation.width = 120.0;
        expanded_translation.height = 240.0;
        expanded_translation.balloon_contour = None;
        expanded_translation.source_font_size = Some(100.0);
        let rendered = TextRenderer::new()
            .render_descriptor(&expanded_translation, &fonts)
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
