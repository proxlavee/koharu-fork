//! Unicode-aware text shaping, line breaking, and layout.

use std::{collections::HashMap, ops::Range};
use unicode_bidi::BidiInfo;

use anyhow::Result;
use harfrust::{Feature, Tag};
use hypher::Lang;
use icu_normalizer::ComposingNormalizerBorrowed;
use icu_properties::{
    CodePointMapData,
    props::{GeneralCategory, GeneralCategoryGroup},
};
use skrifa::{MetadataProvider, instance::Size};

use crate::{
    bubble::{intersected_centroid, polygon_spans},
    fonts::{Font, font_key},
    script::shaping_direction_for_text,
    segment::{HyphenationOptions, LineBreakSuffix, LineBreaker, hyphenation_lang_from_tag},
    shape::{PositionedGlyph, ShapedRun, ShapingOptions, TextShaper, shape_script_runs},
    types::TextAlign,
};

const HYPHENATION_MIN_WORD_LEN: usize = 5;
const COMPACT_HYPHENATION_FRAGMENT_LEN: usize = 3;
const LINE_BREAK_RAGGEDNESS_PENALTY: f32 = 1_000.0;
const LINE_BREAK_HYPHEN_PENALTY: f32 = 2_000.0;
const LINE_BREAK_OVERFLOW_MULTIPLIER: f32 = 10_000.0;
const COMIC_LINE_OVERFLOW_PENALTY: f32 = 1_000_000.0;
const COMIC_MAX_LINES: usize = 64;
const COMIC_BLOCK_SEARCH_MAX_INTERVALS: usize = 32;
const COMIC_BLOCK_REFINEMENT_ITERATIONS: usize = 8;
const COMIC_BLOCK_REFINEMENT_TOLERANCE: f32 = 0.25;
const COMIC_CENTER_ERROR_TOLERANCE_EM: f32 = 0.5;
const COMIC_CENTER_CONTAINER_TOLERANCE_RATIO: f32 = 0.02;
const COMIC_CENTER_RASTER_TOLERANCE: f32 = 1.0;
const COMIC_LINE_RASTER_TOLERANCE: f32 = 0.5;
const COMIC_INLINE_WALL_CLEARANCE_GLYPHS: f32 = 0.5;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HyphenationPolicy {
    /// Do not introduce discretionary hyphenation opportunities.
    Disabled,
    /// Use a discretionary hyphen only when the unhyphenated text overflows.
    LastResort,
    /// Consider discretionary hyphens during normal line optimization.
    #[default]
    Normal,
}

/// Writing mode for text layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritingMode {
    /// Horizontal text, left-to-right, lines flow top-to-bottom.
    #[default]
    Horizontal,
    /// Vertical text, right-to-left columns (traditional CJK).
    VerticalRl,
}

impl WritingMode {
    /// Returns true if the writing mode is vertical.
    pub const fn is_vertical(self) -> bool {
        matches!(self, WritingMode::VerticalRl)
    }
}

/// Glyphs for one line alongside metadata required by the renderer.
#[derive(Debug, Clone, Default)]
pub struct LayoutLine<'a> {
    /// Positioned glyphs in this line.
    pub glyphs: Vec<PositionedGlyph<'a>>,
    /// Range in the original text that this line covers.
    pub range: Range<usize>,
    /// Suffix actually shaped at this discretionary break, outside the source range.
    pub break_suffix: Option<LineBreakSuffix>,
    /// Total advance (width for horizontal, height for vertical) of this line.
    pub advance: f32,
    /// Baseline position for this line (x, y).
    pub baseline: (f32, f32),
}

/// A collection of laid out lines.
#[derive(Debug, Clone)]
pub struct LayoutRun<'a> {
    /// Lines in this layout run.
    pub lines: Vec<LayoutLine<'a>>,
    /// Total width of the layout.
    pub width: f32,
    /// Total height of the layout.
    pub height: f32,
    /// Font size used to generate this layout.
    pub font_size: f32,
    overflowed: bool,
    placement_offset_x: f32,
    placement_offset_y: f32,
}

impl LayoutRun<'_> {
    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub(crate) const fn placement_offset_x(&self) -> f32 {
        self.placement_offset_x
    }

    pub(crate) const fn placement_offset_y(&self) -> f32 {
        self.placement_offset_y
    }

    pub(crate) fn discretionary_hyphen_count(&self) -> usize {
        self.lines
            .iter()
            .filter(|line| line.break_suffix == Some(LineBreakSuffix::Hyphen))
            .count()
    }

    pub(crate) fn composition_cost(&self, text: &str, reference_size: f32) -> f32 {
        let words = alphanumeric_word_count(text).max(1) as f32;
        let hyphens = comic_hyphen_quality(text, self);
        let mut consecutive_hyphens = 0;
        let mut stranded_punctuation = 0;
        let mut adjacent_shape = 0.0;
        let mut automatic_breaks = 0;
        let mut weak_breaks = 0;
        let mut terminal_runt = 0.0;
        for (index, pair) in self.lines.windows(2).enumerate() {
            if explicit_line_break_between(text, pair) {
                continue;
            }
            automatic_breaks += 1;
            let continuation = text[pair[1].range.clone()].trim();
            if !continuation.is_empty()
                && continuation.chars().all(|character| {
                    GeneralCategoryGroup::Punctuation
                        .contains(CodePointMapData::<GeneralCategory>::new().get(character))
                })
            {
                stranded_punctuation += 1;
            }
            if pair[0].break_suffix != Some(LineBreakSuffix::Hyphen)
                && !natural_pause_at(text, pair[0].range.end)
            {
                weak_breaks += 1;
            }
            let longest = pair[0].advance.max(pair[1].advance);
            if longest > 0.0 {
                adjacent_shape += ((pair[0].advance - pair[1].advance) / longest).powi(2);
            }
            let paragraph_end = index + 2 == self.lines.len()
                || explicit_line_break_between(text, &self.lines[index + 1..index + 3]);
            if paragraph_end && pair[0].advance > 0.0 {
                let fullness = pair[1].advance / pair[0].advance;
                terminal_runt += ((0.5 - fullness).max(0.0) / 0.5).powi(2);
            }
            if index > 0
                && pair[0].break_suffix == Some(LineBreakSuffix::Hyphen)
                && self.lines[index - 1].break_suffix == Some(LineBreakSuffix::Hyphen)
            {
                consecutive_hyphens += 1;
            }
        }
        let breaks = automatic_breaks.max(1) as f32;
        let pause_cost = if has_internal_natural_pause(text) {
            weak_breaks as f32 / breaks
        } else {
            0.0
        };
        // An occasional split is a paragraph-level tradeoff. Tiny fragments
        // and runs of broken words are local reading defects: adding intact
        // words elsewhere must not make those defects cheaper.
        font_scale_cost(self.font_size, reference_size)
            + hyphens.total as f32 / words
            + (hyphens.compact + consecutive_hyphens + stranded_punctuation) as f32
            + (adjacent_shape * 0.25 + terminal_runt * 0.5) / breaks
            + pause_cost * 0.25
    }
}

fn explicit_line_break_between(text: &str, pair: &[LayoutLine<'_>]) -> bool {
    text.get(pair[0].range.start..pair[1].range.start)
        .is_some_and(|span| {
            span.chars()
                .any(|ch| matches!(ch, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}'))
        })
}

/// Explicit review shape and assigned flow ownership precede aesthetic cost.
/// Every cost term is nonnegative and dimensionless, including font scale.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CompositionQuality {
    pub(crate) requested_lines_error: usize,
    pub(crate) outside_owned_center: bool,
    pub(crate) cost: f32,
}

impl CompositionQuality {
    pub(crate) fn compare(self, other: Self) -> std::cmp::Ordering {
        self.requested_lines_error
            .cmp(&other.requested_lines_error)
            .then_with(|| self.outside_owned_center.cmp(&other.outside_owned_center))
            .then_with(|| self.cost.total_cmp(&other.cost))
    }
}

pub(crate) fn font_scale_cost(size: f32, reference: f32) -> f32 {
    // A scale change is a soft contextual prior, not an exact CJK-to-Latin em
    // conversion. A twofold change must cost much more than one word split.
    (16.0 * (f64::from(size) / f64::from(reference)).ln().powi(2)) as f32
}

fn composition_font_sizes(minimum: f32, maximum: f32, reference: f32) -> Vec<f32> {
    // Relative samples keep the same composition frontier after image resizing.
    // Endpoints and the exact contextual scale are never rounded to font pixels.
    const SAMPLES_PER_OCTAVE: f64 = 64.0;
    let first =
        ((f64::from(minimum) / f64::from(reference)).log2() * SAMPLES_PER_OCTAVE).ceil() as i32;
    let last =
        ((f64::from(maximum) / f64::from(reference)).log2() * SAMPLES_PER_OCTAVE).floor() as i32;
    let mut sizes = vec![minimum, maximum, reference.clamp(minimum, maximum)];
    sizes.extend(
        (first..=last).map(|step| {
            (f64::from(reference) * (f64::from(step) / SAMPLES_PER_OCTAVE).exp2()) as f32
        }),
    );
    sizes.retain(|size| *size >= minimum && *size <= maximum);
    sizes.sort_by(|left, right| {
        font_scale_cost(*left, reference)
            .total_cmp(&font_scale_cost(*right, reference))
            .then_with(|| right.total_cmp(left))
    });
    sizes.dedup_by(|left, right| {
        (*left - *right).abs() <= f32::EPSILON * left.abs().max(right.abs())
    });
    sizes
}

enum CompositionMeasurement<'a> {
    Pending,
    Infeasible,
    Fitting(LayoutRun<'a>),
}

type FittingLayoutResult<'a> = Result<Option<LayoutRun<'a>>>;

/// One immutable text/footprint measurement context, reused when calibrating
/// the attainable scale and evaluating readability. Failed fits are retained
/// too; a new scoring reference does not repeat shaping for this footprint.
pub(crate) struct CompositionSearch<'a, M> {
    minimum: f32,
    maximum: f32,
    reference: f32,
    samples: Vec<(f32, CompositionMeasurement<'a>)>,
    measure: M,
}

impl<'a, M: FnMut(f32) -> FittingLayoutResult<'a>> CompositionSearch<'a, M> {
    fn new(minimum: f32, maximum: f32, reference: f32, measure: M) -> Result<Self> {
        anyhow::ensure!(
            minimum.is_finite()
                && maximum.is_finite()
                && reference.is_finite()
                && minimum > 0.0
                && maximum >= minimum
                && reference > 0.0,
            "invalid composition font-size interval"
        );
        Ok(Self {
            minimum,
            maximum,
            reference,
            samples: composition_font_sizes(minimum, maximum, reference)
                .into_iter()
                .map(|size| (size, CompositionMeasurement::Pending))
                .collect(),
            measure,
        })
    }

    pub(crate) fn reference(&self) -> f32 {
        self.reference
    }

    pub(crate) fn select(
        &mut self,
        reference: f32,
        ceiling: Option<CompositionQuality>,
        quality_at: impl Fn(&LayoutRun<'a>) -> CompositionQuality,
    ) -> Result<Option<&LayoutRun<'a>>> {
        anyhow::ensure!(
            reference.is_finite() && reference > 0.0,
            "invalid composition reference"
        );
        anyhow::ensure!(
            ceiling.is_none_or(|quality| quality.cost.is_finite() && quality.cost >= 0.0),
            "invalid composition ceiling"
        );
        // Keep the measured sampling grid fixed. A shared reference from another
        // footprint is also considered exactly, without quantizing cached sizes.
        let exact = reference.clamp(self.minimum, self.maximum);
        if !self.samples.iter().any(|(size, _)| *size == exact) {
            self.samples.push((exact, CompositionMeasurement::Pending));
        }
        self.samples.sort_by(|(left, _), (right, _)| {
            font_scale_cost(*left, reference)
                .total_cmp(&font_scale_cost(*right, reference))
                .then_with(|| right.total_cmp(left))
        });
        let mut selected: Option<(usize, CompositionQuality)> = None;
        for (index, (size, measurement)) in self.samples.iter_mut().enumerate() {
            let bound = selected.as_ref().map(|(_, quality)| *quality).or(ceiling);
            if bound.is_some_and(|quality| {
                quality.requested_lines_error == 0
                    && !quality.outside_owned_center
                    && font_scale_cost(*size, reference) > quality.cost
            }) {
                // The scale lower bound also applies to an incumbent from
                // another footprint. Reflow itself need not be monotonic.
                break;
            }
            if matches!(measurement, CompositionMeasurement::Pending) {
                *measurement = match (self.measure)(*size)? {
                    Some(layout) => CompositionMeasurement::Fitting(layout),
                    None => CompositionMeasurement::Infeasible,
                };
            }
            let CompositionMeasurement::Fitting(candidate) = measurement else {
                continue;
            };
            let quality = quality_at(candidate);
            anyhow::ensure!(
                quality.cost.is_finite() && quality.cost >= 0.0,
                "invalid composition quality"
            );
            if ceiling.is_some_and(|ceiling| quality.compare(ceiling).is_gt()) {
                continue;
            }
            if selected
                .as_ref()
                .is_none_or(|(_, current)| quality.compare(*current).is_lt())
            {
                selected = Some((index, quality));
            }
        }
        Ok(selected.map(|(index, _)| {
            let CompositionMeasurement::Fitting(layout) = &self.samples[index].1 else {
                unreachable!("only fitting compositions can be selected")
            };
            layout
        }))
    }
}

#[derive(Clone)]
struct LineRun<'a> {
    shaped: ShapedRun<'a>,
    level: unicode_bidi::Level,
}

#[derive(Clone)]
struct ShapedSegment<'a> {
    range: Range<usize>,
    visible_end: usize,
    next_offset: usize,
    is_mandatory: bool,
    runs: Vec<LineRun<'a>>,
    break_penalty: f32,
    break_suffix: Option<LineBreakSuffix>,
}

#[derive(Clone, Copy, Debug)]
struct BreakPoint {
    hyphenated: bool,
    break_penalty: f32,
    is_mandatory: bool,
}

/// Widths belong to complete candidate lines, not additive word fragments.
/// Rows start at a break opportunity and stop at the next mandatory break.
struct LineMeasurements {
    segments: Vec<BreakPoint>,
    advances: Vec<Vec<f32>>,
}

impl std::ops::Deref for LineMeasurements {
    type Target = [BreakPoint];

    fn deref(&self) -> &Self::Target {
        &self.segments
    }
}

impl LineMeasurements {
    fn advance(&self, start: usize, end: usize) -> f32 {
        self.advances[start][end - start - 1]
    }
}

#[derive(Clone, Copy, Debug)]
struct LineProfile {
    /// Available extent on the inline axis: X for horizontal text, Y for vertical text.
    width: f32,
    center_offset: f32,
    block_baseline: f32,
}

#[derive(Clone, Copy, Debug)]
struct InkBand {
    before: f32,
    after: f32,
}

impl InkBand {
    fn thickness(self) -> f32 {
        self.before + self.after
    }
}

#[derive(Debug)]
struct LineBreakResult {
    breaks: Vec<usize>,
    profiles: Vec<LineProfile>,
    overflowed: bool,
    contour_profiled: bool,
    cost: f32,
}

#[derive(Clone, Debug)]
struct ComicBalloon {
    width: f32,
    height: f32,
    contours: Vec<ContourConstraint>,
    minimum_air: f32,
    preferred_center: Option<(f32, f32)>,
    preferred_line_count: Option<usize>,
    strict_centering: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ComicHyphenQuality {
    compact: usize,
    total: usize,
}

#[derive(Clone, Debug)]
struct ContourConstraint {
    points: Vec<(f32, f32)>,
    air_scale: f32,
}

#[derive(Clone)]
pub struct TextLayout<'a> {
    writing_mode: WritingMode,
    center_vertical_punctuation: bool,
    language: Option<harfrust::Language>,
    hyphenation_lang: Option<Lang>,
    hyphenation_policy: HyphenationPolicy,
    comic_balloon: Option<ComicBalloon>,
    font: &'a Font,
    fallback_fonts: &'a [Font],
    font_size: Option<f32>,
    min_font_size: Option<f32>,
    max_font_size: Option<f32>,
    max_width: Option<f32>,
    max_height: Option<f32>,
    alignment: Option<TextAlign>,
    line_height: Option<f32>,
    letter_spacing: f32,
    word_spacing: f32,
    cjk_punctuation_layout: bool,
    natural_line_breaks: bool,
    preferred_font_size: Option<f32>,
    ink_outset: f32,
}

fn largest_fitting_font_size<T>(
    minimum: f32,
    maximum: f32,
    mut layout_at: impl FnMut(f32) -> Result<T>,
    fits: impl Fn(&T) -> bool,
) -> Result<Option<T>> {
    if !minimum.is_finite() || !maximum.is_finite() || maximum < minimum {
        return Err(anyhow::anyhow!(
            "font size limits must be finite and ordered"
        ));
    }

    let candidate = layout_at(maximum)?;
    if fits(&candidate) {
        return Ok(Some(candidate));
    }
    let mut last_size = maximum;
    let mut whole_size = maximum.floor() as i64;
    let minimum_whole_size = minimum.ceil() as i64;
    while whole_size >= minimum_whole_size {
        let size = whole_size as f32;
        whole_size -= 1;
        if (size - last_size).abs() <= f32::EPSILON {
            continue;
        }
        let candidate = layout_at(size)?;
        if fits(&candidate) {
            return Ok(Some(candidate));
        }
        last_size = size;
    }
    if (minimum - last_size).abs() > f32::EPSILON {
        let candidate = layout_at(minimum)?;
        if fits(&candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn largest_monotonic_fitting_font_size<T>(
    minimum: f32,
    maximum: f32,
    mut layout_at: impl FnMut(f32) -> Result<T>,
    fits: impl Fn(&T) -> bool,
) -> Result<Option<T>> {
    if !minimum.is_finite() || !maximum.is_finite() || maximum < minimum {
        return Err(anyhow::anyhow!(
            "font size limits must be finite and ordered"
        ));
    }

    let maximum_layout = layout_at(maximum)?;
    if fits(&maximum_layout) {
        return Ok(Some(maximum_layout));
    }
    let mut best = layout_at(minimum)?;
    if !fits(&best) {
        return Ok(None);
    }

    let mut low = minimum;
    let mut high = maximum;
    let mut iterations = 0u32;
    while high - low > 0.01 && iterations < 16 {
        iterations += 1;
        let size = (low + high) * 0.5;
        let layout = layout_at(size)?;
        if fits(&layout) {
            best = layout;
            low = size;
        } else {
            high = size;
        }
    }
    Ok(Some(best))
}

/// One paragraph's immutable shaping context is shared by measurement and paint.
/// Discretionary break opportunities never become shaping boundaries by themselves.
struct ParagraphShaper<'t, 'a> {
    layout: &'t TextLayout<'a>,
    bidi: BidiInfo<'t>,
    options: ShapingOptions<'t>,
    fonts: Vec<&'a Font>,
    primary_covers_text: bool,
}

impl<'a> ParagraphShaper<'_, 'a> {
    fn shape(
        &self,
        range: Range<usize>,
        suffix: Option<LineBreakSuffix>,
    ) -> Result<Vec<LineRun<'a>>> {
        let text = self.bidi.text;
        let bidi_info = &self.bidi;
        let options = &self.options;
        let source = &text[range.clone()];
        let line_text = match suffix {
            Some(suffix) => std::borrow::Cow::Owned(format!("{source}{}", suffix.as_str())),
            None => std::borrow::Cow::Borrowed(source),
        };
        // Synthetic punctuation participates in the same shaping context as the
        // final word, but its cluster still points to the source break boundary.
        let level_at = |offset: usize| {
            bidi_info
                .levels
                .get(range.start + offset.min(source.len().saturating_sub(1)))
                .copied()
                .unwrap_or_else(unicode_bidi::Level::ltr)
        };
        let shaper = TextShaper::new();
        let mut runs = Vec::new();
        let mut characters = line_text
            .char_indices()
            .map(|(offset, _)| offset)
            .peekable();
        while let Some(start) = characters.next() {
            let level = level_at(start);
            let mut end = line_text.len();
            while let Some(&next) = characters.peek() {
                if level_at(next) != level {
                    end = next;
                    break;
                }
                characters.next();
            }
            let run_text = &line_text[start..end];
            let mut run_options = options.clone();
            run_options.direction = if self.layout.writing_mode.is_vertical() {
                harfrust::Direction::TopToBottom
            } else if level.is_rtl() {
                harfrust::Direction::RightToLeft
            } else {
                harfrust::Direction::LeftToRight
            };
            let normalized = self
                .layout
                .cjk_punctuation_layout
                .then(|| normalize_cjk_emphasis_punctuation(run_text))
                .flatten();
            let shaping_text = normalized
                .as_ref()
                .map_or(run_text, |(text, _)| text.as_str());
            for mut shaped in shape_script_runs(
                &shaper,
                shaping_text,
                &self.fonts,
                &run_options,
                self.primary_covers_text && normalized.is_none(),
            )? {
                self.layout.apply_spacing(shaping_text, &mut shaped);
                if self.layout.writing_mode.is_vertical() && self.layout.center_vertical_punctuation
                {
                    self.layout.center_vertical_punctuation(
                        options.font_size,
                        shaping_text,
                        &mut shaped.glyphs,
                    );
                }
                if self.layout.cjk_punctuation_layout {
                    self.layout.layout_cjk_emphasis_runs(
                        options.font_size,
                        shaping_text,
                        &mut shaped,
                    );
                }
                for glyph in &mut shaped.glyphs {
                    if let Some((_, map)) = &normalized
                        && let Some(&cluster) = map.get(glyph.cluster as usize)
                    {
                        glyph.cluster = cluster;
                    }
                    glyph.cluster += (range.start + start) as u32;
                }
                runs.push(LineRun { shaped, level });
            }
        }
        Ok(runs)
    }
}

impl<'a> TextLayout<'a> {
    #[must_use]
    pub fn new(font: &'a Font) -> Self {
        Self {
            writing_mode: WritingMode::Horizontal,
            center_vertical_punctuation: true,
            language: None,
            hyphenation_lang: Some(Lang::English),
            hyphenation_policy: HyphenationPolicy::Normal,
            comic_balloon: None,
            font,
            fallback_fonts: &[],
            font_size: None,
            min_font_size: None,
            max_font_size: None,
            max_width: None,
            max_height: None,
            alignment: None,
            line_height: None,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            cjk_punctuation_layout: false,
            natural_line_breaks: false,
            preferred_font_size: None,
            ink_outset: 0.0,
        }
    }

    #[must_use]
    pub fn with_font_size(mut self, size: f32) -> Self {
        self.font_size = Some(size);
        self.min_font_size = None;
        self.max_font_size = None;
        self
    }

    /// Automatically fit the text to its bounds without growing beyond `size`.
    #[must_use]
    pub fn with_max_font_size(mut self, size: f32) -> Self {
        self.font_size = None;
        self.max_font_size = Some(size);
        self
    }

    /// Prevent automatically fitted text from becoming unreadably small.
    #[must_use]
    pub fn with_min_font_size(mut self, size: f32) -> Self {
        self.font_size = None;
        self.min_font_size = Some(size);
        self
    }

    pub(crate) fn with_natural_line_breaks(mut self) -> Self {
        self.natural_line_breaks = true;
        self
    }

    #[must_use]
    pub fn with_writing_mode(mut self, mode: WritingMode) -> Self {
        self.writing_mode = mode;
        self
    }

    #[must_use]
    pub fn with_language_tag(mut self, tag: &str) -> Self {
        self.language = harfrust::Language::new(tag);
        self.hyphenation_lang = hyphenation_lang_from_tag(tag);
        self
    }

    #[must_use]
    pub fn with_hyphenation_policy(mut self, policy: HyphenationPolicy) -> Self {
        self.hyphenation_policy = policy;
        self
    }

    pub(crate) fn with_comic_balloon(
        mut self,
        width: f32,
        height: f32,
        contour: Vec<(f32, f32)>,
        minimum_air: f32,
    ) -> Self {
        let preferred_center = intersected_centroid(&[contour.as_slice()]);
        self.comic_balloon = Some(ComicBalloon {
            width,
            height,
            contours: vec![ContourConstraint {
                points: contour,
                air_scale: 1.0,
            }],
            minimum_air: minimum_air.max(0.0),
            preferred_center,
            preferred_line_count: None,
            strict_centering: false,
        });
        self
    }

    pub(crate) fn with_comic_balloon_constraints(
        mut self,
        width: f32,
        height: f32,
        contours: Vec<Vec<(f32, f32)>>,
        minimum_air: f32,
    ) -> Self {
        let preferred_center =
            intersected_centroid(&contours.iter().map(Vec::as_slice).collect::<Vec<_>>());
        self.comic_balloon = Some(ComicBalloon {
            width,
            height,
            contours: contours
                .into_iter()
                .enumerate()
                .map(|(index, points)| ContourConstraint {
                    points,
                    // The physical wall owns one representative-glyph clearance. A
                    // partition is shared by two flows, so each side contributes half.
                    air_scale: if index == 0 { 1.0 } else { 0.5 },
                })
                .collect(),
            minimum_air: minimum_air.max(0.0),
            preferred_center,
            preferred_line_count: None,
            strict_centering: false,
        });
        self
    }

    pub(crate) fn with_comic_preferred_center(
        mut self,
        center: (f32, f32),
        strict_centering: bool,
    ) -> Self {
        if let Some(balloon) = &mut self.comic_balloon {
            balloon.preferred_center =
                (center.0.is_finite() && center.1.is_finite()).then_some(center);
            balloon.strict_centering = strict_centering;
        }
        self
    }

    pub(crate) fn with_comic_preferred_line_count(mut self, line_count: usize) -> Self {
        if let Some(balloon) = &mut self.comic_balloon {
            balloon.preferred_line_count = (line_count > 0).then_some(line_count);
        }
        self
    }

    pub(crate) fn with_preferred_font_size(mut self, size: f32) -> Self {
        self.preferred_font_size = (size.is_finite() && size > 0.0).then_some(size);
        self
    }

    #[must_use]
    pub fn with_fallback_fonts(mut self, fonts: &'a [Font]) -> Self {
        self.fallback_fonts = fonts;
        self
    }

    #[must_use]
    pub fn with_max_width(mut self, width: f32) -> Self {
        self.max_width = Some(width);
        self
    }

    #[must_use]
    pub fn with_max_height(mut self, height: f32) -> Self {
        self.max_height = Some(height);
        self
    }

    #[must_use]
    pub fn with_alignment(mut self, alignment: TextAlign) -> Self {
        self.alignment = Some(alignment);
        self
    }

    #[must_use]
    pub fn with_line_height(mut self, ratio: f32) -> Self {
        self.line_height = Some(ratio);
        self
    }

    #[must_use]
    pub fn with_spacing(mut self, letter: f32, word: f32) -> Self {
        self.letter_spacing = letter;
        self.word_spacing = word;
        self
    }

    pub(crate) fn with_cjk_punctuation_layout(mut self, enabled: bool) -> Self {
        self.cjk_punctuation_layout = enabled;
        self
    }

    pub fn run(&self, text: &str) -> Result<LayoutRun<'a>> {
        if let Some(font_size) = self.font_size {
            return self.run_with_size(text, font_size);
        }

        self.run_auto(text, |size| self.run_with_size(text, size))
    }

    /// The paint owner resolves dilation at each candidate's actual font size.
    /// Keep it out of stored typography and reserve it before choosing line breaks.
    pub(crate) fn run_with_ink_outset(
        &self,
        text: &str,
        outset_at: impl Fn(f32) -> f32,
    ) -> Result<LayoutRun<'a>> {
        let layout_at = |size| {
            let mut candidate = self.clone();
            candidate.ink_outset = outset_at(size);
            anyhow::ensure!(
                candidate.ink_outset.is_finite() && candidate.ink_outset >= 0.0,
                "text ink outset must be finite and nonnegative"
            );
            candidate.run_with_size(text, size)
        };
        if let Some(size) = self.font_size {
            layout_at(size)
        } else {
            self.run_auto(text, layout_at)
        }
    }

    /// Owns measurements against the caller's painted bounds. Outline dilation
    /// is reserved before wrapping, not diagnosed afterward.
    pub(crate) fn fitting_compositions(
        &self,
        text: &str,
        minimum: f32,
        maximum: f32,
        bounds_at: impl Fn(f32) -> (f32, f32),
        fits: impl Fn(&LayoutRun<'a>) -> bool,
    ) -> Result<Option<CompositionSearch<'a, impl FnMut(f32) -> FittingLayoutResult<'a>>>> {
        let layout_at = move |size| {
            let (width, height) = bounds_at(size);
            self.clone()
                .with_font_size(size)
                .with_max_width(width.max(0.5))
                .with_max_height(height.max(0.5))
                .run(text)
        };
        let reference = match self.preferred_font_size {
            Some(reference) => reference,
            None => {
                let Some(capacity) =
                    largest_monotonic_fitting_font_size(minimum, maximum, &layout_at, &fits)?
                else {
                    return Ok(None);
                };
                capacity.font_size
            }
        };
        Ok(Some(CompositionSearch::new(
            minimum,
            maximum,
            reference,
            move |size| {
                let layout = layout_at(size)?;
                Ok(fits(&layout).then_some(layout))
            },
        )?))
    }

    fn run_auto(
        &self,
        text: &str,
        layout_at: impl Fn(f32) -> Result<LayoutRun<'a>>,
    ) -> Result<LayoutRun<'a>> {
        let max_height = self.max_height.unwrap_or(f32::INFINITY);
        let max_width = self.max_width.unwrap_or(f32::INFINITY);
        let maximum = self.max_font_size.unwrap_or(300.0).max(0.5);
        let minimum = self
            .min_font_size
            .unwrap_or(maximum.min(1.0))
            .max(0.5)
            .min(maximum);
        let fits = |layout: &LayoutRun<'_>| {
            !layout.overflowed()
                && layout.width <= max_width + f32::EPSILON
                && layout.height <= max_height + f32::EPSILON
        };
        let reference = match self.preferred_font_size {
            Some(reference) => reference,
            None => {
                // Only typography without a contextual source scale uses geometric
                // capacity as its reference. Contour reflow is not monotonic.
                let capacity = if self.comic_balloon.is_some() {
                    let upper = self.comic_rectangular_search_maximum(
                        text,
                        minimum,
                        maximum,
                        self.hyphenation_policy,
                    )?;
                    largest_fitting_font_size(minimum, upper, &layout_at, fits)?
                } else {
                    largest_monotonic_fitting_font_size(minimum, maximum, &layout_at, fits)?
                };
                let Some(capacity) = capacity else {
                    return layout_at(minimum);
                };
                capacity.font_size
            }
        };
        let mut search = CompositionSearch::new(minimum, maximum, reference, |size| {
            let layout = layout_at(size)?;
            Ok(fits(&layout).then_some(layout))
        })?;
        let reference = if self.preferred_font_size.is_some() {
            // An unreachable source em must not make a marginal size gain more
            // valuable than intact words. Score aesthetics at the nearest size
            // that can actually fit while retaining explicit flow ownership.
            let nearest = search.select(reference, None, |candidate| CompositionQuality {
                cost: font_scale_cost(candidate.font_size, reference),
                ..self.composition_quality(text, candidate, reference)
            })?;
            let Some(nearest) = nearest else {
                return layout_at(minimum);
            };
            nearest.font_size
        } else {
            reference
        };
        let selected = search
            .select(reference, None, |candidate| {
                self.composition_quality(text, candidate, reference)
            })?
            .cloned();
        #[cfg(debug_assertions)]
        if let Some(selected) = &selected {
            tracing::trace!(
                target: "koharu_typesetting_probe",
                marker = "composition_choice",
                text, minimum, maximum, reference,
                selected_size = selected.font_size,
                selected_lines = selected.lines.len(),
                quality = ?self.composition_quality(text, selected, reference),
            );
        }
        match selected {
            Some(layout) => Ok(layout),
            None => layout_at(minimum),
        }
    }

    fn composition_quality(
        &self,
        text: &str,
        layout: &LayoutRun<'_>,
        reference: f32,
    ) -> CompositionQuality {
        let mut quality = CompositionQuality {
            cost: layout.composition_cost(text, reference),
            ..CompositionQuality::default()
        };
        if let Some(balloon) = &self.comic_balloon {
            quality.requested_lines_error = balloon
                .preferred_line_count
                .map_or(0, |requested| layout.lines.len().abs_diff(requested));
            quality.outside_owned_center = balloon.strict_centering
                && balloon.preferred_center.is_some()
                && !self.comic_layout_is_centered(layout);
            let block_extent = if self.writing_mode.is_vertical() {
                balloon.width
            } else {
                balloon.height
            };
            let comfortable_error = block_extent * COMIC_CENTER_CONTAINER_TOLERANCE_RATIO;
            quality.cost += ((self.comic_center_error(layout) - comfortable_error).max(0.0)
                / layout.font_size)
                .powi(2);
        }
        quality
    }

    fn comic_layout_is_centered(&self, layout: &LayoutRun<'_>) -> bool {
        self.comic_balloon
            .as_ref()
            .is_some_and(|balloon| balloon.preferred_center.is_some())
            && self.comic_center_error_em(layout) <= COMIC_CENTER_ERROR_TOLERANCE_EM
    }

    fn comic_center_error_em(&self, layout: &LayoutRun<'_>) -> f32 {
        self.comic_center_error(layout) / layout.font_size.max(0.5)
    }

    fn comic_center_error(&self, layout: &LayoutRun<'_>) -> f32 {
        let balloon = self
            .comic_balloon
            .as_ref()
            .expect("comic centeredness requires a balloon");
        let actual = (
            balloon.width * 0.5 + layout.placement_offset_x,
            balloon.height * 0.5 + layout.placement_offset_y,
        );
        let preferred = balloon.center();
        (actual.0 - preferred.0).hypot(actual.1 - preferred.1)
    }

    fn comic_rectangular_search_maximum(
        &self,
        text: &str,
        minimum: f32,
        maximum: f32,
        policy: HyphenationPolicy,
    ) -> Result<f32> {
        let balloon = self
            .comic_balloon
            .as_ref()
            .expect("rectangular comic preflight requires a balloon");
        let outer_rectangle = balloon
            .contours
            .first()
            .filter(|contour| {
                contour.points.len() >= 3
                    && contour
                        .points
                        .iter()
                        .all(|(x, y)| x.is_finite() && y.is_finite())
            })
            .and_then(|contour| {
                let (left, top, right, bottom) = contour.points.iter().fold(
                    (
                        f32::INFINITY,
                        f32::INFINITY,
                        f32::NEG_INFINITY,
                        f32::NEG_INFINITY,
                    ),
                    |(left, top, right, bottom), &(x, y)| {
                        (left.min(x), top.min(y), right.max(x), bottom.max(y))
                    },
                );
                (right > left && bottom > top).then_some(vec![
                    (left, top),
                    (right, top),
                    (right, bottom),
                    (left, bottom),
                ])
            })
            .unwrap_or_else(|| {
                vec![
                    (0.0, 0.0),
                    (balloon.width, 0.0),
                    (balloon.width, balloon.height),
                    (0.0, balloon.height),
                ]
            });
        let mut rectangle = self.clone();
        rectangle.hyphenation_policy = policy;
        rectangle.comic_balloon = Some(ComicBalloon {
            width: balloon.width,
            height: balloon.height,
            contours: vec![ContourConstraint {
                // Balloon contours use the original geometry frame translated into
                // the inset layout frame, so the physical wall can legitimately sit
                // outside 0..width/height. The preflight must enclose that wall or it
                // can cap the exhaustive contour search below a legal font size.
                points: outer_rectangle,
                air_scale: 1.0,
            }],
            minimum_air: balloon.minimum_air,
            // This is only a capacity upper bound. Using the irregular body's
            // off-center axis here would understate the rectangle's usable width.
            preferred_center: None,
            preferred_line_count: balloon.preferred_line_count,
            strict_centering: false,
        });
        let max_width = self.max_width.unwrap_or(f32::INFINITY);
        let max_height = self.max_height.unwrap_or(f32::INFINITY);
        let fits = |layout: &LayoutRun<'_>| {
            !layout.overflowed()
                && layout.width <= max_width + f32::EPSILON
                && layout.height <= max_height + f32::EPSILON
        };
        let upper = largest_fitting_font_size(
            minimum,
            maximum,
            |size| rectangle.run_with_size(text, size),
            fits,
        )?
        .map_or(minimum, |layout| layout.font_size.ceil());
        #[cfg(debug_assertions)]
        if upper <= minimum + f32::EPSILON
            && tracing::enabled!(
                target: "koharu_typesetting_probe",
                tracing::Level::TRACE
            )
        {
            let next_size = (upper.floor() + 1.0).min(maximum);
            let trace = [upper, next_size]
                .into_iter()
                .map(|size| {
                    let layout = rectangle.run_with_size(text, size).ok();
                    (
                        size,
                        layout.as_ref().map(|layout| layout.width),
                        layout.as_ref().map(|layout| layout.height),
                        layout.as_ref().map(|layout| layout.lines.len()),
                        layout.as_ref().map(LayoutRun::discretionary_hyphen_count),
                        layout.as_ref().map(LayoutRun::overflowed),
                        layout.as_ref().map(|layout| {
                            layout
                                .lines
                                .iter()
                                .map(|line| text.get(line.range.clone()).unwrap_or("<invalid>"))
                                .collect::<Vec<_>>()
                        }),
                    )
                })
                .collect::<Vec<_>>();
            tracing::trace!(
                target: "koharu_typesetting_probe",
                marker = "comic_preflight",
                text,
                minimum,
                requested_maximum = maximum,
                preflight_upper = upper,
                rectangle_width = balloon.width,
                rectangle_height = balloon.height,
                max_width = ?self.max_width,
                max_height = ?self.max_height,
                ?trace,
            );
        }
        Ok(upper.clamp(minimum, maximum))
    }

    fn run_with_size(&self, text: &str, font_size: f32) -> Result<LayoutRun<'a>> {
        if self.hyphenation_policy == HyphenationPolicy::LastResort {
            // LastResort keeps intact words whenever that layout fits. Starting
            // with the smaller, word-only frontier also avoids measuring unused
            // discretionary candidates for ordinary dialogue.
            let mut clean = self.clone();
            clean.hyphenation_policy = HyphenationPolicy::Disabled;
            let clean = clean.run_with_size(text, font_size)?;
            if !clean.overflowed() {
                return Ok(clean);
            }

            let mut hyphenated = self.clone();
            hyphenated.hyphenation_policy = HyphenationPolicy::Normal;
            return hyphenated.run_with_size(text, font_size);
        }

        let ordinary = self.run_with_size_pass(text, font_size, false)?;
        if ordinary.overflowed()
            && self.comic_balloon.is_some()
            && !self.writing_mode.is_vertical()
            && self.hyphenation_policy != HyphenationPolicy::Disabled
        {
            let punctuation_continuation = self.run_with_size_pass(text, font_size, true)?;
            if !punctuation_continuation.overflowed() {
                return Ok(punctuation_continuation);
            }
        }
        Ok(ordinary)
    }

    fn run_with_size_pass(
        &self,
        text: &str,
        font_size: f32,
        emergency_terminal_punctuation: bool,
    ) -> Result<LayoutRun<'a>> {
        let mut line_breaker = LineBreaker::new();
        if !self.writing_mode.is_vertical()
            && self.hyphenation_policy != HyphenationPolicy::Disabled
            && let Some(lang) = self.hyphenation_lang
        {
            let (minimum_prefix_length, minimum_suffix_length) = lang.bounds();
            let options = HyphenationOptions::new(lang, HYPHENATION_MIN_WORD_LEN);
            let options = if self.comic_balloon.is_some() {
                options.with_fragment_bounds(
                    minimum_prefix_length.min(COMPACT_HYPHENATION_FRAGMENT_LEN),
                    minimum_suffix_length.min(COMPACT_HYPHENATION_FRAGMENT_LEN),
                )
            } else {
                options
            };
            line_breaker = line_breaker.with_hyphenation(options);
        }
        if emergency_terminal_punctuation {
            line_breaker = line_breaker.with_emergency_terminal_punctuation();
        }
        // Use real font metrics for consistent line sizing across modes.
        let font_ref = self.font.skrifa_ref()?;
        let metrics = font_ref.metrics(Size::new(font_size), self.font.location());
        let ascent = metrics.ascent;
        let descent = -metrics.descent;
        let line_height = self.line_height.map_or_else(
            || (ascent + descent + metrics.leading).max(font_size),
            |ratio| font_size * ratio,
        );

        let bidi_info = BidiInfo::new(text, None);

        let (direction, script) = shaping_direction_for_text(text, self.writing_mode);
        let options = ShapingOptions {
            direction,
            script,
            language: self.language.as_ref(),
            font_size,
            features: if self.writing_mode.is_vertical() {
                &[
                    Feature::new(Tag::new(b"vert"), 1, ..),
                    Feature::new(Tag::new(b"vrt2"), 1, ..),
                ]
            } else {
                &[]
            },
        };
        let max_extent = if self.writing_mode.is_vertical() {
            self.max_height
        } else {
            self.max_width
        }
        .unwrap_or(f32::INFINITY);

        let mut fonts: Vec<&Font> = Vec::with_capacity(1 + self.fallback_fonts.len());
        fonts.push(self.font);
        fonts.extend(self.fallback_fonts.iter());
        let shaper = ParagraphShaper {
            layout: self,
            bidi: bidi_info,
            options,
            fonts,
            primary_covers_text: !self.cjk_punctuation_layout && self.font.covers(text),
        };

        let mut shaped_segments = Vec::new();
        for segment in line_breaker.line_segments(text) {
            let visible_end = segment.range.start + text[segment.range.clone()].trim_end().len();
            let runs = shaper.shape(segment.range.clone(), None)?;
            shaped_segments.push(ShapedSegment {
                range: segment.range,
                visible_end,
                next_offset: segment.next_offset,
                is_mandatory: segment.is_mandatory,
                runs,
                break_penalty: if self.comic_balloon.is_some() || self.natural_line_breaks {
                    natural_break_penalty(text, segment.next_offset)
                } else {
                    0.0
                },
                break_suffix: segment.break_suffix,
            });
        }

        let fallback_ink = if self.writing_mode.is_vertical() {
            InkBand {
                before: line_height * 0.5,
                after: line_height * 0.5,
            }
        } else {
            InkBand {
                before: ascent,
                after: descent,
            }
        };
        let line_ink = if self.comic_balloon.is_some() {
            self.shaped_block_extents(font_size, &shaped_segments)
                .unwrap_or(fallback_ink)
        } else {
            fallback_ink
        };

        let (balloon_air_x, balloon_air_y) =
            self.comic_balloon.as_ref().map_or((0.0, 0.0), |balloon| {
                let inline_air = balloon.air(
                    self.representative_inline_extent(&shaped_segments, line_ink)
                        * COMIC_INLINE_WALL_CLEARANCE_GLYPHS,
                );
                let block_air = balloon.air(line_ink.thickness());
                if self.writing_mode.is_vertical() {
                    (block_air, inline_air)
                } else {
                    (inline_air, block_air)
                }
            });

        // Scan curved walls over the whole painted band, not only the fill.
        // The outline is physical ink, separate from aesthetic wall clearance.
        let line_ink = InkBand {
            before: line_ink.before + self.ink_outset,
            after: line_ink.after + self.ink_outset,
        };

        let mut lines: Vec<LayoutLine<'a>> = Vec::new();
        let mut line_profiles = Vec::new();
        let mut contour_overflowed = false;
        let mut contour_profiled = false;
        let mut line_offset = 0usize;
        if self.comic_balloon.is_some() {
            (contour_overflowed, contour_profiled) = self.append_balanced_segment_lines(
                &shaper,
                &shaped_segments,
                &mut line_offset,
                text.len(),
                false,
                max_extent,
                line_height,
                line_ink,
                balloon_air_x,
                balloon_air_y,
                &mut lines,
                &mut line_profiles,
            )?;
        } else {
            let mut paragraph_start = 0usize;
            for (index, segment) in shaped_segments.iter().enumerate() {
                if !segment.is_mandatory {
                    continue;
                }
                contour_overflowed |= self
                    .append_balanced_segment_lines(
                        &shaper,
                        &shaped_segments[paragraph_start..=index],
                        &mut line_offset,
                        segment.next_offset,
                        true,
                        max_extent,
                        line_height,
                        line_ink,
                        balloon_air_x,
                        balloon_air_y,
                        &mut lines,
                        &mut line_profiles,
                    )?
                    .0;
                paragraph_start = index + 1;
            }
            if paragraph_start < shaped_segments.len() {
                contour_overflowed |= self
                    .append_balanced_segment_lines(
                        &shaper,
                        &shaped_segments[paragraph_start..],
                        &mut line_offset,
                        text.len(),
                        false,
                        max_extent,
                        line_height,
                        line_ink,
                        balloon_air_x,
                        balloon_air_y,
                        &mut lines,
                        &mut line_profiles,
                    )?
                    .0;
            }
        }

        // Baselines depend only on line index and metrics. For vertical text we compute absolute X
        // positions within the layout bounds (0..width) so the renderer can draw from the left.
        let line_count = lines.len();
        let effective_alignment = self.alignment.unwrap_or(TextAlign::Left);

        for (i, line) in lines.iter_mut().enumerate() {
            line.baseline = match self.writing_mode {
                WritingMode::VerticalRl => (
                    self.comic_balloon
                        .as_ref()
                        .and_then(|_| line_profiles.get(i))
                        .map_or(
                            (line_count.saturating_sub(1) as f32 - i as f32) * line_height
                                + line_height * 0.5,
                            |profile| profile.block_baseline,
                        ),
                    ascent,
                ),
                WritingMode::Horizontal => {
                    let y = self
                        .comic_balloon
                        .as_ref()
                        .and_then(|_| line_profiles.get(i))
                        .map_or(ascent + i as f32 * line_height, |profile| {
                            profile.block_baseline
                        });
                    (0.0, y)
                }
            };
        }

        if effective_alignment == TextAlign::Justify && !self.writing_mode.is_vertical() {
            justify_lines(
                text,
                &mut lines,
                max_extent,
                &line_profiles,
                self.ink_outset,
            );
        }

        // Alignment is an inline-axis operation inside one intrinsic text block. Balloon
        // contours constrain line breaking and block placement, but do not bend the text axis.
        const PAD: f32 = 1.0;
        let inline_extent = lines
            .iter()
            .filter_map(|line| self.ink_bounds(font_size, std::slice::from_ref(line)))
            .map(|(min_x, min_y, max_x, max_y)| {
                if self.writing_mode.is_vertical() {
                    max_y - min_y
                } else {
                    max_x - min_x
                }
            })
            .fold(0.0_f32, f32::max);
        let inline_box = (inline_extent > 0.0).then(|| {
            let extent = inline_extent + PAD * 2.0;
            (extent, PAD, extent * 0.5, extent - PAD)
        });
        if let Some((_, inline_start, inline_center, inline_end)) = inline_box {
            for line in &mut lines {
                if let Some((min_x, min_y, max_x, max_y)) =
                    self.ink_bounds(font_size, std::slice::from_ref(line))
                {
                    let (minimum, center, maximum) = if self.writing_mode.is_vertical() {
                        (min_y, (min_y + max_y) * 0.5, max_y)
                    } else {
                        (min_x, (min_x + max_x) * 0.5, max_x)
                    };
                    let offset = match effective_alignment {
                        TextAlign::Left | TextAlign::Justify => inline_start - minimum,
                        TextAlign::Center => inline_center - center,
                        TextAlign::Right => inline_end - maximum,
                    };
                    if self.writing_mode.is_vertical() {
                        line.baseline.1 += offset;
                    } else {
                        line.baseline.0 += offset;
                    }
                }
            }
        }

        if contour_profiled && let Some((_, _, inline_center, _)) = inline_box {
            // Advances guide line breaking; aligned glyph ink owns the final
            // fit decision, including outlines and overhanging font shapes.
            let painted_overflowed =
                !self.painted_lines_fit_profiles(font_size, &lines, &line_profiles, inline_center);
            #[cfg(debug_assertions)]
            if painted_overflowed != contour_overflowed
                && tracing::enabled!(
                    target: "koharu_typesetting_probe",
                    tracing::Level::TRACE
                )
            {
                let painted_profile_trace = lines
                    .iter()
                    .zip(&line_profiles)
                    .map(|(line, profile)| {
                        (
                            self.inline_ink_bounds(font_size, line),
                            profile.width,
                            line.advance,
                        )
                    })
                    .collect::<Vec<_>>();
                tracing::trace!(
                    target: "koharu_typesetting_probe",
                    marker = "comic_painted_profile_fit",
                    text,
                    font_size,
                    advances_overflowed = contour_overflowed,
                    painted_overflowed,
                    ?painted_profile_trace,
                );
            }
            contour_overflowed = painted_overflowed;
        }

        // Compute a tight ink bounding box using per-glyph bounds from the font tables (via skrifa),
        // then normalize only the block axis. The inline axis remains in the fixed layout box.
        let (mut width, mut height) = (0.0, 0.0);
        let mut placement_offset_x = 0.0;
        let mut placement_offset_y = 0.0;
        if let Some((mut min_x, mut min_y, mut max_x, mut max_y)) =
            self.ink_bounds(font_size, &lines)
        {
            // Keep a tiny safety pad for hinting/AA differences.
            min_x -= PAD;
            min_y -= PAD;
            max_x += PAD;
            max_y += PAD;

            for line in &mut lines {
                if inline_box.is_none() || self.writing_mode.is_vertical() {
                    line.baseline.0 -= min_x;
                }
                if inline_box.is_none() || !self.writing_mode.is_vertical() {
                    line.baseline.1 -= min_y;
                }
            }
            let actual_width = (max_x - min_x).max(0.0);
            let actual_height = (max_y - min_y).max(0.0);
            if self.writing_mode.is_vertical() {
                width = actual_width;
                if let Some((inline_extent, _, _, _)) = inline_box {
                    height = inline_extent;
                } else {
                    height = actual_height;
                }
            } else {
                height = actual_height;
                if let Some((inline_extent, _, _, _)) = inline_box {
                    width = inline_extent;
                } else {
                    width = actual_width;
                }
            }

            if let Some(balloon) = &self.comic_balloon {
                let inline_offset = line_profiles
                    .first()
                    .map_or(0.0, |profile| profile.center_offset);
                if self.writing_mode.is_vertical() {
                    let default_left = (balloon.width - width) * 0.5;
                    placement_offset_x = min_x - default_left;
                    placement_offset_y = inline_offset;
                } else {
                    let default_top = (balloon.height - height) * 0.5;
                    placement_offset_x = inline_offset;
                    placement_offset_y = min_y - default_top;
                }
            }
        }

        let overflowed = contour_overflowed
            || self
                .max_width
                .is_some_and(|maximum| width + self.ink_outset * 2.0 > maximum + f32::EPSILON)
            || self
                .max_height
                .is_some_and(|maximum| height + self.ink_outset * 2.0 > maximum + f32::EPSILON);

        Ok(LayoutRun {
            lines,
            width,
            height,
            font_size,
            overflowed,
            placement_offset_x,
            placement_offset_y,
        })
    }

    fn shaped_block_extents(
        &self,
        font_size: f32,
        segments: &[ShapedSegment<'a>],
    ) -> Option<InkBand> {
        let mut metrics_cache = HashMap::new();
        let mut minimum = f32::INFINITY;
        let mut maximum = f32::NEG_INFINITY;

        for segment in segments {
            for run in &segment.runs {
                for glyph in &run.shaped.glyphs {
                    let key = font_key(glyph.font);
                    let glyph_metrics = match metrics_cache.entry(key) {
                        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            let Ok(font_ref) = glyph.font.skrifa_ref() else {
                                continue;
                            };
                            entry.insert(
                                font_ref.glyph_metrics(Size::new(font_size), glyph.font.location()),
                            )
                        }
                    };

                    let glyph_id = skrifa::GlyphId::new(glyph.glyph_id);
                    if let Some(bounds) = glyph_metrics.bounds(glyph_id) {
                        if self.writing_mode.is_vertical() {
                            let synthetic_pad = glyph
                                .font
                                .synthetic_skew()
                                .map_or(0.0, |_| font_size * 0.25)
                                + if glyph.font.synthetic_bold() {
                                    font_size * 0.05
                                } else {
                                    0.0
                                };
                            minimum = minimum.min(glyph.x_offset + bounds.x_min - synthetic_pad);
                            maximum = maximum.max(glyph.x_offset + bounds.x_max + synthetic_pad);
                        } else {
                            minimum = minimum.min(-glyph.y_offset - bounds.y_max);
                            maximum = maximum.max(-glyph.y_offset - bounds.y_min);
                        }
                    }
                }
            }
        }

        minimum.is_finite().then(|| InkBand {
            before: (-minimum).max(0.0),
            after: maximum.max(0.0),
        })
    }

    fn representative_inline_extent(
        &self,
        segments: &[ShapedSegment<'a>],
        line_ink: InkBand,
    ) -> f32 {
        // Comic lettering convention expresses wall clearance in letter-sized terms.
        // This returns one representative glyph; the caller assigns half of it to
        // each physical side wall. Use the shaped text's median advance instead of a
        // font-wide average, which substantially overstates compact Latin lettering.
        let mut extents = Vec::new();
        for glyph in segments
            .iter()
            .flat_map(|segment| segment.runs.iter())
            .flat_map(|run| run.shaped.glyphs.iter())
        {
            let extent = if self.writing_mode.is_vertical() {
                glyph.y_advance.abs()
            } else {
                glyph.x_advance.abs()
            };
            if extent.is_finite() && extent > f32::EPSILON {
                extents.push(extent);
            }
        }
        extents.sort_by(f32::total_cmp);
        let representative = extents
            .get(extents.len() / 2)
            .copied()
            .unwrap_or_else(|| line_ink.thickness());
        if self.writing_mode.is_vertical() {
            representative
        } else {
            // A punctuation-heavy string must still retain visible wall air even
            // when most inline advances are much narrower than an ordinary letter.
            representative.max(line_ink.thickness() * 0.5)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_balanced_segment_lines(
        &self,
        shaper: &ParagraphShaper<'_, 'a>,
        segments: &[ShapedSegment<'a>],
        line_offset: &mut usize,
        final_next_offset: usize,
        force_final_line: bool,
        max_extent: f32,
        line_height: f32,
        line_ink: InkBand,
        balloon_air_x: f32,
        balloon_air_y: f32,
        lines: &mut Vec<LayoutLine<'a>>,
        line_profiles: &mut Vec<LineProfile>,
    ) -> Result<(bool, bool)> {
        if segments.is_empty() {
            if force_final_line {
                *line_offset = self.push_layout_line(
                    Vec::new(),
                    *line_offset..*line_offset,
                    final_next_offset,
                    None,
                    true,
                    lines,
                );
                line_profiles.push(LineProfile {
                    width: max_extent,
                    center_offset: 0.0,
                    block_baseline: 0.0,
                });
            }
            return Ok((false, false));
        }

        let mut measures = LineMeasurements {
            segments: segments
                .iter()
                .map(|segment| BreakPoint {
                    hyphenated: segment.break_suffix.is_some() && !segment.is_mandatory,
                    break_penalty: segment.break_penalty,
                    is_mandatory: segment.is_mandatory,
                })
                .collect(),
            advances: Vec::with_capacity(segments.len()),
        };
        for start in 0..segments.len() {
            let mut advances = Vec::with_capacity(segments.len() - start);
            for end in start + 1..=segments.len() {
                let last = &segments[end - 1];
                let suffix = if end < segments.len() && !last.is_mandatory {
                    last.break_suffix
                } else {
                    None
                };
                let runs = shaper.shape(segments[start].range.start..last.visible_end, suffix)?;
                let advance = runs
                    .iter()
                    .flat_map(|run| &run.shaped.glyphs)
                    .map(|glyph| {
                        if self.writing_mode.is_vertical() {
                            glyph.y_advance.abs()
                        } else {
                            glyph.x_advance
                        }
                    })
                    .sum::<f32>();
                advances.push(advance + self.ink_outset * 2.0);
                if last.is_mandatory {
                    break;
                }
            }
            measures.advances.push(advances);
        }
        let result = if let Some(balloon) = self.comic_balloon.as_ref() {
            comic_line_breaks(
                &measures,
                balloon,
                self.writing_mode,
                line_height,
                line_ink,
                (balloon_air_x, balloon_air_y),
                self.hyphenation_policy,
            )
        } else if max_extent.is_finite() && max_extent > 0.0 {
            line_breaks_with_policy(&measures, max_extent, self.hyphenation_policy)
        } else {
            LineBreakResult {
                breaks: vec![segments.len()],
                profiles: vec![LineProfile {
                    width: measures.advance(0, segments.len()),
                    center_offset: 0.0,
                    block_baseline: 0.0,
                }],
                overflowed: false,
                contour_profiled: false,
                cost: 0.0,
            }
        };

        let mut start = 0usize;
        for (line_index, end) in result.breaks.iter().copied().enumerate() {
            if end <= start || end > segments.len() {
                continue;
            }
            let final_line = end == segments.len();
            let mandatory_line = segments[end - 1].is_mandatory;
            let visible_end = segments[end - 1].visible_end;
            let next_offset = if mandatory_line {
                segments[end - 1].next_offset
            } else if final_line {
                final_next_offset
            } else {
                segments[end].range.start
            };
            let suffix = if final_line || mandatory_line {
                None
            } else {
                segments[end - 1].break_suffix
            };
            let runs = shaper.shape(segments[start].range.start..visible_end, suffix)?;
            *line_offset = self.push_layout_line(
                runs,
                *line_offset..visible_end,
                next_offset,
                suffix,
                mandatory_line || (force_final_line && final_line),
                lines,
            );
            line_profiles.push(
                result
                    .profiles
                    .get(line_index)
                    .copied()
                    .unwrap_or(LineProfile {
                        width: max_extent,
                        center_offset: 0.0,
                        block_baseline: 0.0,
                    }),
            );
            start = end;
        }
        Ok((result.overflowed, result.contour_profiled))
    }

    fn painted_lines_fit_profiles(
        &self,
        font_size: f32,
        lines: &[LayoutLine<'a>],
        profiles: &[LineProfile],
        inline_center: f32,
    ) -> bool {
        lines.len() == profiles.len()
            && lines.iter().zip(profiles).all(|(line, profile)| {
                self.inline_ink_bounds(font_size, line)
                    .is_none_or(|(minimum, maximum)| {
                        let half_width = profile.width * 0.5;
                        minimum - self.ink_outset
                            >= inline_center - half_width - COMIC_LINE_RASTER_TOLERANCE
                            && maximum + self.ink_outset
                                <= inline_center + half_width + COMIC_LINE_RASTER_TOLERANCE
                    })
            })
    }

    fn inline_ink_bounds(&self, font_size: f32, line: &LayoutLine<'a>) -> Option<(f32, f32)> {
        self.ink_bounds(font_size, std::slice::from_ref(line)).map(
            |(minimum_x, minimum_y, maximum_x, maximum_y)| {
                if self.writing_mode.is_vertical() {
                    (minimum_y, maximum_y)
                } else {
                    (minimum_x, maximum_x)
                }
            },
        )
    }

    fn push_layout_line(
        &self,
        mut runs: Vec<LineRun<'a>>,
        range: Range<usize>,
        next_offset: usize,
        break_suffix: Option<LineBreakSuffix>,
        force_push: bool,
        lines: &mut Vec<LayoutLine<'a>>,
    ) -> usize {
        if runs.is_empty() && !force_push {
            return next_offset;
        }

        let levels: Vec<unicode_bidi::Level> = runs.iter().map(|r| r.level).collect();
        let visual_indices = reorder_visual(&levels);

        let mut line = LayoutLine {
            range,
            break_suffix,
            ..Default::default()
        };

        for idx in visual_indices {
            let run = &mut runs[idx];
            for glyph in std::mem::take(&mut run.shaped.glyphs) {
                line.glyphs.push(glyph);
            }
        }

        // Use the exact glyph sequence measured for this line, including a
        // discretionary suffix and excluding invisible trailing whitespace.
        line.advance = if self.writing_mode.is_vertical() {
            line.glyphs
                .iter()
                .map(|glyph| glyph.y_advance)
                .sum::<f32>()
                .abs()
        } else {
            line.glyphs.iter().map(|glyph| glyph.x_advance).sum()
        };

        lines.push(line);
        next_offset
    }

    pub(crate) fn ink_bounds(
        &self,
        font_size: f32,
        lines: &[LayoutLine<'a>],
    ) -> Option<(f32, f32, f32, f32)> {
        let mut metrics_cache = HashMap::new();

        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;

        for line in lines {
            let (mut x, mut y) = line.baseline;
            for g in &line.glyphs {
                let key = font_key(g.font);
                let glyph_metrics = match metrics_cache.entry(key) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let Ok(font_ref) = g.font.skrifa_ref() else {
                            x += g.x_advance;
                            y -= g.y_advance;
                            continue;
                        };
                        entry
                            .insert(font_ref.glyph_metrics(Size::new(font_size), g.font.location()))
                    }
                };

                let gid = skrifa::GlyphId::new(g.glyph_id);
                if let Some(b) = glyph_metrics.bounds(gid) {
                    let x0 = x + g.x_offset + b.x_min;
                    let x1 = x + g.x_offset + b.x_max;
                    let synthetic_pad = g.font.synthetic_skew().map_or(0.0, |_| font_size * 0.25)
                        + if g.font.synthetic_bold() {
                            font_size * 0.05
                        } else {
                            0.0
                        };

                    // `b` is in a Y-up font coordinate system. Our layout coordinates are Y-down
                    // while screen-space Y grows downward, so we flip by subtracting.
                    let y0 = (y - g.y_offset) - b.y_max;
                    let y1 = (y - g.y_offset) - b.y_min;

                    min_x = min_x.min(x0 - synthetic_pad).min(x1 - synthetic_pad);
                    max_x = max_x.max(x0 + synthetic_pad).max(x1 + synthetic_pad);
                    min_y = min_y.min(y0).min(y1);
                    max_y = max_y.max(y0).max(y1);
                }

                x += g.x_advance;
                y -= g.y_advance;
            }
        }

        if min_x.is_finite() {
            Some((min_x, min_y, max_x, max_y))
        } else {
            None
        }
    }

    fn center_vertical_punctuation(
        &self,
        font_size: f32,
        segment: &str,
        glyphs: &mut [PositionedGlyph<'a>],
    ) {
        if segment.is_empty() || glyphs.is_empty() {
            return;
        }

        let mut metrics_cache = HashMap::new();
        let categories = CodePointMapData::<GeneralCategory>::new();
        for glyph in glyphs {
            let cluster = glyph.cluster as usize;
            let Some(ch) = segment.get(cluster..).and_then(|tail| tail.chars().next()) else {
                continue;
            };
            if !GeneralCategoryGroup::Punctuation.contains(categories.get(ch)) {
                continue;
            }

            let key = font_key(glyph.font);
            let glyph_metrics = match metrics_cache.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let Ok(font_ref) = glyph.font.skrifa_ref() else {
                        continue;
                    };
                    entry
                        .insert(font_ref.glyph_metrics(Size::new(font_size), glyph.font.location()))
                }
            };

            let gid = skrifa::GlyphId::new(glyph.glyph_id);
            let Some(bounds) = glyph_metrics.bounds(gid) else {
                continue;
            };
            glyph.x_offset = centered_x_offset(bounds.x_min, bounds.x_max);
            glyph.y_offset = glyph.y_advance * 0.5 - (bounds.y_min + bounds.y_max) * 0.5;
        }
    }

    fn layout_cjk_emphasis_runs(&self, font_size: f32, text: &str, shaped: &mut ShapedRun<'a>) {
        const GAP_EM: f32 = 0.04;

        let mut metrics_cache = HashMap::new();
        let mut start = 0;
        while start < shaped.glyphs.len() {
            let is_emphasis = |glyph: &PositionedGlyph<'_>| {
                text.get(glyph.cluster as usize..)
                    .and_then(|tail| tail.chars().next())
                    .and_then(cjk_emphasis_mark)
                    .is_some()
            };
            if !is_emphasis(&shaped.glyphs[start]) {
                start += 1;
                continue;
            }

            let mut end = start + 1;
            while end < shaped.glyphs.len() && end - start < 3 && is_emphasis(&shaped.glyphs[end]) {
                end += 1;
            }
            if end - start < 2 {
                start = end;
                continue;
            }

            let bounds = shaped.glyphs[start..end]
                .iter()
                .map(|glyph| {
                    let key = font_key(glyph.font);
                    let glyph_metrics = match metrics_cache.entry(key) {
                        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            let font_ref = glyph.font.skrifa_ref().ok()?;
                            entry.insert(
                                font_ref.glyph_metrics(Size::new(font_size), glyph.font.location()),
                            )
                        }
                    };
                    let bounds = glyph_metrics.bounds(skrifa::GlyphId::new(glyph.glyph_id))?;
                    Some((bounds.x_min, bounds.y_min, bounds.x_max, bounds.y_max))
                })
                .collect::<Option<Vec<_>>>();
            let Some(bounds) = bounds else {
                start = end;
                continue;
            };

            let gap = font_size * GAP_EM;
            if self.writing_mode.is_vertical() {
                let cell_advance = -shaped.glyphs[start..end]
                    .iter()
                    .map(|glyph| glyph.y_advance.abs())
                    .fold(font_size, f32::max);
                let group_width = bounds
                    .iter()
                    .map(|(x_min, _, x_max, _)| x_max - x_min)
                    .sum::<f32>()
                    + gap * (bounds.len() - 1) as f32;
                let mut cursor = -group_width * 0.5;
                let last = end - start - 1;
                for (index, (glyph, (x_min, y_min, x_max, y_max))) in
                    shaped.glyphs[start..end].iter_mut().zip(bounds).enumerate()
                {
                    glyph.x_offset = cursor - x_min;
                    glyph.y_offset = cell_advance * 0.5 - (y_min + y_max) * 0.5;
                    glyph.x_advance = 0.0;
                    glyph.y_advance = if index == last { cell_advance } else { 0.0 };
                    cursor += x_max - x_min + gap;
                }
            } else {
                let last = end - start - 1;
                for (index, (glyph, (x_min, _, x_max, _))) in
                    shaped.glyphs[start..end].iter_mut().zip(bounds).enumerate()
                {
                    glyph.x_offset = -x_min;
                    glyph.x_advance = if index == last {
                        extend_advance(x_max - x_min, self.letter_spacing)
                    } else {
                        x_max - x_min + gap
                    };
                }
            }

            start = end;
        }

        shaped.x_advance = shaped.glyphs.iter().map(|glyph| glyph.x_advance).sum();
        shaped.y_advance = shaped.glyphs.iter().map(|glyph| glyph.y_advance).sum();
    }

    fn apply_spacing(&self, text: &str, shaped: &mut ShapedRun<'a>) {
        if self.letter_spacing == 0.0 && self.word_spacing == 0.0 {
            return;
        }
        for glyph in &mut shaped.glyphs {
            let character = text
                .get(glyph.cluster as usize..)
                .and_then(|tail| tail.chars().next());
            let extra = self.letter_spacing
                + character
                    .filter(|character| character.is_whitespace())
                    .map_or(0.0, |_| self.word_spacing);
            if self.writing_mode.is_vertical() {
                glyph.y_advance = extend_advance(glyph.y_advance, extra);
            } else {
                glyph.x_advance = extend_advance(glyph.x_advance, extra);
            }
        }
        shaped.x_advance = shaped.glyphs.iter().map(|glyph| glyph.x_advance).sum();
        shaped.y_advance = shaped.glyphs.iter().map(|glyph| glyph.y_advance).sum();
    }
}

fn alphanumeric_word_count(text: &str) -> usize {
    text.split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .count()
}

fn comic_hyphen_quality(text: &str, layout: &LayoutRun<'_>) -> ComicHyphenQuality {
    let total = layout.discretionary_hyphen_count();
    let compact = layout
        .lines
        .windows(2)
        .filter(|pair| {
            if pair[0].break_suffix != Some(LineBreakSuffix::Hyphen) {
                return false;
            }
            let before = text[pair[0].range.clone()]
                .chars()
                .rev()
                .take_while(|character| character.is_alphabetic())
                .count();
            let after = text[pair[1].range.clone()]
                .chars()
                .take_while(|character| character.is_alphabetic())
                .count();
            before.min(after) <= COMPACT_HYPHENATION_FRAGMENT_LEN
        })
        .count();
    ComicHyphenQuality { compact, total }
}

fn extend_advance(advance: f32, extra: f32) -> f32 {
    if advance < 0.0 {
        advance - extra
    } else {
        advance + extra
    }
}

fn justify_lines(
    text: &str,
    lines: &mut [LayoutLine<'_>],
    max_width: f32,
    profiles: &[LineProfile],
    ink_outset: f32,
) {
    if profiles.is_empty() && (!max_width.is_finite() || max_width <= 0.0) {
        return;
    }
    let last = lines.len().saturating_sub(1);
    for (index, line) in lines.iter_mut().enumerate() {
        if index == last
            || text
                .get(line.range.end..)
                .and_then(|tail| tail.chars().next())
                .is_some_and(|character| {
                    matches!(
                        character,
                        '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}'
                    )
                })
        {
            continue;
        }
        let target_width = profiles
            .get(index)
            .map_or(max_width, |profile| profile.width)
            - ink_outset * 2.0;
        if !target_width.is_finite() || target_width <= 0.0 {
            continue;
        }
        let is_space = |glyph: &PositionedGlyph<'_>| {
            text.get(glyph.cluster as usize..)
                .and_then(|tail| tail.chars().next())
                .is_some_and(char::is_whitespace)
        };
        let spaces = line.glyphs.iter().filter(|glyph| is_space(glyph)).count();
        if spaces == 0 || line.advance >= target_width {
            continue;
        }
        let extra = (target_width - line.advance) / spaces as f32;
        for glyph in &mut line.glyphs {
            if is_space(glyph) {
                glyph.x_advance = extend_advance(glyph.x_advance, extra);
            }
        }
        line.advance = target_width;
    }
}

fn line_breaks_with_policy(
    segments: &LineMeasurements,
    max_extent: f32,
    policy: HyphenationPolicy,
) -> LineBreakResult {
    if policy == HyphenationPolicy::LastResort {
        let without_hyphens = optimal_uniform_line_breaks(segments, max_extent, false);
        if !without_hyphens.overflowed {
            return without_hyphens;
        }
    }
    optimal_uniform_line_breaks(segments, max_extent, policy != HyphenationPolicy::Disabled)
}

fn optimal_uniform_line_breaks(
    segments: &LineMeasurements,
    max_extent: f32,
    allow_hyphenation: bool,
) -> LineBreakResult {
    let len = segments.len();
    if len == 0 {
        return LineBreakResult {
            breaks: Vec::new(),
            profiles: Vec::new(),
            overflowed: false,
            contour_profiled: false,
            cost: 0.0,
        };
    }
    if !max_extent.is_finite() || max_extent <= 0.0 {
        return LineBreakResult {
            breaks: vec![len],
            profiles: vec![LineProfile {
                width: max_extent,
                center_offset: 0.0,
                block_baseline: 0.0,
            }],
            overflowed: false,
            contour_profiled: false,
            cost: 0.0,
        };
    }

    // Overflow is a feasibility constraint, not an aesthetic tradeoff. A finite
    // badness can otherwise prefer one slightly overfull line to several ragged
    // legal lines, which makes auto-fit shrink despite an available layout.
    uniform_line_breaks_pass(segments, max_extent, allow_hyphenation, false)
        .or_else(|| uniform_line_breaks_pass(segments, max_extent, allow_hyphenation, true))
        .unwrap_or_else(|| LineBreakResult {
            breaks: vec![len],
            profiles: vec![LineProfile {
                width: max_extent,
                center_offset: 0.0,
                block_baseline: 0.0,
            }],
            overflowed: true,
            contour_profiled: false,
            cost: f32::INFINITY,
        })
}

const LINE_BREAK_FITNESS_CLASSES: usize = 4;
const LINE_BREAK_ROOT_FITNESS: usize = LINE_BREAK_FITNESS_CLASSES;
const LINE_BREAK_FITNESS_PENALTY: f32 = 300.0;
const LINE_BREAK_RUNT_PENALTY: f32 = 500.0;
const LINE_BREAK_REPEATED_HYPHEN_PENALTY: f32 = 1_000.0;

#[derive(Clone, Copy)]
struct BreakState {
    cost: f32,
    previous: Option<(usize, usize)>,
}

impl BreakState {
    const EMPTY: Self = Self {
        cost: f32::INFINITY,
        previous: None,
    };
}

fn line_fitness(advance: f32, width: f32) -> usize {
    ((advance / width).clamp(0.0, 1.0) * LINE_BREAK_FITNESS_CLASSES as f32)
        .floor()
        .min((LINE_BREAK_FITNESS_CLASSES - 1) as f32) as usize
}

fn paragraph_transition_cost(
    segments: &LineMeasurements,
    range: Range<usize>,
    ratio: f32,
    previous_fitness: usize,
    fitness: usize,
    hyphenated: bool,
) -> f32 {
    let follows_mandatory = range.start > 0 && segments[range.start - 1].is_mandatory;
    let mut cost = 0.0;
    if previous_fitness != LINE_BREAK_ROOT_FITNESS && !follows_mandatory {
        let jump = previous_fitness.abs_diff(fitness).saturating_sub(1) as f32;
        cost += jump * jump * LINE_BREAK_FITNESS_PENALTY;
        if range.end == segments.len() {
            let shortfall = ((0.5 - ratio) / 0.5).max(0.0);
            cost += shortfall * shortfall * LINE_BREAK_RUNT_PENALTY;
        }
        if hyphenated && segments[range.start - 1].hyphenated {
            cost += LINE_BREAK_REPEATED_HYPHEN_PENALTY;
        }
    }
    cost
}

fn uniform_line_breaks_pass(
    segments: &LineMeasurements,
    max_extent: f32,
    allow_hyphenation: bool,
    allow_overflow: bool,
) -> Option<LineBreakResult> {
    let len = segments.len();
    // Keep a frontier for the previous line's fullness: the cheapest prefix is
    // not necessarily the best predecessor for the next line's shape.
    let mut states = vec![[BreakState::EMPTY; LINE_BREAK_FITNESS_CLASSES + 1]; len + 1];
    states[0][LINE_BREAK_ROOT_FITNESS].cost = 0.0;

    for start in 0..len {
        let predecessors = states[start];
        if !predecessors.iter().any(|state| state.cost.is_finite()) {
            continue;
        }
        for end in start + 1..=len {
            let line_advance = segments.advance(start, end);
            let hyphenated = end < len && segments[end - 1].hyphenated;
            let stop = segments[end - 1].is_mandatory;
            let overflowed = line_advance > max_extent + f32::EPSILON;
            if (!hyphenated || allow_hyphenation) && (!overflowed || allow_overflow) {
                let fitness = line_fitness(line_advance, max_extent);
                let mut line_cost = line_break_badness(line_advance, max_extent);
                if hyphenated {
                    line_cost += LINE_BREAK_HYPHEN_PENALTY;
                }
                if end < len {
                    line_cost += segments[end - 1].break_penalty;
                }
                for (previous_fitness, previous) in predecessors.iter().enumerate() {
                    if !previous.cost.is_finite() {
                        continue;
                    }
                    let cost = previous.cost
                        + line_cost
                        + paragraph_transition_cost(
                            segments,
                            start..end,
                            line_advance / max_extent,
                            previous_fitness,
                            fitness,
                            hyphenated,
                        );
                    if cost < states[end][fitness].cost {
                        states[end][fitness] = BreakState {
                            cost,
                            previous: Some((start, previous_fitness)),
                        };
                    }
                }
            }
            if stop {
                break;
            }
        }
    }

    let (mut fitness, selected) = states[len]
        .iter()
        .enumerate()
        .filter(|(_, state)| state.cost.is_finite())
        .min_by(|(_, left), (_, right)| {
            left.cost
                .total_cmp(&right.cost)
                .then_with(|| left.previous.cmp(&right.previous))
        })?;
    let cost = selected.cost;
    let mut breaks = Vec::new();
    let mut end = len;
    while end > 0 {
        breaks.push(end);
        (end, fitness) = states[end][fitness].previous?;
    }
    breaks.reverse();
    let overflowed = breaks_overflow(segments, &breaks, &[max_extent; 1], f32::EPSILON);
    let profiles = vec![
        LineProfile {
            width: max_extent,
            center_offset: 0.0,
            block_baseline: 0.0,
        };
        breaks.len()
    ];
    debug_assert!(allow_overflow || !overflowed);
    Some(LineBreakResult {
        breaks,
        profiles,
        overflowed,
        contour_profiled: false,
        cost,
    })
}

fn comic_line_breaks(
    segments: &LineMeasurements,
    balloon: &ComicBalloon,
    writing_mode: WritingMode,
    line_height: f32,
    line_ink: InkBand,
    air: (f32, f32),
    policy: HyphenationPolicy,
) -> LineBreakResult {
    let (air_x, air_y) = air;
    let block_extent = if writing_mode.is_vertical() {
        balloon.width
    } else {
        balloon.height
    };
    let block_air = if writing_mode.is_vertical() {
        air_x
    } else {
        air_y
    };
    let preferred_center = balloon.block_center(writing_mode, block_extent);
    let available_block_extent = (block_extent - block_air * 2.0).max(0.0);
    let maximum_lines = if line_ink.thickness() <= available_block_extent {
        1 + ((available_block_extent - line_ink.thickness()) / line_height).floor() as usize
    } else {
        0
    }
    .min(COMIC_MAX_LINES)
    .min(segments.len());
    if maximum_lines == 0 {
        let mut fallback = line_breaks_with_policy(
            segments,
            balloon.inline_extent(writing_mode, air_x, air_y),
            policy,
        );
        fallback.overflowed = true;
        return fallback;
    }

    let select = |allow_hyphenation: bool| {
        let mut candidates = (1..=maximum_lines)
            .flat_map(|line_count| {
                profiled_comic_line_break_candidates(
                    segments,
                    balloon,
                    writing_mode,
                    line_count,
                    line_height,
                    line_ink,
                    air_x,
                    air_y,
                    allow_hyphenation,
                )
            })
            .collect::<Vec<_>>();
        if candidates.iter().any(|candidate| !candidate.overflowed) {
            candidates.retain(|candidate| !candidate.overflowed);
        }
        if let Some(preferred) = balloon.preferred_line_count
            && let Some(minimum_penalty) = candidates
                .iter()
                .map(|candidate| candidate.profiles.len().abs_diff(preferred))
                .min()
        {
            candidates.retain(|candidate| {
                candidate.profiles.len().abs_diff(preferred) == minimum_penalty
            });
        }
        let best_center_distance = candidates
            .iter()
            .map(|candidate| {
                block_center_distance(candidate, line_height, line_ink, preferred_center)
            })
            .min_by(f32::total_cmp)?;
        candidates
            .into_iter()
            .filter(|candidate| {
                block_center_distance(candidate, line_height, line_ink, preferred_center)
                    <= best_center_distance + COMIC_CENTER_RASTER_TOLERANCE
            })
            .min_by(|left, right| {
                left.cost
                    .total_cmp(&right.cost)
                    .then_with(|| {
                        block_center_distance(left, line_height, line_ink, preferred_center)
                            .total_cmp(&block_center_distance(
                                right,
                                line_height,
                                line_ink,
                                preferred_center,
                            ))
                    })
                    // Break cost already accounts for the number of lines. Keep the
                    // explicit comparison only as a deterministic tie-breaker.
                    .then_with(|| left.profiles.len().cmp(&right.profiles.len()))
                    .then_with(|| {
                        minimum_line_clearance(segments, right)
                            .total_cmp(&minimum_line_clearance(segments, left))
                    })
            })
    };

    if policy == HyphenationPolicy::LastResort
        && let Some(without_hyphens) = select(false)
        && !without_hyphens.overflowed
    {
        return without_hyphens;
    }

    let allow_hyphenation = policy != HyphenationPolicy::Disabled;
    let selected = select(allow_hyphenation).unwrap_or_else(|| {
        let mut fallback = line_breaks_with_policy(
            segments,
            balloon.inline_extent(writing_mode, air_x, air_y),
            policy,
        );
        fallback.overflowed = true;
        fallback
    });

    #[cfg(debug_assertions)]
    if selected.overflowed
        && tracing::enabled!(
            target: "koharu_typesetting_probe",
            tracing::Level::TRACE
        )
    {
        let profile_trace = (1..=maximum_lines)
            .map(|line_count| {
                let candidates = profiled_comic_line_break_candidates(
                    segments,
                    balloon,
                    writing_mode,
                    line_count,
                    line_height,
                    line_ink,
                    air_x,
                    air_y,
                    allow_hyphenation,
                );
                let fitted_count = candidates
                    .iter()
                    .filter(|candidate| !candidate.overflowed)
                    .count();
                let best = candidates.iter().max_by(|left, right| {
                    minimum_line_clearance(segments, left)
                        .total_cmp(&minimum_line_clearance(segments, right))
                });
                (
                    line_count,
                    candidates.len(),
                    fitted_count,
                    best.map(|candidate| minimum_line_clearance(segments, candidate)),
                    best.map(|candidate| candidate.breaks.clone()),
                    best.map(|candidate| {
                        candidate
                            .profiles
                            .iter()
                            .map(|profile| profile.width)
                            .collect::<Vec<_>>()
                    }),
                )
            })
            .collect::<Vec<_>>();
        let segment_trace = segments
            .iter()
            .map(|segment| {
                (
                    segment.hyphenated,
                    segment.break_penalty,
                    segment.is_mandatory,
                )
            })
            .collect::<Vec<_>>();
        tracing::trace!(
            target: "koharu_typesetting_probe",
            marker = "comic_composition_miss",
            writing_mode = ?writing_mode,
            line_height,
            line_ink_before = line_ink.before,
            line_ink_after = line_ink.after,
            air_x,
            air_y,
            maximum_lines,
            allow_hyphenation,
            ?segment_trace,
            ?profile_trace,
        );
    }

    selected
}

#[allow(clippy::too_many_arguments)]
fn profiled_comic_line_break_candidates(
    segments: &LineMeasurements,
    balloon: &ComicBalloon,
    writing_mode: WritingMode,
    line_count: usize,
    line_height: f32,
    line_ink: InkBand,
    air_x: f32,
    air_y: f32,
    allow_hyphenation: bool,
) -> Vec<LineBreakResult> {
    let origins = balloon.line_profile_origins(
        writing_mode,
        line_count,
        line_height,
        line_ink,
        air_x,
        air_y,
    );
    if origins.is_empty() {
        return Vec::new();
    }

    let vertical = writing_mode.is_vertical();
    let (block_extent, inline_extent, block_air, inline_air) = if vertical {
        (balloon.width, balloon.height, air_x, air_y)
    } else {
        (balloon.height, balloon.width, air_y, air_x)
    };
    let block_size = line_ink.thickness() + line_count.saturating_sub(1) as f32 * line_height;
    let preferred_center = balloon.block_center(writing_mode, block_extent);
    let Some((first_origin, last_origin)) =
        balloon.block_origin_range(writing_mode, block_extent, block_air, block_size)
    else {
        return Vec::new();
    };
    let preferred_origin = (preferred_center - block_size * 0.5).clamp(first_origin, last_origin);

    let evaluate = |block_origin| {
        balloon
            .line_profiles_at_origin(
                writing_mode,
                line_count,
                line_height,
                line_ink,
                block_extent,
                inline_extent,
                block_air,
                inline_air,
                block_origin,
            )
            .and_then(|profiles| exact_profiled_line_breaks(segments, profiles, allow_hyphenation))
    };

    let mut sampled = origins
        .into_iter()
        .map(|origin| (origin, evaluate(origin)))
        .collect::<Vec<_>>();
    sampled.sort_by(|left, right| left.0.total_cmp(&right.0));
    sampled.dedup_by(|left, right| (left.0 - right.0).abs() <= f32::EPSILON);

    let mut brackets = Vec::with_capacity(2);
    let mut inner_miss = None;
    for (origin, result) in sampled
        .iter()
        .rev()
        .filter(|(origin, _)| *origin <= preferred_origin + f32::EPSILON)
    {
        if result.as_ref().is_some_and(|result| !result.overflowed) {
            if let Some(miss) = inner_miss {
                brackets.push((miss, *origin));
            }
            break;
        }
        inner_miss = Some(*origin);
    }
    inner_miss = None;
    for (origin, result) in sampled
        .iter()
        .filter(|(origin, _)| *origin + f32::EPSILON >= preferred_origin)
    {
        if result.as_ref().is_some_and(|result| !result.overflowed) {
            if let Some(miss) = inner_miss {
                brackets.push((miss, *origin));
            }
            break;
        }
        inner_miss = Some(*origin);
    }

    let mut candidates = sampled
        .into_iter()
        .filter_map(|(_, result)| result)
        .collect::<Vec<_>>();
    for (mut miss, mut fit) in brackets {
        let mut refined = None;
        for _ in 0..COMIC_BLOCK_REFINEMENT_ITERATIONS {
            if (fit - miss).abs() <= COMIC_BLOCK_REFINEMENT_TOLERANCE {
                break;
            }
            let midpoint = (miss + fit) * 0.5;
            match evaluate(midpoint) {
                Some(result) if !result.overflowed => {
                    fit = midpoint;
                    refined = Some(result);
                }
                _ => miss = midpoint,
            }
        }
        if let Some(result) = refined {
            candidates.push(result);
        }
    }
    candidates
}

fn exact_profiled_line_breaks(
    segments: &LineMeasurements,
    profiles: Vec<LineProfile>,
    allow_hyphenation: bool,
) -> Option<LineBreakResult> {
    // Preserve the same hard-fit contract as the uniform breaker. The fallback
    // pass still produces a useful least-bad layout when the profile cannot fit.
    exact_profiled_line_breaks_pass(segments, &profiles, allow_hyphenation, false)
        .or_else(|| exact_profiled_line_breaks_pass(segments, &profiles, allow_hyphenation, true))
}

fn exact_profiled_line_breaks_pass(
    segments: &LineMeasurements,
    profiles: &[LineProfile],
    allow_hyphenation: bool,
    allow_overflow: bool,
) -> Option<LineBreakResult> {
    let len = segments.len();
    let line_count = profiles.len();
    if line_count == 0 || line_count > len {
        return None;
    }
    let mut states =
        vec![vec![[BreakState::EMPTY; LINE_BREAK_FITNESS_CLASSES + 1]; len + 1]; line_count + 1];
    states[0][0][LINE_BREAK_ROOT_FITNESS].cost = 0.0;

    for line in 0..line_count {
        let remaining_lines = line_count - line - 1;
        for start in line..len {
            let predecessors = states[line][start];
            if !predecessors.iter().any(|state| state.cost.is_finite()) {
                continue;
            }
            let last_end = len - remaining_lines;
            for end in start + 1..=last_end {
                let hyphenated = end < len && segments[end - 1].hyphenated;
                let line_advance = segments.advance(start, end);
                let width = profiles[line].width.max(1.0);
                let overflowed = line_advance > width + COMIC_LINE_RASTER_TOLERANCE;
                let stop = segments[end - 1].is_mandatory;
                if (!hyphenated || allow_hyphenation) && (!overflowed || allow_overflow) {
                    let slack = (width - line_advance).max(0.0) / width;
                    let overflow = (line_advance - width).max(0.0) / width;
                    let fitness = line_fitness(line_advance, width);
                    let mut line_cost = slack * slack * LINE_BREAK_RAGGEDNESS_PENALTY
                        + overflow * overflow * COMIC_LINE_OVERFLOW_PENALTY;
                    if hyphenated {
                        line_cost += LINE_BREAK_HYPHEN_PENALTY;
                    }
                    if end < len {
                        line_cost += segments[end - 1].break_penalty;
                    }
                    for (previous_fitness, previous) in predecessors.iter().enumerate() {
                        if !previous.cost.is_finite() {
                            continue;
                        }
                        let cost = previous.cost
                            + line_cost
                            + paragraph_transition_cost(
                                segments,
                                start..end,
                                line_advance / width,
                                previous_fitness,
                                fitness,
                                hyphenated,
                            );
                        if cost < states[line + 1][end][fitness].cost {
                            states[line + 1][end][fitness] = BreakState {
                                cost,
                                previous: Some((start, previous_fitness)),
                            };
                        }
                    }
                }
                if stop {
                    break;
                }
            }
        }
    }

    let (mut fitness, selected) = states[line_count][len]
        .iter()
        .enumerate()
        .filter(|(_, state)| state.cost.is_finite())
        .min_by(|(_, left), (_, right)| {
            left.cost
                .total_cmp(&right.cost)
                .then_with(|| left.previous.cmp(&right.previous))
        })?;
    let cost = selected.cost / line_count as f32 + line_count as f32 * 8.0;
    let mut breaks = Vec::with_capacity(line_count);
    let mut end = len;
    for line in (1..=line_count).rev() {
        breaks.push(end);
        (end, fitness) = states[line][end][fitness].previous?;
    }
    if end != 0 {
        return None;
    }
    breaks.reverse();
    let widths = profiles
        .iter()
        .map(|profile| profile.width)
        .collect::<Vec<_>>();
    let overflowed = breaks_overflow(segments, &breaks, &widths, COMIC_LINE_RASTER_TOLERANCE);
    debug_assert!(allow_overflow || !overflowed);
    Some(LineBreakResult {
        breaks,
        profiles: profiles.to_vec(),
        overflowed,
        contour_profiled: true,
        cost,
    })
}

fn breaks_overflow(
    segments: &LineMeasurements,
    breaks: &[usize],
    widths: &[f32],
    tolerance: f32,
) -> bool {
    let mut start = 0usize;
    for (line, end) in breaks.iter().copied().enumerate() {
        let advance = segments.advance(start, end);
        let Some(width) = widths
            .get(line)
            .copied()
            .or_else(|| widths.first().copied())
        else {
            return true;
        };
        if advance > width + tolerance {
            return true;
        }
        start = end;
    }
    false
}

fn minimum_line_clearance(segments: &LineMeasurements, result: &LineBreakResult) -> f32 {
    let mut start = 0usize;
    let mut clearance = f32::INFINITY;
    for (line, end) in result.breaks.iter().copied().enumerate() {
        if end <= start || end > segments.len() {
            return f32::NEG_INFINITY;
        }
        let advance = segments.advance(start, end);
        let Some(width) = result.profiles.get(line).map(|profile| profile.width) else {
            return f32::NEG_INFINITY;
        };
        clearance = clearance.min(width - advance);
        start = end;
    }
    if start == segments.len() {
        clearance
    } else {
        f32::NEG_INFINITY
    }
}

fn block_center_distance(
    result: &LineBreakResult,
    line_height: f32,
    line_ink: InkBand,
    preferred_center: f32,
) -> f32 {
    let Some(origin) = result
        .profiles
        .iter()
        .map(|profile| profile.block_baseline - line_ink.before)
        .min_by(f32::total_cmp)
    else {
        return f32::INFINITY;
    };
    let block_size =
        line_ink.thickness() + result.profiles.len().saturating_sub(1) as f32 * line_height;
    (origin + block_size * 0.5 - preferred_center).abs()
}

impl ComicBalloon {
    fn air(&self, representative_extent: f32) -> f32 {
        self.minimum_air.max(representative_extent)
    }

    fn inline_extent(&self, writing_mode: WritingMode, air_x: f32, air_y: f32) -> f32 {
        if writing_mode.is_vertical() {
            (self.height - air_y * 2.0).max(1.0)
        } else {
            (self.width - air_x * 2.0).max(1.0)
        }
    }

    #[cfg(test)]
    fn line_profiles(
        &self,
        writing_mode: WritingMode,
        line_count: usize,
        line_height: f32,
        line_ink: InkBand,
        air_x: f32,
        air_y: f32,
    ) -> Option<Vec<LineProfile>> {
        self.line_profile_candidates(
            writing_mode,
            line_count,
            line_height,
            line_ink,
            air_x,
            air_y,
        )
        .into_iter()
        .next()
    }

    #[cfg(test)]
    fn line_profile_candidates(
        &self,
        writing_mode: WritingMode,
        line_count: usize,
        line_height: f32,
        line_ink: InkBand,
        air_x: f32,
        air_y: f32,
    ) -> Vec<Vec<LineProfile>> {
        let origins = self.line_profile_origins(
            writing_mode,
            line_count,
            line_height,
            line_ink,
            air_x,
            air_y,
        );
        if origins.is_empty() {
            return Vec::new();
        }
        let vertical = writing_mode.is_vertical();
        let (block_extent, inline_extent, block_air, inline_air) = if vertical {
            (self.width, self.height, air_x, air_y)
        } else {
            (self.height, self.width, air_y, air_x)
        };

        origins
            .into_iter()
            .filter_map(|block_origin| {
                self.line_profiles_at_origin(
                    writing_mode,
                    line_count,
                    line_height,
                    line_ink,
                    block_extent,
                    inline_extent,
                    block_air,
                    inline_air,
                    block_origin,
                )
            })
            .collect()
    }

    fn line_profile_origins(
        &self,
        writing_mode: WritingMode,
        line_count: usize,
        line_height: f32,
        line_ink: InkBand,
        air_x: f32,
        air_y: f32,
    ) -> Vec<f32> {
        let vertical = writing_mode.is_vertical();
        let (block_extent, inline_extent, block_air, inline_air) = if vertical {
            (self.width, self.height, air_x, air_y)
        } else {
            (self.height, self.width, air_y, air_x)
        };
        let inline_radius = inline_extent * 0.5 - inline_air;
        let block_size = line_ink.thickness() + line_count.saturating_sub(1) as f32 * line_height;
        if line_count == 0 || inline_radius <= 0.0 {
            return Vec::new();
        }
        let Some((first_origin, last_origin)) =
            self.block_origin_range(writing_mode, block_extent, block_air, block_size)
        else {
            return Vec::new();
        };

        // Font size, line breaks, and block position are coupled inside an irregular
        // balloon. Seed the exact area-weighted body center, then search the remaining
        // feasible interval so a centered candidate that overflows can still move.
        let preferred_center = self.block_center(writing_mode, block_extent);
        let preferred_origin =
            (preferred_center - block_size * 0.5).clamp(first_origin, last_origin);
        let travel = (last_origin - first_origin).max(0.0);
        let target_step = (line_ink.thickness().min(line_height) * 0.5).max(1.0);
        let intervals = if travel <= f32::EPSILON {
            0
        } else {
            ((travel / target_step).ceil() as usize).clamp(1, COMIC_BLOCK_SEARCH_MAX_INTERVALS)
        };
        let mut origins = Vec::with_capacity(intervals.saturating_add(2));
        origins.push(preferred_origin);
        if intervals > 0 {
            for index in 0..=intervals {
                let origin = first_origin + travel * index as f32 / intervals as f32;
                if origins
                    .iter()
                    .all(|candidate| (candidate - origin).abs() > f32::EPSILON)
                {
                    origins.push(origin);
                }
            }
        }

        origins
    }

    #[allow(clippy::too_many_arguments)]
    fn line_profiles_at_origin(
        &self,
        writing_mode: WritingMode,
        line_count: usize,
        line_height: f32,
        line_ink: InkBand,
        block_extent: f32,
        inline_extent: f32,
        block_air: f32,
        inline_air: f32,
        block_origin: f32,
    ) -> Option<Vec<LineProfile>> {
        let inline_center = inline_extent * 0.5;
        let mut spans = Vec::with_capacity(line_count);
        for line in 0..line_count {
            let block_index = if writing_mode == WritingMode::VerticalRl {
                line_count - line - 1
            } else {
                line
            };
            let baseline = block_origin + line_ink.before + block_index as f32 * line_height;
            let band_start = baseline - line_ink.before;
            let mut left = f32::NEG_INFINITY;
            let mut right = f32::INFINITY;
            // Intersect the contour across the glyph ink band. Leading belongs between
            // baselines and must not consume inline space at a balloon's tapered ends.
            for sample in 0..=4 {
                let block = band_start + line_ink.thickness() * sample as f32 / 4.0;
                let (sample_left, sample_right) = self.inline_span(
                    writing_mode,
                    block,
                    block_extent,
                    inline_extent,
                    block_air,
                    inline_air,
                )?;
                left = left.max(sample_left);
                right = right.min(sample_right);
            }
            if right <= left {
                return None;
            }
            spans.push((left, right, baseline));
        }

        // A phrase has one visual axis. The contour controls the usable width of
        // each line, while the common center keeps the setting typographically
        // coherent instead of producing a zig-zag paragraph.
        let common_left = spans
            .iter()
            .map(|(left, _, _)| *left)
            .fold(f32::NEG_INFINITY, f32::max);
        let common_right = spans
            .iter()
            .map(|(_, right, _)| *right)
            .fold(f32::INFINITY, f32::min);
        if common_right <= common_left {
            return None;
        }
        // A short line near an asymmetric wall must not drag every line toward
        // that wall. Prefer the body's axis; retain a feasible shared axis when
        // this block position cannot contain it at all.
        let center = self.center();
        let preferred_axis = if writing_mode.is_vertical() {
            center.1
        } else {
            center.0
        };
        let shared_center = if preferred_axis > common_left && preferred_axis < common_right {
            preferred_axis
        } else {
            (common_left + common_right) * 0.5
        };
        spans
            .into_iter()
            .map(|(left, right, block_baseline)| {
                let half_width = (shared_center - left).min(right - shared_center);
                (half_width > 0.0).then_some(LineProfile {
                    width: half_width * 2.0,
                    center_offset: shared_center - inline_center,
                    block_baseline,
                })
            })
            .collect()
    }

    fn block_origin_range(
        &self,
        writing_mode: WritingMode,
        block_extent: f32,
        block_air: f32,
        block_size: f32,
    ) -> Option<(f32, f32)> {
        let mut first = block_air;
        let mut last = block_extent - block_air;
        for contour in self
            .contours
            .iter()
            .filter(|contour| contour.points.len() >= 3)
        {
            let air = block_air * contour.air_scale;
            let (minimum, maximum) = contour.points.iter().fold(
                (f32::INFINITY, f32::NEG_INFINITY),
                |(minimum, maximum), &(x, y)| {
                    let block = if writing_mode.is_vertical() { x } else { y };
                    (minimum.min(block), maximum.max(block))
                },
            );
            first = first.max(minimum + air);
            last = last.min(maximum - air);
        }
        let last_origin = last - block_size;
        (last_origin + f32::EPSILON >= first).then_some((first, last_origin.max(first)))
    }

    fn center(&self) -> (f32, f32) {
        self.preferred_center
            .or_else(|| {
                intersected_centroid(
                    &self
                        .contours
                        .iter()
                        .map(|contour| contour.points.as_slice())
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or((self.width * 0.5, self.height * 0.5))
    }

    fn block_center(&self, writing_mode: WritingMode, block_extent: f32) -> f32 {
        let center = self.center();
        (if writing_mode.is_vertical() {
            center.0
        } else {
            center.1
        })
        .clamp(0.0, block_extent)
    }

    #[allow(clippy::too_many_arguments)]
    fn inline_span(
        &self,
        writing_mode: WritingMode,
        block: f32,
        block_extent: f32,
        inline_extent: f32,
        block_air: f32,
        inline_air: f32,
    ) -> Option<(f32, f32)> {
        if self
            .contours
            .iter()
            .any(|contour| contour.points.len() >= 3)
        {
            return self.contour_inline_span(writing_mode, block, inline_air);
        }

        let block_radius = block_extent * 0.5 - block_air;
        let inline_radius = inline_extent * 0.5 - inline_air;
        let normalized_block = ((block - block_extent * 0.5) / block_radius).clamp(-1.0, 1.0);
        let half_extent = inline_radius * (1.0 - normalized_block * normalized_block).sqrt();
        (half_extent > 0.0).then_some((
            inline_extent * 0.5 - half_extent,
            inline_extent * 0.5 + half_extent,
        ))
    }

    fn contour_inline_span(
        &self,
        writing_mode: WritingMode,
        block: f32,
        inline_air: f32,
    ) -> Option<(f32, f32)> {
        let mut constraints = self
            .contours
            .iter()
            .filter(|contour| contour.points.len() >= 3)
            .map(|contour| {
                let air = inline_air * contour.air_scale;
                polygon_spans(&contour.points, writing_mode.is_vertical(), block)
                    .into_iter()
                    .filter_map(|(left, right)| {
                        let span = (left + air, right - air);
                        (span.1 > span.0).then_some(span)
                    })
                    .collect::<Vec<_>>()
            });
        let mut spans = constraints.next()?;
        for constraint in constraints {
            spans = spans
                .iter()
                .flat_map(|&(left, right)| {
                    constraint
                        .iter()
                        .filter_map(move |&(other_left, other_right)| {
                            let intersection = (left.max(other_left), right.min(other_right));
                            (intersection.1 > intersection.0).then_some(intersection)
                        })
                })
                .collect();
            if spans.is_empty() {
                return None;
            }
        }
        spans
            .into_iter()
            .max_by(|left, right| (left.1 - left.0).total_cmp(&(right.1 - right.0)))
    }
}

fn is_natural_pause_character(character: char) -> bool {
    matches!(
        character,
        '.' | '!'
            | '?'
            | '…'
            | '‼'
            | '⁇'
            | '⁈'
            | '⁉'
            | ','
            | ';'
            | ':'
            | '—'
            | '–'
            | '~'
            | '〜'
            | '～'
    )
}

pub(crate) fn has_internal_natural_pause(text: &str) -> bool {
    text.char_indices().any(|(offset, character)| {
        if !is_natural_pause_character(character) {
            return false;
        }
        !text[offset + character.len_utf8()..]
            .trim_start_matches(is_natural_pause_character)
            .trim_start()
            .is_empty()
    })
}

fn natural_pause_at(text: &str, boundary: usize) -> bool {
    let boundary = boundary.min(text.len());
    text[..boundary]
        .trim_end()
        .chars()
        .next_back()
        .is_some_and(is_natural_pause_character)
}

fn natural_break_penalty(text: &str, boundary: usize) -> f32 {
    let boundary = boundary.min(text.len());
    let before = text[..boundary].trim_end();
    let after = text[boundary..].trim_start();
    match before.chars().next_back() {
        Some('.' | '!' | '?' | '…' | '‼' | '⁇' | '⁈' | '⁉' | '~' | '〜' | '～') => {
            return 0.0;
        }
        Some(',' | ';' | ':' | '—' | '–') => return 20.0,
        _ => {}
    }
    let next_word = after
        .split(|character: char| !character.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    if matches!(
        next_word.to_ascii_lowercase().as_str(),
        "and" | "but" | "or" | "so" | "because" | "although" | "while" | "then"
    ) {
        return 40.0;
    }
    let previous_word = before
        .rsplit(|character: char| !character.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    if matches!(
        previous_word.to_ascii_lowercase().as_str(),
        "a" | "an" | "the" | "to" | "of" | "for" | "in" | "on" | "at" | "with" | "from"
    ) {
        300.0
    } else {
        100.0
    }
}

fn line_break_badness(line_advance: f32, max_extent: f32) -> f32 {
    // Break penalties are dimensionless. Pixel-space slack changes their relative
    // weight when the same text and container are rendered at another resolution.
    let difference = (max_extent - line_advance) / max_extent;
    if line_advance <= max_extent {
        difference.powi(3) * LINE_BREAK_RAGGEDNESS_PENALTY
    } else {
        (-difference).powi(3) * LINE_BREAK_RAGGEDNESS_PENALTY * LINE_BREAK_OVERFLOW_MULTIPLIER
    }
}

fn centered_x_offset(x_min: f32, x_max: f32) -> f32 {
    -((x_min + x_max) * 0.5)
}

fn cjk_emphasis_mark(character: char) -> Option<char> {
    let mut normalized =
        ComposingNormalizerBorrowed::new_nfkc().normalize_iter(std::iter::once(character));
    let character = normalized.next()?;
    if normalized.next().is_some() {
        return None;
    }
    match character {
        '!' => Some('！'),
        '?' => Some('？'),
        _ => None,
    }
}

fn normalize_cjk_emphasis_punctuation(text: &str) -> Option<(String, Vec<u32>)> {
    if !text
        .chars()
        .any(|character| cjk_emphasis_mark(character).is_some_and(|mark| mark != character))
    {
        return None;
    }

    let mut output = String::with_capacity(text.len());
    let mut cluster_map = Vec::with_capacity(text.len());
    for (source_offset, character) in text.char_indices() {
        let output_character = cjk_emphasis_mark(character).unwrap_or(character);
        output.push(output_character);
        cluster_map.resize(output.len(), source_offset as u32);
    }

    Some((output, cluster_map))
}

fn reorder_visual(levels: &[unicode_bidi::Level]) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..levels.len()).collect();
    if levels.is_empty() {
        return indices;
    }

    let max_level = levels.iter().map(|l| l.number()).max().unwrap();
    let min_odd_level = levels
        .iter()
        .map(|l| l.number())
        .filter(|&n| n % 2 != 0)
        .min()
        .unwrap_or(u8::MAX);

    if min_odd_level == u8::MAX {
        return indices;
    }

    for level in (min_odd_level..=max_level).rev() {
        let mut i = 0;
        while i < levels.len() {
            if levels[i].number() >= level {
                let mut j = i;
                while j < levels.len() && levels[j].number() >= level {
                    j += 1;
                }
                indices[i..j].reverse();
                i = j;
            } else {
                i += 1;
            }
        }
    }
    indices
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::fonts::{Font, FontSystem};
    use skrifa::{MetadataProvider, instance::Size};

    #[derive(Clone, Copy, Debug)]
    struct LineBreakMeasure {
        advance: f32,
        trailing_advance: f32,
        break_suffix_advance: f32,
        break_penalty: f32,
        is_mandatory: bool,
    }

    impl LineMeasurements {
        fn additive(segments: &[LineBreakMeasure]) -> Self {
            let advances = (0..segments.len())
                .map(|start| {
                    let mut sum = 0.0;
                    segments[start..]
                        .iter()
                        .enumerate()
                        .map(|(offset, segment)| {
                            sum += segment.advance;
                            sum - segment.trailing_advance
                                + if start + offset + 1 < segments.len() && !segment.is_mandatory {
                                    segment.break_suffix_advance
                                } else {
                                    0.0
                                }
                        })
                        .collect()
                })
                .collect();
            Self {
                segments: segments
                    .iter()
                    .map(|segment| BreakPoint {
                        hyphenated: segment.break_suffix_advance > 0.0 && !segment.is_mandatory,
                        break_penalty: segment.break_penalty,
                        is_mandatory: segment.is_mandatory,
                    })
                    .collect(),
                advances,
            }
        }
    }

    fn optimal_line_breaks(segments: &[LineBreakMeasure], max_extent: f32) -> Vec<usize> {
        optimal_uniform_line_breaks(&LineMeasurements::additive(segments), max_extent, true).breaks
    }

    fn any_system_font() -> Font {
        let mut fonts = FontSystem::new();

        // Prefer fonts that are commonly available depending on OS/environment.
        // This is only used to construct a `TextLayout` for calling `compute_bounds`.
        let preferred = [
            "Yu Gothic",
            "MS Gothic",
            "Noto Sans CJK JP",
            "Noto Sans",
            "Arial",
            "DejaVu Sans",
            "Liberation Sans",
        ];

        for name in preferred {
            if let Ok(font) = fonts.query_family(name) {
                return font;
            }
        }
        fonts
            .first_font()
            .expect("no system font available for tests")
    }

    fn assert_approx_eq(actual: f32, expected: f32) {
        if actual.is_infinite()
            && expected.is_infinite()
            && actual.is_sign_positive() == expected.is_sign_positive()
        {
            return;
        }
        let eps = 1e-4;
        assert!(
            (actual - expected).abs() <= eps,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn target_language_reaches_shaping_and_hyphenation() {
        let font = any_system_font();
        for tag in ["tr-TR", "en-US", "ja-JP", "sr-Latn"] {
            let layout = TextLayout::new(&font).with_language_tag(tag);
            assert_eq!(
                layout.language.as_ref().map(harfrust::Language::as_str),
                Some(tag.to_ascii_lowercase().as_str()),
            );
            assert_eq!(layout.hyphenation_lang, hyphenation_lang_from_tag(tag));
        }
    }

    #[test]
    fn preferred_font_size_ignores_invalid_scales() {
        let font = any_system_font();
        for size in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
            assert!(
                TextLayout::new(&font)
                    .with_preferred_font_size(size)
                    .preferred_font_size
                    .is_none()
            );
        }
    }

    #[test]
    fn preferred_font_size_allows_reflow_without_filling_the_source_area() -> anyhow::Result<()> {
        let first = any_system_font();
        let second = FontSystem::new()
            .first_font()
            .expect("a system font is required");
        for font in [&first, &second] {
            for mode in [WritingMode::Horizontal, WritingMode::VerticalRl] {
                let text = "ONE TWO THREE FOUR FIVE SIX";
                let builder = TextLayout::new(font)
                    .with_writing_mode(mode)
                    .with_hyphenation_policy(HyphenationPolicy::LastResort)
                    .with_max_width(220.0)
                    .with_max_height(220.0)
                    .with_comic_balloon(
                        220.0,
                        220.0,
                        vec![(0.0, 0.0), (220.0, 0.0), (220.0, 220.0), (0.0, 220.0)],
                        4.0,
                    );
                let reference = builder.clone().with_font_size(24.0).run(text)?;
                assert!(!reference.overflowed());
                let automatic = builder
                    .clone()
                    .with_min_font_size(12.0)
                    .with_max_font_size(80.0);
                let largest = automatic.run(text)?;
                let guided_builder = automatic.with_preferred_font_size(24.0);
                let guided = guided_builder.run(text)?;
                assert!(!guided.overflowed());
                assert!(
                    (guided.font_size - 24.0).abs() <= 1.0,
                    "mode={mode:?} expected the contextual scale, got {}",
                    guided.font_size
                );
                assert!(
                    font_scale_cost(guided.font_size, 24.0)
                        < font_scale_cost(largest.font_size, 24.0)
                );
                if mode == WritingMode::Horizontal {
                    assert!(largest.font_size - guided.font_size > 4.0);
                    assert_ne!(largest.lines.len(), guided.lines.len());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn unreachable_source_scale_preserves_capacity_based_composition() -> anyhow::Result<()> {
        let font = any_system_font();
        let mut intact_cases = 0;
        for (language, text) in [("en-US", "Take control!"), ("tr-TR", "Lanet olsun!")] {
            for scale in [0.5, 1.0, 2.0] {
                let width = 120.0 * scale;
                let height = 240.0 * scale;
                let base = TextLayout::new(&font)
                    .with_language_tag(language)
                    .with_hyphenation_policy(HyphenationPolicy::LastResort)
                    .with_alignment(TextAlign::Center)
                    .with_line_height(1.2)
                    .with_min_font_size(12.0 * scale)
                    .with_max_font_size(160.0 * scale)
                    .with_max_width(width)
                    .with_max_height(height)
                    .with_comic_balloon(
                        width,
                        height,
                        vec![(0.0, 0.0), (width, 0.0), (width, height), (0.0, height)],
                        4.0 * scale,
                    );
                let expected = base.run(text)?;
                intact_cases += usize::from(expected.discretionary_hyphen_count() == 0);
                for source_scale in [200.0, 800.0] {
                    let layout = base
                        .clone()
                        .with_preferred_font_size(source_scale * scale)
                        .run(text)?;
                    assert!(!layout.overflowed(), "{language} at {scale}");
                    assert_eq!(
                        layout.discretionary_hyphen_count(),
                        expected.discretionary_hyphen_count(),
                        "{language}: unreachable source size {source_scale} changed fragmentation"
                    );
                    assert_eq!(
                        layout.lines.len(),
                        expected.lines.len(),
                        "{language} at {scale}"
                    );
                    assert!(
                        (layout.font_size / expected.font_size - 1.0).abs() < 0.025,
                        "only infeasible typography changed: {} vs {}",
                        expected.font_size,
                        layout.font_size
                    );
                }
            }
        }
        assert!(
            intact_cases > 0,
            "no unfragmented composition was exercised"
        );
        Ok(())
    }

    #[test]
    fn preferred_font_size_searches_both_sides_of_a_nonmonotonic_gap() -> anyhow::Result<()> {
        let font = any_system_font();
        let builder = TextLayout::new(&font).with_preferred_font_size(24.5);
        let layout_at = |size| TextLayout::new(&font).with_font_size(size).run("A phrase");
        let selected = CompositionSearch::new(12.0, 50.0, 24.5, |size| {
            ((23.9..24.2).contains(&size) || size >= 26.0)
                .then(|| layout_at(size))
                .transpose()
        })?
        .select(24.5, None, |layout| {
            builder.composition_quality("A phrase", layout, 24.5)
        })?
        .cloned()
        .unwrap();
        assert!((23.9..24.2).contains(&selected.font_size));
        let selected_above = CompositionSearch::new(12.0, 50.0, 24.5, |size| {
            (size >= 26.0).then(|| layout_at(size)).transpose()
        })?
        .select(24.5, None, |layout| {
            builder.composition_quality("A phrase", layout, 24.5)
        })?
        .cloned()
        .unwrap();
        assert!((26.0..26.3).contains(&selected_above.font_size));
        Ok(())
    }

    #[test]
    fn composition_search_does_not_stop_at_the_source_size() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "prosperity";
        for scale in [0.5, 1.0, 2.0, 4.0] {
            let builder = TextLayout::new(&font).with_preferred_font_size(24.0 * scale);
            let layout_at = |size: f32| {
                let mut layout = TextLayout::new(&font).with_font_size(size).run(text)?;
                if size > 20.0 * scale && size < 27.0 * scale {
                    layout.lines = vec![
                        LayoutLine {
                            range: 0..5,
                            break_suffix: Some(LineBreakSuffix::Hyphen),
                            advance: 3.0 * size,
                            ..LayoutLine::default()
                        },
                        LayoutLine {
                            range: 5..text.len(),
                            advance: 3.0 * size,
                            ..LayoutLine::default()
                        },
                    ];
                }
                Ok(layout)
            };
            let selected =
                CompositionSearch::new(12.0 * scale, 40.0 * scale, 24.0 * scale, |size| {
                    layout_at(size).map(Some)
                })?
                .select(24.0 * scale, None, |layout| {
                    builder.composition_quality(text, layout, 24.0 * scale)
                })?
                .cloned()
                .unwrap();
            assert!(
                (27.0..27.5).contains(&(selected.font_size / scale)),
                "the closer whole-word composition above the source size was discarded: {} at scale {scale}",
                selected.font_size / scale,
            );
        }
        Ok(())
    }

    fn comic_balloon(
        width: f32,
        height: f32,
        contour: Vec<(f32, f32)>,
        minimum_air: f32,
    ) -> ComicBalloon {
        ComicBalloon {
            width,
            height,
            contours: vec![ContourConstraint {
                points: contour,
                air_scale: 1.0,
            }],
            minimum_air,
            preferred_center: None,
            preferred_line_count: None,
            strict_centering: false,
        }
    }

    #[test]
    fn capped_auto_size_shrinks_text_to_fit_the_available_height() -> anyhow::Result<()> {
        let font = any_system_font();
        let preferred_size = 24.0;
        let fixed = TextLayout::new(&font)
            .with_font_size(preferred_size)
            .with_max_width(1_000.0)
            .run("First\nSecond\nThird")?;
        let max_height = fixed.height * 0.65;

        let fitted = TextLayout::new(&font)
            .with_max_font_size(preferred_size)
            .with_max_width(1_000.0)
            .with_max_height(max_height)
            .run("First\nSecond\nThird")?;

        assert!(fitted.font_size < preferred_size);
        assert!(fitted.width <= 1_000.0);
        assert!(fitted.height <= max_height + 0.01);
        Ok(())
    }

    #[test]
    fn capped_auto_size_does_not_enlarge_text_that_already_fits() -> anyhow::Result<()> {
        let font = any_system_font();
        let preferred_size = 18.0;

        let fitted = TextLayout::new(&font)
            .with_max_font_size(preferred_size)
            .with_max_width(1_000.0)
            .with_max_height(1_000.0)
            .run("Fits")?;

        assert_eq!(fitted.font_size, preferred_size);
        Ok(())
    }

    #[test]
    fn bounded_auto_size_preserves_a_source_painted_footprint() -> anyhow::Result<()> {
        let font = any_system_font();
        let preferred_size = 80.0;
        let builder = TextLayout::new(&font).with_line_height(1.2);
        let unrestricted = builder
            .clone()
            .with_font_size(preferred_size)
            .with_max_width(1_000.0)
            .run("Şup♡")?;
        let maximum_area = unrestricted.width * unrestricted.height * 0.4;

        let mut search = builder
            .fitting_compositions(
                "Şup♡",
                12.0,
                preferred_size,
                |_| (1_000.0, 1_000.0),
                |layout| {
                    let dilation = layout.font_size * 0.1;
                    (layout.width + dilation * 2.0) * (layout.height + dilation * 2.0)
                        <= maximum_area
                },
            )?
            .expect("the minimum setting fits the painted footprint");
        let reference = search.reference();
        let fitted = search
            .select(reference, None, |layout| CompositionQuality {
                cost: layout.composition_cost("Şup♡", reference),
                ..CompositionQuality::default()
            })?
            .expect("a fitting composition exists");

        assert!(fitted.font_size < preferred_size);
        let dilation = fitted.font_size * 0.1;
        assert!(
            (fitted.width + dilation * 2.0) * (fitted.height + dilation * 2.0)
                <= maximum_area + 0.1
        );
        Ok(())
    }

    #[test]
    fn capped_auto_size_respects_the_readable_floor() -> anyhow::Result<()> {
        let font = any_system_font();

        let fitted = TextLayout::new(&font)
            .with_max_font_size(16.0)
            .with_min_font_size(8.0)
            .with_max_width(12.0)
            .with_max_height(12.0)
            .run("This translation cannot fit")?;

        assert_eq!(fitted.font_size, 8.0);
        Ok(())
    }

    #[test]
    fn capped_auto_size_keeps_the_configured_line_height() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "First\nSecond\nThird\nFourth";
        let expected = TextLayout::new(&font)
            .with_font_size(16.0)
            .with_line_height(1.2)
            .run(text)?;
        let max_height = expected.height - 1.0;

        let fitted = TextLayout::new(&font)
            .with_max_font_size(16.0)
            .with_min_font_size(16.0)
            .with_line_height(1.2)
            .with_max_width(1_000.0)
            .with_max_height(max_height)
            .run(text)?;

        assert_eq!(fitted.font_size, 16.0);
        assert!(fitted.height > max_height);
        assert_approx_eq(fitted.height, expected.height);
        let leading = fitted.lines[1].baseline.1 - fitted.lines[0].baseline.1;
        assert_approx_eq(leading, 16.0 * 1.2);
        Ok(())
    }

    #[test]
    fn font_size_search_finds_a_narrow_high_non_monotonic_fit() -> anyhow::Result<()> {
        let calls = Cell::new(0);
        let fitted = largest_fitting_font_size(
            9.0,
            40.0,
            |size| {
                calls.set(calls.get() + 1);
                Ok(size)
            },
            |size| *size == 37.0 || (12.0..=20.0).contains(size),
        )?
        .expect("the narrow higher fitting range should be found");

        assert_eq!(fitted, 37.0);
        assert_eq!(calls.get(), 4);
        Ok(())
    }

    #[test]
    fn comic_auto_size_uses_tall_balloon_capacity() -> anyhow::Result<()> {
        let font = any_system_font();
        let width = 130.0;
        let height = 250.0;
        let fitted = TextLayout::new(&font)
            .with_max_font_size(width)
            .with_min_font_size(9.0)
            .with_line_height(1.2)
            .with_hyphenation_policy(HyphenationPolicy::LastResort)
            .with_max_width(width)
            .with_max_height(height)
            .with_comic_balloon(
                width,
                height,
                vec![(0.0, 0.0), (width, 0.0), (width, height), (0.0, height)],
                4.0,
            )
            .run(
                "The Justice Realization Committee is a volunteer organization dedicated to the cause of justice.",
            )?;

        assert!(
            !fitted.overflowed(),
            "auto-fit overflowed at size {} with {} lines and height {}",
            fitted.font_size,
            fitted.lines.len(),
            fitted.height
        );
        assert!(
            fitted.height >= height * 0.5,
            "auto-fit left excessive vertical space: font size {}, height {}",
            fitted.font_size,
            fitted.height
        );
        Ok(())
    }

    #[test]
    fn irregular_tall_balloon_keeps_translated_dialogue_readable() -> anyhow::Result<()> {
        let font = any_system_font();
        let width = 150.0;
        let height = 260.0;
        let minimum = 9.0;
        let fitted = TextLayout::new(&font)
            .with_max_font_size(72.0)
            .with_min_font_size(minimum)
            .with_line_height(1.2)
            .with_hyphenation_policy(HyphenationPolicy::LastResort)
            .with_max_width(width)
            .with_max_height(height)
            .with_comic_balloon(
                width,
                height,
                vec![
                    (60.0, 0.0),
                    (112.0, 8.0),
                    (145.0, 45.0),
                    (150.0, 120.0),
                    (138.0, 210.0),
                    (105.0, 250.0),
                    (40.0, 258.0),
                    (8.0, 215.0),
                    (0.0, 130.0),
                    (12.0, 45.0),
                ],
                4.0,
            )
            .run(
                "You told me this would happen before. What are we supposed to do about it now?",
            )?;

        assert!(!fitted.overflowed());
        assert!(
            fitted.font_size >= minimum + 3.0,
            "unexpectedly small fitted size {}",
            fitted.font_size
        );
        Ok(())
    }

    #[test]
    fn optimal_line_breaks_balance_ragged_lines() {
        let segments = vec![
            LineBreakMeasure {
                advance: 30.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            };
            7
        ];

        assert_eq!(optimal_line_breaks(&segments, 100.0), vec![2, 4, 7]);
    }

    #[test]
    fn uniform_line_breaks_keep_same_decisions_when_scaled() {
        let reference = [1, 3];
        for scale in [0.5, 1.0, 2.0, 4.0] {
            let segments = [
                LineBreakMeasure {
                    advance: 4.0 * scale,
                    trailing_advance: scale,
                    break_suffix_advance: 0.0,
                    break_penalty: 0.0,
                    is_mandatory: false,
                },
                LineBreakMeasure {
                    advance: 4.0 * scale,
                    trailing_advance: 0.0,
                    break_suffix_advance: scale,
                    break_penalty: 0.0,
                    is_mandatory: false,
                },
                LineBreakMeasure {
                    advance: 4.0 * scale,
                    trailing_advance: 0.0,
                    break_suffix_advance: 0.0,
                    break_penalty: 0.0,
                    is_mandatory: false,
                },
            ];
            let result = optimal_uniform_line_breaks(
                &LineMeasurements::additive(&segments),
                10.0 * scale,
                true,
            );
            assert!(!result.overflowed, "scale={scale}");
            assert_eq!(result.breaks, reference, "scale={scale}");
        }
    }

    #[test]
    fn paragraph_breaks_retain_alternative_prefix_shapes() {
        let segments = [6.0, 6.0, 6.0, 20.0, 6.0, 6.0, 6.0].map(|advance| LineBreakMeasure {
            advance,
            trailing_advance: 0.0,
            break_suffix_advance: 0.0,
            break_penalty: 0.0,
            is_mandatory: false,
        });
        let result =
            optimal_uniform_line_breaks(&LineMeasurements::additive(&segments), 40.0, false);
        assert!(!result.overflowed);
        assert_eq!(result.breaks, [3, 4, 7]);
        assert!((result.cost - 462.75).abs() < 0.001);
    }

    #[test]
    fn paragraph_breaks_match_exhaustive_short_paragraphs() {
        fn cost_for(
            segments: &[LineBreakMeasure],
            breaks: &[usize],
            widths: &[f32],
            profiled: bool,
            allow_hyphenation: bool,
        ) -> Option<f32> {
            let mut start = 0;
            let mut previous_fitness = LINE_BREAK_ROOT_FITNESS;
            let mut cost = 0.0;
            for (line, &end) in breaks.iter().enumerate() {
                if segments[start..end - 1]
                    .iter()
                    .any(|segment| segment.is_mandatory)
                {
                    return None;
                }
                let last = segments[end - 1];
                let suffix = if end == segments.len() {
                    0.0
                } else {
                    last.break_suffix_advance
                };
                let hyphenated = suffix > 0.0;
                if hyphenated && !allow_hyphenation {
                    return None;
                }
                let width = widths[line.min(widths.len() - 1)];
                let advance = segments[start..end]
                    .iter()
                    .map(|segment| segment.advance)
                    .sum::<f32>()
                    - last.trailing_advance
                    + suffix;
                if advance > width {
                    return None;
                }
                let ratio = advance / width;
                let fitness = line_fitness(advance, width);
                cost += if profiled {
                    (1.0 - ratio).powi(2) * LINE_BREAK_RAGGEDNESS_PENALTY
                } else {
                    (1.0 - ratio).powi(3) * LINE_BREAK_RAGGEDNESS_PENALTY
                };
                if hyphenated {
                    cost += LINE_BREAK_HYPHEN_PENALTY;
                }
                if end != segments.len() {
                    cost += last.break_penalty;
                }
                cost += paragraph_transition_cost(
                    &LineMeasurements::additive(segments),
                    start..end,
                    ratio,
                    previous_fitness,
                    fitness,
                    hyphenated,
                );
                previous_fitness = fitness;
                start = end;
            }
            Some(if profiled {
                cost / breaks.len() as f32 + breaks.len() as f32 * 8.0
            } else {
                cost
            })
        }

        for case in 0..3_usize.pow(5) {
            let mut code = case;
            let segments = std::array::from_fn::<_, 5, _>(|index| {
                let advance = [6.0, 12.0, 20.0][code % 3];
                code /= 3;
                LineBreakMeasure {
                    advance,
                    trailing_advance: if index % 2 == 0 { 1.0 } else { 0.0 },
                    break_suffix_advance: if index % 2 == 1 { 1.0 } else { 0.0 },
                    break_penalty: if index == 2 { 100.0 } else { 0.0 },
                    is_mandatory: case % 7 == 0 && index == 2,
                }
            });
            for allow_hyphenation in [false, true] {
                for (profiled, widths) in [(false, &[40.0][..]), (true, &[24.0, 40.0, 28.0][..])] {
                    let minimum = (0..1_usize << (segments.len() - 1))
                        .filter_map(|mask| {
                            let mut breaks = (1..segments.len())
                                .filter(|end| mask & (1 << (end - 1)) != 0)
                                .collect::<Vec<_>>();
                            breaks.push(segments.len());
                            if profiled && breaks.len() != widths.len() {
                                return None;
                            }
                            cost_for(&segments, &breaks, widths, profiled, allow_hyphenation)
                        })
                        .min_by(f32::total_cmp);
                    let actual = if profiled {
                        let profiles = widths
                            .iter()
                            .map(|&width| LineProfile {
                                width,
                                center_offset: 0.0,
                                block_baseline: 0.0,
                            })
                            .collect::<Vec<_>>();
                        exact_profiled_line_breaks_pass(
                            &LineMeasurements::additive(&segments),
                            &profiles,
                            allow_hyphenation,
                            false,
                        )
                    } else {
                        uniform_line_breaks_pass(
                            &LineMeasurements::additive(&segments),
                            widths[0],
                            allow_hyphenation,
                            false,
                        )
                    };
                    match (actual, minimum) {
                        (Some(actual), Some(expected)) => {
                            assert!(!actual.overflowed);
                            assert!(
                                (actual.cost - expected).abs() < 0.01,
                                "case={case} profiled={profiled} hyphenation={allow_hyphenation} actual={} expected={expected}",
                                actual.cost
                            );
                        }
                        (None, None) => {}
                        _ => panic!(
                            "feasibility mismatch for case={case} profiled={profiled} hyphenation={allow_hyphenation}"
                        ),
                    }
                }
            }
        }
    }

    #[test]
    fn line_advance_counts_only_drawn_glyphs() -> anyhow::Result<()> {
        let font = any_system_font();
        for mode in [WritingMode::Horizontal, WritingMode::VerticalRl] {
            let layout = TextLayout::new(&font)
                .with_font_size(24.0)
                .with_writing_mode(mode)
                .with_hyphenation_policy(HyphenationPolicy::Disabled)
                .run("ONE  \nTWO  ")?;
            assert_eq!(layout.lines.len(), 2);
            for line in &layout.lines {
                let drawn_advance = if mode.is_vertical() {
                    line.glyphs
                        .iter()
                        .map(|glyph| glyph.y_advance)
                        .sum::<f32>()
                        .abs()
                } else {
                    line.glyphs.iter().map(|glyph| glyph.x_advance).sum()
                };
                assert_approx_eq(line.advance, drawn_advance);
            }
        }
        Ok(())
    }

    #[test]
    fn line_fit_excludes_invisible_trailing_break_space() {
        let segments = [
            LineBreakMeasure {
                advance: 55.0,
                trailing_advance: 10.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 40.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
        ];

        let result = line_breaks_with_policy(
            &LineMeasurements::additive(&segments),
            50.0,
            HyphenationPolicy::Disabled,
        );

        assert_eq!(result.breaks, [1, 2]);
        assert!(!result.overflowed);
    }

    #[test]
    fn uniform_breaks_reject_overflow_when_legal_breaks_fit() {
        let segments = [
            LineBreakMeasure {
                advance: 51.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 50.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
        ];

        let result =
            optimal_uniform_line_breaks(&LineMeasurements::additive(&segments), 100.0, false);

        assert_eq!(result.breaks, [1, 2]);
        assert!(!result.overflowed);
    }

    #[test]
    fn profiled_breaks_reject_overflow_when_legal_breaks_fit() {
        let segments = [
            LineBreakMeasure {
                advance: 51.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 50.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 1.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
        ];
        let profiles = vec![
            LineProfile {
                width: 100.0,
                center_offset: 0.0,
                block_baseline: 0.0,
            },
            LineProfile {
                width: 100.0,
                center_offset: 0.0,
                block_baseline: 20.0,
            },
        ];

        let result =
            exact_profiled_line_breaks(&LineMeasurements::additive(&segments), profiles, false)
                .unwrap();

        assert_eq!(result.breaks, [1, 3]);
        assert!(!result.overflowed);
    }

    #[test]
    fn profiled_breaks_allow_only_subpixel_raster_overhang() {
        let profile = || {
            vec![LineProfile {
                width: 100.0,
                center_offset: 0.0,
                block_baseline: 20.0,
            }]
        };
        let segment = |advance| {
            vec![LineBreakMeasure {
                advance,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            }]
        };

        let raster_fit = exact_profiled_line_breaks(
            &LineMeasurements::additive(&segment(100.5)),
            profile(),
            false,
        )
        .unwrap();
        let visible_overflow = exact_profiled_line_breaks(
            &LineMeasurements::additive(&segment(100.51)),
            profile(),
            false,
        )
        .unwrap();

        assert!(!raster_fit.overflowed);
        assert!(visible_overflow.overflowed);
    }

    #[test]
    fn comic_profiles_are_widest_in_the_middle() {
        let balloon = comic_balloon(200.0, 120.0, Vec::new(), 0.0);
        let profiles = balloon
            .line_profiles(
                WritingMode::Horizontal,
                5,
                16.0,
                InkBand {
                    before: 8.0,
                    after: 8.0,
                },
                10.0,
                10.0,
            )
            .unwrap();

        assert!(profiles[0].width < profiles[1].width);
        assert!(profiles[1].width < profiles[2].width);
        assert!((profiles[0].width - profiles[4].width).abs() < 0.001);
        assert!((profiles[1].width - profiles[3].width).abs() < 0.001);
    }

    #[test]
    fn vertical_comic_profiles_are_tallest_in_the_middle() {
        let balloon = comic_balloon(120.0, 200.0, Vec::new(), 0.0);
        let profiles = balloon
            .line_profiles(
                WritingMode::VerticalRl,
                5,
                16.0,
                InkBand {
                    before: 8.0,
                    after: 8.0,
                },
                10.0,
                10.0,
            )
            .unwrap();

        assert!(profiles[0].width < profiles[1].width);
        assert!(profiles[1].width < profiles[2].width);
        assert!((profiles[0].width - profiles[4].width).abs() < 0.001);
        assert!((profiles[1].width - profiles[3].width).abs() < 0.001);
    }

    #[test]
    fn comic_profiles_use_the_detected_contour_without_an_extra_ellipse() {
        let balloon = comic_balloon(
            200.0,
            120.0,
            vec![(0.0, 0.0), (200.0, 0.0), (200.0, 120.0), (0.0, 120.0)],
            0.0,
        );
        let profiles = balloon
            .line_profiles(
                WritingMode::Horizontal,
                5,
                16.0,
                InkBand {
                    before: 8.0,
                    after: 8.0,
                },
                10.0,
                10.0,
            )
            .unwrap();

        for profile in profiles {
            assert_approx_eq(profile.width, 180.0);
        }
    }

    #[test]
    fn comic_profiles_measure_the_glyph_ink_band_instead_of_leading() {
        let balloon = comic_balloon(
            200.0,
            120.0,
            vec![(100.0, 0.0), (200.0, 60.0), (100.0, 120.0), (0.0, 60.0)],
            0.0,
        );
        let profile = balloon
            .line_profiles(
                WritingMode::Horizontal,
                1,
                80.0,
                InkBand {
                    before: 20.0,
                    after: 20.0,
                },
                0.0,
                10.0,
            )
            .unwrap()[0];

        assert!((130.0..=135.0).contains(&profile.width));
    }

    #[test]
    fn comic_profiles_respect_asymmetric_contours() {
        let balloon = comic_balloon(
            200.0,
            120.0,
            vec![(0.0, 0.0), (140.0, 0.0), (140.0, 120.0), (0.0, 120.0)],
            0.0,
        );
        let profile = balloon
            .line_profiles(
                WritingMode::Horizontal,
                1,
                16.0,
                InkBand {
                    before: 8.0,
                    after: 8.0,
                },
                10.0,
                10.0,
            )
            .unwrap()[0];

        assert!(profile.center_offset < -20.0);
        assert!(profile.width < 130.0);
    }

    #[test]
    fn comic_profiles_keep_one_axis_across_an_irregular_contour() {
        let balloon = comic_balloon(
            200.0,
            160.0,
            vec![(30.0, 0.0), (200.0, 0.0), (165.0, 160.0), (0.0, 160.0)],
            0.0,
        );
        let horizontal = balloon
            .line_profiles(
                WritingMode::Horizontal,
                4,
                24.0,
                InkBand {
                    before: 10.0,
                    after: 10.0,
                },
                8.0,
                8.0,
            )
            .unwrap();
        let vertical = balloon
            .line_profiles(
                WritingMode::VerticalRl,
                4,
                24.0,
                InkBand {
                    before: 10.0,
                    after: 10.0,
                },
                8.0,
                8.0,
            )
            .unwrap();

        for profiles in [&horizontal, &vertical] {
            let axis = profiles[0].center_offset;
            assert!(
                profiles
                    .iter()
                    .all(|profile| (profile.center_offset - axis).abs() < 0.001)
            );
        }
    }

    #[test]
    fn comic_profiles_keep_the_body_axis_instead_of_following_a_tapered_line() {
        let balloon = comic_balloon(
            200.0,
            110.0,
            vec![(0.0, 0.0), (100.0, 0.0), (200.0, 110.0), (90.0, 110.0)],
            0.0,
        );
        let profiles = balloon
            .line_profiles_at_origin(
                WritingMode::Horizontal,
                2,
                80.0,
                InkBand {
                    before: 10.0,
                    after: 10.0,
                },
                110.0,
                200.0,
                0.0,
                0.0,
                5.0,
            )
            .unwrap();

        assert_approx_eq(profiles[0].center_offset, profiles[1].center_offset);
        assert_approx_eq(
            profiles[0].center_offset,
            balloon.center().0 - balloon.width * 0.5,
        );
        assert!(profiles.iter().all(|profile| profile.width > 0.0));
        assert!((profiles[0].width - profiles[1].width).abs() > 1.0);
    }

    #[test]
    fn comic_profiles_seed_the_area_centroid_when_wider_space_is_elsewhere() {
        let balloon = comic_balloon(
            100.0,
            200.0,
            vec![
                (0.0, 0.0),
                (100.0, 0.0),
                (100.0, 200.0),
                (50.0, 200.0),
                (50.0, 80.0),
                (0.0, 80.0),
            ],
            0.0,
        );
        let preferred_center = balloon.block_center(WritingMode::Horizontal, balloon.height);

        let profiles = balloon
            .line_profiles(
                WritingMode::Horizontal,
                3,
                20.0,
                InkBand {
                    before: 10.0,
                    after: 10.0,
                },
                0.0,
                0.0,
            )
            .unwrap();

        let block_center = profiles[0].block_baseline + 20.0;
        assert_approx_eq(block_center, preferred_center);
        assert!(profiles[0].width > 50.0);
        assert!(profiles.last().unwrap().width < 50.0);
        let axis = balloon.center().0;
        for profile in &profiles {
            assert_approx_eq(profile.center_offset + balloon.width * 0.5, axis);
            for block in [profile.block_baseline - 10.0, profile.block_baseline + 10.0] {
                let (left, right) = balloon
                    .contour_inline_span(WritingMode::Horizontal, block, 0.0)
                    .unwrap();
                assert!(axis - profile.width * 0.5 >= left - 0.001);
                assert!(axis + profile.width * 0.5 <= right + 0.001);
            }
        }
    }

    #[test]
    fn comic_profiles_use_the_resolved_flow_center() {
        let mut balloon = comic_balloon(
            100.0,
            200.0,
            vec![(0.0, 0.0), (100.0, 0.0), (100.0, 200.0), (0.0, 200.0)],
            0.0,
        );
        balloon.preferred_center = Some((50.0, 70.0));

        let profiles = balloon
            .line_profiles(
                WritingMode::Horizontal,
                3,
                20.0,
                InkBand {
                    before: 10.0,
                    after: 10.0,
                },
                0.0,
                0.0,
            )
            .unwrap();

        let block_center = profiles[0].block_baseline + 20.0;
        assert_approx_eq(block_center, 70.0);
    }

    #[test]
    fn comic_breaks_refine_the_nearest_fit_toward_the_flow_center() {
        let segments = [LineBreakMeasure {
            advance: 65.0,
            trailing_advance: 0.0,
            break_suffix_advance: 0.0,
            break_penalty: 0.0,
            is_mandatory: false,
        }];
        let mut balloon = comic_balloon(
            100.0,
            100.0,
            vec![(50.0, 0.0), (100.0, 50.0), (50.0, 100.0), (0.0, 50.0)],
            0.0,
        );
        balloon.preferred_center = Some((50.0, 70.0));

        let result = comic_line_breaks(
            &LineMeasurements::additive(&segments),
            &balloon,
            WritingMode::Horizontal,
            20.0,
            InkBand {
                before: 10.0,
                after: 10.0,
            },
            (0.0, 0.0),
            HyphenationPolicy::Disabled,
        );

        assert!(!result.overflowed);
        let center = result.profiles[0].block_baseline;
        assert!((57.25..=57.75).contains(&center), "refined center {center}");
    }

    #[test]
    fn comic_breaks_use_the_balloon_body_instead_of_its_tail() {
        let segments = [LineBreakMeasure {
            advance: 60.0,
            trailing_advance: 0.0,
            break_suffix_advance: 0.0,
            break_penalty: 0.0,
            is_mandatory: false,
        }; 6];
        let balloon = comic_balloon(
            160.0,
            220.0,
            vec![
                (0.0, 0.0),
                (160.0, 0.0),
                (160.0, 100.0),
                (90.0, 100.0),
                (80.0, 220.0),
                (70.0, 100.0),
                (0.0, 100.0),
            ],
            0.0,
        );

        let result = comic_line_breaks(
            &LineMeasurements::additive(&segments),
            &balloon,
            WritingMode::Horizontal,
            20.0,
            InkBand {
                before: 10.0,
                after: 10.0,
            },
            (0.0, 0.0),
            HyphenationPolicy::Disabled,
        );

        assert_eq!(result.breaks, [2, 4, 6]);
        assert!(!result.overflowed);
        assert!(result.profiles.iter().all(|profile| profile.width >= 120.0));
        assert!(result.profiles[0].block_baseline < 80.0);
    }

    #[test]
    fn comic_short_line_prefers_body_clearance_over_tail_center() {
        let segments = [LineBreakMeasure {
            advance: 10.0,
            trailing_advance: 0.0,
            break_suffix_advance: 0.0,
            break_penalty: 0.0,
            is_mandatory: false,
        }];
        let balloon = comic_balloon(
            160.0,
            220.0,
            vec![
                (0.0, 0.0),
                (160.0, 0.0),
                (160.0, 100.0),
                (90.0, 100.0),
                (80.0, 220.0),
                (70.0, 100.0),
                (0.0, 100.0),
            ],
            0.0,
        );

        let result = comic_line_breaks(
            &LineMeasurements::additive(&segments),
            &balloon,
            WritingMode::Horizontal,
            20.0,
            InkBand {
                before: 10.0,
                after: 10.0,
            },
            (0.0, 0.0),
            HyphenationPolicy::Disabled,
        );

        assert!(!result.overflowed);
        assert!(result.profiles[0].width >= 120.0);
        assert!(result.profiles[0].block_baseline <= 90.0 + f32::EPSILON);
    }

    #[test]
    fn comic_breaks_do_not_chase_a_one_pixel_edge_advantage() {
        let segments = [LineBreakMeasure {
            advance: 40.0,
            trailing_advance: 0.0,
            break_suffix_advance: 0.0,
            break_penalty: 0.0,
            is_mandatory: false,
        }];
        let balloon = comic_balloon(
            100.0,
            200.0,
            vec![(1.0, 0.0), (99.0, 0.0), (100.0, 200.0), (0.0, 200.0)],
            0.0,
        );

        let result = comic_line_breaks(
            &LineMeasurements::additive(&segments),
            &balloon,
            WritingMode::Horizontal,
            20.0,
            InkBand {
                before: 10.0,
                after: 10.0,
            },
            (0.0, 0.0),
            HyphenationPolicy::Disabled,
        );

        assert!(!result.overflowed);
        assert!((result.profiles[0].block_baseline - 100.0).abs() < 1.0);
    }

    #[test]
    fn comic_breaks_center_before_preferring_fewer_lines() {
        let segments = [
            LineBreakMeasure {
                advance: 40.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 35.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
        ];
        let balloon = comic_balloon(
            100.0,
            200.0,
            vec![
                (0.0, 0.0),
                (100.0, 0.0),
                (100.0, 60.0),
                (70.0, 80.0),
                (70.0, 120.0),
                (100.0, 140.0),
                (100.0, 200.0),
                (0.0, 200.0),
                (0.0, 140.0),
                (30.0, 120.0),
                (30.0, 80.0),
                (0.0, 60.0),
            ],
            0.0,
        );

        let result = comic_line_breaks(
            &LineMeasurements::additive(&segments),
            &balloon,
            WritingMode::Horizontal,
            20.0,
            InkBand {
                before: 10.0,
                after: 10.0,
            },
            (0.0, 0.0),
            HyphenationPolicy::Disabled,
        );

        assert_eq!(result.breaks, [1, 2]);
        assert!(!result.overflowed);
        assert_approx_eq(result.profiles[0].block_baseline, 90.0);
        assert_approx_eq(result.profiles[1].block_baseline, 110.0);
    }

    #[test]
    fn vertical_comic_breaks_use_the_balloon_body_instead_of_its_tail() {
        let segments = [LineBreakMeasure {
            advance: 60.0,
            trailing_advance: 0.0,
            break_suffix_advance: 0.0,
            break_penalty: 0.0,
            is_mandatory: false,
        }; 6];
        let balloon = comic_balloon(
            220.0,
            160.0,
            vec![
                (0.0, 0.0),
                (0.0, 160.0),
                (100.0, 160.0),
                (100.0, 90.0),
                (220.0, 80.0),
                (100.0, 70.0),
                (100.0, 0.0),
            ],
            0.0,
        );

        let result = comic_line_breaks(
            &LineMeasurements::additive(&segments),
            &balloon,
            WritingMode::VerticalRl,
            20.0,
            InkBand {
                before: 10.0,
                after: 10.0,
            },
            (0.0, 0.0),
            HyphenationPolicy::Disabled,
        );

        assert_eq!(result.breaks, [2, 4, 6]);
        assert!(!result.overflowed);
        assert!(result.profiles.iter().all(|profile| profile.width >= 120.0));
        assert!(result.profiles[0].block_baseline <= 90.0 + f32::EPSILON);
    }

    #[test]
    fn comic_profiles_center_on_the_divided_lobe_bounds() {
        let balloon = ComicBalloon {
            width: 100.0,
            height: 200.0,
            contours: vec![
                ContourConstraint {
                    points: vec![(0.0, 0.0), (100.0, 0.0), (100.0, 200.0), (0.0, 200.0)],
                    air_scale: 1.0,
                },
                ContourConstraint {
                    points: vec![(0.0, 40.0), (100.0, 40.0), (100.0, 160.0), (0.0, 160.0)],
                    air_scale: 0.5,
                },
            ],
            minimum_air: 0.0,
            preferred_center: None,
            preferred_line_count: None,
            strict_centering: false,
        };

        let profiles = balloon
            .line_profiles(
                WritingMode::Horizontal,
                3,
                20.0,
                InkBand {
                    before: 10.0,
                    after: 10.0,
                },
                0.0,
                0.0,
            )
            .unwrap();

        assert!((profiles[0].block_baseline - 80.0).abs() < 0.01);
        assert!((profiles[2].block_baseline - 120.0).abs() < 0.01);
    }

    #[test]
    fn comic_profiles_offer_off_center_origins_for_irregular_capacity() {
        let balloon = ComicBalloon {
            width: 100.0,
            height: 160.0,
            contours: vec![
                ContourConstraint {
                    points: vec![(0.0, 0.0), (100.0, 0.0), (100.0, 160.0), (0.0, 160.0)],
                    air_scale: 1.0,
                },
                ContourConstraint {
                    points: vec![(0.0, 0.0), (100.0, 0.0), (100.0, 160.0), (48.0, 160.0)],
                    air_scale: 0.5,
                },
            ],
            minimum_air: 0.0,
            preferred_center: None,
            preferred_line_count: None,
            strict_centering: false,
        };
        let line_ink = InkBand {
            before: 5.0,
            after: 5.0,
        };
        let preferred_center = balloon.block_center(WritingMode::Horizontal, balloon.height);
        let candidates =
            balloon.line_profile_candidates(WritingMode::Horizontal, 5, 20.0, line_ink, 0.0, 0.0);

        assert!(candidates.len() > 1);
        let first_block_center = candidates[0][0].block_baseline + 40.0;
        assert_approx_eq(first_block_center, preferred_center);
        assert!(candidates.iter().any(|profiles| {
            (profiles[0].block_baseline - candidates[0][0].block_baseline).abs() >= 10.0
        }));
    }

    #[test]
    fn comic_constraints_intersect_before_selecting_a_concave_lobe() {
        let balloon = ComicBalloon {
            width: 100.0,
            height: 100.0,
            contours: vec![
                ContourConstraint {
                    points: vec![
                        (0.0, 0.0),
                        (100.0, 0.0),
                        (100.0, 100.0),
                        (70.0, 100.0),
                        (70.0, 20.0),
                        (40.0, 20.0),
                        (40.0, 100.0),
                        (0.0, 100.0),
                    ],
                    air_scale: 1.0,
                },
                ContourConstraint {
                    points: vec![(60.0, 0.0), (100.0, 0.0), (100.0, 100.0), (60.0, 100.0)],
                    air_scale: 0.5,
                },
            ],
            minimum_air: 0.0,
            preferred_center: None,
            preferred_line_count: None,
            strict_centering: false,
        };

        assert_eq!(
            balloon.contour_inline_span(WritingMode::Horizontal, 50.0, 0.0),
            Some((70.0, 100.0))
        );
    }

    #[test]
    fn comic_flow_boundaries_share_the_inter_phrase_air() {
        let balloon = ComicBalloon {
            width: 100.0,
            height: 100.0,
            contours: vec![
                ContourConstraint {
                    points: vec![(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0)],
                    air_scale: 1.0,
                },
                ContourConstraint {
                    points: vec![(50.0, 0.0), (100.0, 0.0), (100.0, 100.0), (50.0, 100.0)],
                    air_scale: 0.5,
                },
            ],
            minimum_air: 0.0,
            preferred_center: None,
            preferred_line_count: None,
            strict_centering: false,
        };

        assert_eq!(
            balloon.contour_inline_span(WritingMode::Horizontal, 50.0, 10.0),
            Some((55.0, 90.0))
        );
    }

    #[test]
    fn comic_layout_preserves_the_contour_center_for_both_writing_modes() -> anyhow::Result<()> {
        let font = any_system_font();
        let horizontal = TextLayout::new(&font)
            .with_font_size(16.0)
            .with_alignment(TextAlign::Center)
            .with_max_width(200.0)
            .with_max_height(120.0)
            .with_comic_balloon(
                200.0,
                120.0,
                vec![(0.0, 0.0), (140.0, 0.0), (140.0, 120.0), (0.0, 120.0)],
                0.0,
            )
            .run("Hello")?;
        let vertical = TextLayout::new(&font)
            .with_font_size(16.0)
            .with_writing_mode(WritingMode::VerticalRl)
            .with_alignment(TextAlign::Left)
            .with_max_width(120.0)
            .with_max_height(200.0)
            .with_comic_balloon(
                120.0,
                200.0,
                vec![(0.0, 0.0), (120.0, 0.0), (120.0, 140.0), (0.0, 140.0)],
                0.0,
            )
            .run("Hello")?;

        assert!(horizontal.placement_offset_x() < -10.0);
        assert!(vertical.placement_offset_y() < -10.0);
        Ok(())
    }

    #[test]
    fn comic_auto_fit_only_hyphenates_one_word_for_a_larger_raster_size() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "prosperity";
        let layout = |policy| {
            TextLayout::new(&font)
                .with_min_font_size(9.0)
                .with_max_font_size(24.0)
                .with_language_tag("en")
                .with_hyphenation_policy(policy)
                .with_alignment(TextAlign::Center)
                .with_max_width(80.0)
                .with_max_height(100.0)
                .with_comic_balloon(
                    80.0,
                    100.0,
                    vec![(0.0, 0.0), (80.0, 0.0), (80.0, 100.0), (0.0, 100.0)],
                    0.0,
                )
                .run(text)
        };
        let clean = layout(HyphenationPolicy::Disabled)?;
        let last_resort = layout(HyphenationPolicy::LastResort)?;
        let discretionary_hyphens = last_resort.discretionary_hyphen_count();

        assert!(!last_resort.overflowed());
        assert!(
            last_resort.font_size + f32::EPSILON >= clean.font_size,
            "clean size {}, LastResort size {}",
            clean.font_size,
            last_resort.font_size
        );
        assert!(
            discretionary_hyphens == 0 || last_resort.font_size.floor() > clean.font_size.floor(),
            "{discretionary_hyphens} discretionary hyphens recovered only a subpixel gain: clean size {}, LastResort size {}",
            clean.font_size,
            last_resort.font_size
        );
        Ok(())
    }

    fn best_test_composition<'a>(
        builder: &TextLayout<'a>,
        text: &str,
        candidates: &[LayoutRun<'a>],
    ) -> LayoutRun<'a> {
        let reference = candidates
            .iter()
            .map(|candidate| candidate.font_size)
            .max_by(f32::total_cmp)
            .unwrap();
        candidates
            .iter()
            .min_by(|left, right| {
                builder
                    .composition_quality(text, left, reference)
                    .compare(builder.composition_quality(text, right, reference))
            })
            .unwrap()
            .clone()
    }

    #[test]
    fn painted_lines_stay_inside_the_balloon_after_alignment() -> anyhow::Result<()> {
        let font = any_system_font();
        let width = 220.0;
        let height = 150.0;
        let contour = vec![(110.0, 0.0), (220.0, 75.0), (110.0, 150.0), (0.0, 75.0)];
        for alignment in [TextAlign::Left, TextAlign::Center, TextAlign::Right] {
            for outset in [0.0, 6.0] {
                let template = TextLayout::new(&font)
                    .with_comic_balloon(width, height, contour.clone(), 0.0)
                    .with_max_width(width)
                    .with_max_height(height)
                    .with_min_font_size(12.0)
                    .with_max_font_size(32.0)
                    .with_preferred_font_size(32.0)
                    .with_alignment(alignment)
                    .with_line_height(1.2);
                let result = template.run_with_ink_outset("Ink\nOutline\nSill", |_| outset)?;
                assert!(!result.overflowed(), "{alignment:?} outset={outset}");
                let x = (width - result.width) * 0.5 + result.placement_offset_x();
                let y = (height - result.height) * 0.5 + result.placement_offset_y();
                for line in &result.lines {
                    let (left, top, right, bottom) = template
                        .ink_bounds(result.font_size, std::slice::from_ref(line))
                        .unwrap();
                    for px in [left + x - outset, right + x + outset] {
                        for py in [top + y - outset, bottom + y + outset] {
                            assert!(
                                (px - width * 0.5).abs() / (width * 0.5)
                                    + (py - height * 0.5).abs() / (height * 0.5)
                                    <= 1.0,
                                "{alignment:?} outset={outset}: ({px}, {py}) escaped the wall"
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn painted_outsets_leave_room_in_justified_paragraphs() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "Painted outlines need room around every justified line.";
        let template = TextLayout::new(&font)
            .with_font_size(28.0)
            .with_alignment(TextAlign::Justify)
            .with_max_width(260.0)
            .with_max_height(500.0);
        let plain = template.run(text)?;
        assert!(plain.lines.len() > 1 && !plain.overflowed());
        let outset = 10.0;
        let outlined = template.run_with_ink_outset(text, |_| outset)?;
        assert!(!outlined.overflowed(), "outlined width: {}", outlined.width);
        assert!(outlined.width + outset * 2.0 <= 260.0);
        assert_eq!(outlined.font_size, 28.0);
        Ok(())
    }

    #[test]
    fn painted_outsets_respect_curved_walls_and_flow_partitions() -> anyhow::Result<()> {
        let font = any_system_font();
        for mode in [WritingMode::Horizontal, WritingMode::VerticalRl] {
            for partition in [false, true] {
                let width = 220.0;
                let height = 280.0;
                let contour = vec![(110.0, 0.0), (220.0, 140.0), (110.0, 280.0), (0.0, 140.0)];
                let mut contours = vec![contour];
                if partition {
                    contours.push(vec![
                        (30.0, 0.0),
                        (140.0, 0.0),
                        (140.0, height),
                        (30.0, height),
                    ]);
                }
                let template = TextLayout::new(&font)
                    .with_writing_mode(mode)
                    .with_comic_balloon_constraints(width, height, contours, 0.0)
                    .with_max_width(width)
                    .with_max_height(height)
                    .with_min_font_size(12.0)
                    .with_max_font_size(40.0)
                    .with_preferred_font_size(32.0)
                    .with_alignment(TextAlign::Center)
                    .with_line_height(1.2);
                let outset = 6.0;
                let result = template.run_with_ink_outset("Ink\nOutline", |_| outset)?;
                assert!(!result.overflowed(), "{mode:?} partition={partition}");
                let x = (width - result.width) * 0.5 + result.placement_offset_x();
                let y = (height - result.height) * 0.5 + result.placement_offset_y();
                for line in &result.lines {
                    let (left, top, right, bottom) = template
                        .ink_bounds(result.font_size, std::slice::from_ref(line))
                        .unwrap();
                    for px in [left + x - outset, right + x + outset] {
                        for py in [top + y - outset, bottom + y + outset] {
                            assert!(
                                (px - width * 0.5).abs() / (width * 0.5)
                                    + (py - height * 0.5).abs() / (height * 0.5)
                                    <= 1.0,
                                "painted corner ({px}, {py}) escaped the curved wall"
                            );
                            if partition {
                                assert!(
                                    (30.0..=140.0).contains(&px),
                                    "outline escaped the flow cell: {px}"
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn composition_cost_trades_fragmentation_against_relative_scale() {
        let text = "prosperity";
        let candidate = |font_size, ranges: &[std::ops::Range<usize>]| LayoutRun {
            lines: ranges
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, range)| LayoutLine {
                    advance: (range.len() as f32) * font_size * 0.5,
                    break_suffix: (index + 1 < ranges.len()).then_some(LineBreakSuffix::Hyphen),
                    range,
                    ..LayoutLine::default()
                })
                .collect(),
            width: font_size * 4.0,
            height: font_size * 2.0,
            font_size,
            overflowed: false,
            placement_offset_x: 0.0,
            placement_offset_y: 0.0,
        };
        for scale in [0.5, 1.0, 2.0, 4.0] {
            let reference = 32.0 * scale;
            let fragmented = candidate(reference, &[0..3, 3..6, 6..text.len()]);
            let clean = candidate(26.0 * scale, std::slice::from_ref(&(0..text.len())));
            let tiny = candidate(16.0 * scale, std::slice::from_ref(&(0..text.len())));
            assert!(
                clean.composition_cost(text, reference)
                    < fragmented.composition_cost(text, reference)
            );
            assert!(
                tiny.composition_cost(text, reference)
                    > fragmented.composition_cost(text, reference)
            );
        }

        let sparse = "one two three four five six lettering";
        let split = sparse.find("lettering").unwrap() + 4;
        let readable = candidate(32.0, &[0..split, split..sparse.len()]);
        let smaller = candidate(26.0, std::slice::from_ref(&(0..sparse.len())));
        assert!(readable.composition_cost(sparse, 32.0) < smaller.composition_cost(sparse, 32.0));
    }

    #[test]
    fn composition_cost_does_not_penalize_authored_line_breaks() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "A deliberate long line.\nI";
        let authored = TextLayout::new(&font).with_font_size(24.0).run(text)?;
        assert_eq!(authored.lines.len(), 2);
        assert_eq!(authored.composition_cost(text, 24.0), 0.0);
        Ok(())
    }

    #[test]
    fn natural_script_breaks_are_not_discretionary_hyphens() -> anyhow::Result<()> {
        let font = any_system_font();
        for (language, text) in [
            ("ja-JP", "今日は良い天気です。新しい本を読みます。"),
            ("zh-CN", "今天天气很好，我们一起阅读新的故事。"),
        ] {
            for mode in [WritingMode::Horizontal, WritingMode::VerticalRl] {
                let layout = TextLayout::new(&font)
                    .with_font_size(24.0)
                    .with_language_tag(language)
                    .with_writing_mode(mode)
                    .with_max_width(150.0)
                    .with_max_height(150.0)
                    .with_hyphenation_policy(HyphenationPolicy::Disabled)
                    .run(text)?;
                assert!(layout.lines.len() > 1, "{language} {mode:?}");
                assert!(!layout.overflowed(), "{language} {mode:?}");
                assert_eq!(
                    layout.discretionary_hyphen_count(),
                    0,
                    "{language} {mode:?}: natural line breaks do not draw hyphens"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn discretionary_hyphen_metadata_matches_the_drawn_suffix() -> anyhow::Result<()> {
        let font = any_system_font();
        for (language, text) in [
            ("en-US", "characteristically"),
            ("tr-TR", "tasarruflarımız"),
        ] {
            let base = TextLayout::new(&font)
                .with_font_size(24.0)
                .with_language_tag(language);
            let unbroken = base
                .clone()
                .with_hyphenation_policy(HyphenationPolicy::Disabled)
                .run(text)?;
            let wrapped = base
                .clone()
                .with_max_width(unbroken.width * 0.55)
                .with_max_height(600.0)
                .with_hyphenation_policy(HyphenationPolicy::Normal)
                .run(text)?;
            assert!(!wrapped.overflowed(), "{language}");
            assert!(wrapped.discretionary_hyphen_count() > 0, "{language}");
            for line in &wrapped.lines {
                let mut displayed = text[line.range.clone()].to_owned();
                if let Some(suffix) = line.break_suffix {
                    displayed.push_str(suffix.as_str());
                }
                let expected = base
                    .clone()
                    .with_hyphenation_policy(HyphenationPolicy::Disabled)
                    .run(&displayed)?;
                assert_eq!(
                    line.glyphs
                        .iter()
                        .map(|glyph| glyph.glyph_id)
                        .collect::<Vec<_>>(),
                    expected.lines[0]
                        .glyphs
                        .iter()
                        .map(|glyph| glyph.glyph_id)
                        .collect::<Vec<_>>(),
                    "{language}: {displayed}"
                );
                assert_approx_eq(line.advance, expected.lines[0].advance);
            }
        }
        let authored = TextLayout::new(&font)
            .with_font_size(24.0)
            .run("ready-made\nword-")?;
        assert_eq!(authored.discretionary_hyphen_count(), 0);
        Ok(())
    }

    #[test]
    fn composition_cost_keeps_local_fragmentation_visible_in_long_dialogue() {
        for prefix_words in [8, 32] {
            let prefix = "calm ".repeat(prefix_words);
            let text = format!("{prefix}prosperity");
            let split = prefix.len();
            for scale in [0.5, 1.0, 2.0] {
                let candidate = |size: f32, ranges: &[Range<usize>]| LayoutRun {
                    lines: ranges
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(|(index, range)| LayoutLine {
                            advance: range.len() as f32 * size * 0.5,
                            break_suffix: (index + 1 < ranges.len())
                                .then_some(LineBreakSuffix::Hyphen),
                            range,
                            ..LayoutLine::default()
                        })
                        .collect(),
                    width: size * 4.0,
                    height: size * 3.0,
                    font_size: size,
                    overflowed: false,
                    placement_offset_x: 0.0,
                    placement_offset_y: 0.0,
                };
                let reference = 32.0 * scale;
                let broken = candidate(
                    reference,
                    &[0..split + 3, split + 3..split + 6, split + 6..text.len()],
                );
                let clean = candidate(26.0 * scale, std::slice::from_ref(&(0..text.len())));
                assert!(
                    clean.composition_cost(&text, reference)
                        < broken.composition_cost(&text, reference),
                    "{prefix_words} intact words diluted a twice-broken word at scale {scale}"
                );
            }
        }
    }

    #[test]
    fn compact_hyphens_measure_the_fragments_actually_drawn() {
        let text = "characteristically";
        let layout = LayoutRun {
            lines: [0..9, 9..11, 11..text.len()]
                .into_iter()
                .enumerate()
                .map(|(index, range)| LayoutLine {
                    break_suffix: (index < 2).then_some(LineBreakSuffix::Hyphen),
                    range,
                    ..LayoutLine::default()
                })
                .collect(),
            width: 100.0,
            height: 100.0,
            font_size: 24.0,
            overflowed: false,
            placement_offset_x: 0.0,
            placement_offset_y: 0.0,
        };
        let quality = comic_hyphen_quality(text, &layout);
        assert_eq!(quality.total, 2);
        assert_eq!(
            quality.compact, 2,
            "the two-letter middle fragment was missed"
        );
    }

    #[test]
    fn composition_cost_keeps_automatic_terminal_punctuation_attached() -> anyhow::Result<()> {
        let font = any_system_font();
        for punctuation in ["...!", "？！", "…"] {
            let text = format!("Begin now{punctuation}");
            let authored_text = format!("Begin now\n{punctuation}");
            let authored = TextLayout::new(&font)
                .with_font_size(32.0)
                .run(&authored_text)?;
            let mut separated = authored.clone();
            // Reuse the measured lines without the explicit newline to model
            // a punctuation-only continuation created by the line breaker.
            separated.lines[1].range.start -= 1;
            separated.lines[1].range.end -= 1;
            let clean = TextLayout::new(&font).with_font_size(26.0).run(&text)?;
            assert!(
                clean.composition_cost(&text, 32.0) < separated.composition_cost(&text, 32.0),
                "an automatic punctuation-only line was preferred: {punctuation}"
            );
            assert_eq!(authored.composition_cost(&authored_text, 32.0), 0.0);
            let symbol = TextLayout::new(&font)
                .with_font_size(32.0)
                .run(punctuation)?;
            assert_eq!(symbol.composition_cost(punctuation, 32.0), 0.0);
        }
        Ok(())
    }

    #[test]
    fn composition_search_reuses_fits_when_the_reference_changes() -> anyhow::Result<()> {
        use std::{cell::RefCell, collections::BTreeMap};
        let font = any_system_font();
        let measurements = RefCell::new(BTreeMap::new());
        let mut search = CompositionSearch::new(6.0, 96.0, 48.0, |size| {
            *measurements.borrow_mut().entry(size.to_bits()).or_insert(0) += 1;
            let layout = TextLayout::new(&font).with_font_size(size).run("One two")?;
            // A nonmonotonic gap must stay infeasible across both passes.
            Ok((size <= 24.0 && !(21.0..23.0).contains(&size)).then_some(layout))
        })?;
        let nearest = search
            .select(48.0, None, |layout| CompositionQuality {
                cost: font_scale_cost(layout.font_size, 48.0),
                ..CompositionQuality::default()
            })?
            .unwrap()
            .font_size;
        let quality_at = |layout: &LayoutRun<'_>| CompositionQuality {
            cost: font_scale_cost(layout.font_size, nearest)
                + (layout.font_size / 9.0).sin().powi(2),
            ..CompositionQuality::default()
        };
        let selected = search.select(nearest, None, quality_at)?.unwrap().font_size;
        let visited = measurements.borrow().len();
        assert_eq!(
            search.select(nearest, None, quality_at)?.unwrap().font_size,
            selected
        );
        assert_eq!(measurements.borrow().len(), visited);
        assert!(measurements.borrow().values().all(|count| *count == 1));
        let exhaustive = composition_font_sizes(6.0, 96.0, 48.0)
            .into_iter()
            .filter(|size| *size <= 24.0 && !(21.0..23.0).contains(size))
            .map(|size| TextLayout::new(&font).with_font_size(size).run("One two"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .min_by(|left, right| quality_at(left).compare(quality_at(right)))
            .unwrap();
        assert_eq!(selected, exhaustive.font_size);
        Ok(())
    }

    #[test]
    fn composition_search_measures_a_shared_reference_exactly() -> anyhow::Result<()> {
        use std::cell::Cell;
        let font = any_system_font();
        let count = Cell::new(0);
        let mut search = CompositionSearch::new(6.0, 96.0, 48.0, |size| {
            count.set(count.get() + 1);
            TextLayout::new(&font)
                .with_font_size(size)
                .run("Text")
                .map(Some)
        })?;
        for reference in [48.0, 23.17, 48.0] {
            let selected = search
                .select(reference, None, |layout| CompositionQuality {
                    cost: font_scale_cost(layout.font_size, reference),
                    ..CompositionQuality::default()
                })?
                .unwrap();
            assert_eq!(selected.font_size, reference);
        }
        assert_eq!(count.get(), 2);
        Ok(())
    }

    #[test]
    fn composition_search_does_not_cache_measurement_errors_as_failed_fits() -> anyhow::Result<()> {
        use std::cell::Cell;
        let font = any_system_font();
        let count = Cell::new(0);
        let mut search = CompositionSearch::new(6.0, 96.0, 48.0, |size| {
            count.set(count.get() + 1);
            anyhow::ensure!(count.get() > 1, "measurement failed");
            TextLayout::new(&font)
                .with_font_size(size)
                .run("Text")
                .map(Some)
        })?;
        let quality_at = |layout: &LayoutRun<'_>| CompositionQuality {
            cost: font_scale_cost(layout.font_size, 48.0),
            ..CompositionQuality::default()
        };
        assert!(search.select(48.0, None, quality_at).is_err());
        assert_eq!(
            search.select(48.0, None, quality_at)?.unwrap().font_size,
            48.0
        );
        assert_eq!(count.get(), 2);
        Ok(())
    }

    #[test]
    fn composition_search_incumbent_prunes_only_worse_footprints() -> anyhow::Result<()> {
        use std::cell::Cell;
        let font = any_system_font();
        let count = Cell::new(0);
        let reference = 48.0;
        let incumbent = CompositionQuality {
            cost: font_scale_cost(40.0, reference),
            ..CompositionQuality::default()
        };
        let mut search = CompositionSearch::new(6.0, 24.0, reference, |size| {
            count.set(count.get() + 1);
            TextLayout::new(&font)
                .with_font_size(size)
                .run("One two")
                .map(Some)
        })?;
        let quality_at = |layout: &LayoutRun<'_>| CompositionQuality {
            cost: font_scale_cost(layout.font_size, reference),
            ..CompositionQuality::default()
        };
        assert!(
            search
                .select(reference, Some(incumbent), quality_at)?
                .is_none()
        );
        assert_eq!(
            count.get(),
            0,
            "a worse scale interval was shaped unnecessarily"
        );
        // A footprint that can honor an explicit line count outranks scale cost.
        let relaxed = CompositionQuality {
            requested_lines_error: 1,
            ..incumbent
        };
        assert_eq!(
            search
                .select(reference, Some(relaxed), quality_at)?
                .unwrap()
                .font_size,
            24.0
        );
        Ok(())
    }

    #[test]
    fn composition_search_pruning_matches_the_complete_frontier() -> anyhow::Result<()> {
        use std::cell::Cell;
        let font = any_system_font();
        let reference = 24.0;
        let samples = composition_font_sizes(6.0, 96.0, reference);
        for requested in [None, Some(2)] {
            let count = Cell::new(0);
            let layout_at = |size: f32| {
                count.set(count.get() + 1);
                let text = if size < 18.0 { "One\nTwo" } else { "One Two" };
                TextLayout::new(&font).with_font_size(size).run(text)
            };
            let quality_at = |layout: &LayoutRun<'_>| CompositionQuality {
                requested_lines_error: requested
                    .map_or(0, |lines| layout.lines.len().abs_diff(lines)),
                cost: font_scale_cost(layout.font_size, reference)
                    + (layout.font_size / 9.0).sin().powi(2),
                ..CompositionQuality::default()
            };
            let selected =
                CompositionSearch::new(6.0, 96.0, reference, |size| layout_at(size).map(Some))?
                    .select(reference, None, quality_at)?
                    .cloned()
                    .unwrap();
            let visited = count.get();
            let exhaustive = samples
                .iter()
                .map(|size| layout_at(*size))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .min_by(|left, right| quality_at(left).compare(quality_at(right)))
                .unwrap();
            assert_eq!(selected.font_size, exhaustive.font_size);
            assert!(
                visited < samples.len(),
                "the lower bound did not prune any candidates"
            );
        }
        Ok(())
    }

    #[test]
    fn comic_quality_uses_only_a_small_font_trade_for_materially_better_centering() {
        let font = any_system_font();
        let candidate = |font_size, placement_offset_y| LayoutRun {
            lines: vec![LayoutLine {
                range: 0..4,
                ..LayoutLine::default()
            }],
            width: 40.0,
            height: 20.0,
            font_size,
            overflowed: false,
            placement_offset_x: 0.0,
            placement_offset_y,
        };
        let larger = candidate(20.0, 0.0);
        let centered = candidate(19.0, 9.0);
        let too_small = candidate(17.0, 10.0);
        let layout = TextLayout::new(&font)
            .with_comic_balloon(
                80.0,
                80.0,
                vec![(0.0, 0.0), (80.0, 0.0), (80.0, 80.0), (0.0, 80.0)],
                0.0,
            )
            .with_comic_preferred_center((40.0, 50.0), false);

        let selected = best_test_composition(&layout, "text", &[larger, centered, too_small]);

        assert_eq!(selected.font_size, 19.0);
        assert_eq!(layout.comic_center_error(&selected), 1.0);
    }

    #[test]
    fn comic_quality_keeps_size_for_negligible_center_error_in_a_tall_balloon() {
        let font = any_system_font();
        let candidate = |font_size, placement_offset_y| LayoutRun {
            lines: vec![LayoutLine {
                range: 0..4,
                ..LayoutLine::default()
            }],
            width: 40.0,
            height: 20.0,
            font_size,
            overflowed: false,
            placement_offset_x: 0.0,
            placement_offset_y,
        };
        let larger = candidate(19.0, 0.0);
        let exactly_centered = candidate(18.0, 4.0);
        let layout = TextLayout::new(&font)
            .with_comic_balloon(
                80.0,
                200.0,
                vec![(0.0, 0.0), (80.0, 0.0), (80.0, 200.0), (0.0, 200.0)],
                0.0,
            )
            .with_comic_preferred_center((40.0, 104.0), false);

        let selected = best_test_composition(&layout, "text", &[larger, exactly_centered]);

        assert_eq!(selected.font_size, 19.0);
        assert_eq!(layout.comic_center_error(&selected), 4.0);
    }

    #[test]
    fn comic_quality_anchors_center_tolerance_to_the_best_fitted_candidate() {
        let font = any_system_font();
        let text = "lettering";
        let boundary = 4;
        let candidate =
            |font_size, ranges: &[std::ops::Range<usize>], placement_offset_y| LayoutRun {
                lines: ranges
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(|(index, range)| LayoutLine {
                        break_suffix: (index + 1 < ranges.len()).then_some(LineBreakSuffix::Hyphen),
                        range,
                        ..LayoutLine::default()
                    })
                    .collect(),
                width: 40.0,
                height: 20.0,
                font_size,
                overflowed: false,
                placement_offset_x: 0.0,
                placement_offset_y,
            };
        let displaced_largest = candidate(22.0, &[0..boundary, boundary..text.len()], 25.0);
        let centered_readable = candidate(21.0, &[0..boundary, boundary..text.len()], 0.0);
        let displaced_clean = candidate(16.0, std::slice::from_ref(&(0..text.len())), 17.0);
        let layout = TextLayout::new(&font)
            .with_comic_balloon(
                80.0,
                80.0,
                vec![(0.0, 0.0), (80.0, 0.0), (80.0, 80.0), (0.0, 80.0)],
                0.0,
            )
            .with_comic_preferred_center((40.0, 40.0), false);

        let selected = best_test_composition(
            &layout,
            text,
            &[displaced_largest, centered_readable, displaced_clean],
        );

        assert_eq!(selected.font_size, 21.0);
        assert_eq!(layout.comic_center_error(&selected), 0.0);
    }

    #[test]
    fn composition_search_keeps_owned_flow_centering_authoritative() {
        let font = any_system_font();
        let candidate = |font_size, placement_offset_y| LayoutRun {
            lines: vec![LayoutLine {
                range: 0..4,
                ..LayoutLine::default()
            }],
            width: 40.0,
            height: 20.0,
            font_size,
            overflowed: false,
            placement_offset_x: 0.0,
            placement_offset_y,
        };
        let base = TextLayout::new(&font).with_comic_balloon(
            80.0,
            80.0,
            vec![(0.0, 0.0), (80.0, 0.0), (80.0, 80.0), (0.0, 80.0)],
            0.0,
        );
        let readable = candidate(15.0, 0.0);
        let emergency_local = candidate(12.0, 10.0);

        let soft_anchor = base
            .clone()
            .with_comic_preferred_center((40.0, 50.0), false);
        let selected = best_test_composition(
            &soft_anchor,
            "text",
            &[readable.clone(), emergency_local.clone()],
        );
        assert_eq!(selected.font_size, 15.0);

        let owned_flow = base.with_comic_preferred_center((40.0, 50.0), true);
        let selected = best_test_composition(&owned_flow, "text", &[readable, emergency_local]);
        assert_eq!(selected.font_size, 12.0);
        assert!(owned_flow.comic_layout_is_centered(&selected));
    }

    #[test]
    fn comic_auto_fit_stays_in_the_assigned_lobe_without_an_ocr_position() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "Başta Haruto'ya karşı iğrenti duymuştum ama";
        let width = 255.0;
        let height = 637.0;
        let flow_center = (71.0, (338.0 + height) * 0.5);
        let contour = vec![
            (99.0, 0.0),
            (255.0, 0.0),
            (255.0, 338.0),
            (142.0, 338.0),
            (142.0, 637.0),
            (0.0, 637.0),
            (0.0, 338.0),
            (99.0, 338.0),
        ];
        let base = || {
            TextLayout::new(&font)
                .with_min_font_size(12.0)
                .with_max_font_size(27.0)
                .with_line_height(1.2)
                .with_language_tag("tr-TR")
                .with_hyphenation_policy(HyphenationPolicy::LastResort)
                .with_alignment(TextAlign::Center)
                .with_max_width(width)
                .with_max_height(height)
                .with_comic_balloon(width, height, contour.clone(), 0.0)
        };
        let unconstrained = base().run(text)?;
        let source_local = base()
            .with_comic_balloon_constraints(
                width,
                height,
                vec![
                    contour.clone(),
                    vec![(0.0, 338.0), (width, 338.0), (width, height), (0.0, height)],
                ],
                0.0,
            )
            .run(text)?;
        let unconstrained_center = height * 0.5 + unconstrained.placement_offset_y();
        let source_local_center = height * 0.5 + source_local.placement_offset_y();

        assert!(!source_local.overflowed());
        assert!(
            (source_local_center - flow_center.1).abs()
                <= source_local.font_size * COMIC_CENTER_ERROR_TOLERANCE_EM + 0.01
        );
        assert!(source_local_center - source_local.height * 0.5 >= 338.0);
        assert!(
            unconstrained_center + unconstrained.font_size * 2.0 < source_local_center,
            "unconstrained center {unconstrained_center}, source-local center {source_local_center}"
        );
        Ok(())
    }

    #[test]
    fn comic_auto_size_does_not_fake_capacity_with_extra_leading() -> anyhow::Result<()> {
        let font = any_system_font();
        let font_size = 16.0;
        let layout = TextLayout::new(&font)
            .with_max_font_size(font_size)
            .with_min_font_size(font_size)
            .with_line_height(1.2)
            .with_alignment(TextAlign::Center)
            .with_max_width(200.0)
            .with_max_height(150.0)
            .with_comic_balloon(
                200.0,
                150.0,
                vec![(0.0, 0.0), (200.0, 0.0), (200.0, 150.0), (0.0, 150.0)],
                10.0,
            )
            .run("First\nSecond\nThird")?;

        let leading = layout.lines[1].baseline.1 - layout.lines[0].baseline.1;
        assert_approx_eq(leading, font_size * 1.2);
        Ok(())
    }

    #[test]
    fn comic_auto_size_preserves_air_for_an_already_filled_layout() -> anyhow::Result<()> {
        let font = any_system_font();
        let font_size = 16.0;
        let layout = TextLayout::new(&font)
            .with_max_font_size(font_size)
            .with_min_font_size(font_size)
            .with_line_height(1.2)
            .with_alignment(TextAlign::Center)
            .with_max_width(200.0)
            .with_max_height(150.0)
            .with_comic_balloon(
                200.0,
                150.0,
                vec![(0.0, 0.0), (200.0, 0.0), (200.0, 150.0), (0.0, 150.0)],
                10.0,
            )
            .run("First\nSecond\nThird\nFourth\nFifth\nSixth")?;

        let leading = layout.lines[1].baseline.1 - layout.lines[0].baseline.1;
        assert_approx_eq(leading, font_size * 1.2);
        Ok(())
    }

    #[test]
    fn comic_breaks_prefer_natural_pauses() {
        let mut segments = vec![
            LineBreakMeasure {
                advance: 30.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            };
            5
        ];
        segments[2].break_penalty = 500.0;

        assert_eq!(optimal_line_breaks(&segments, 90.0), vec![2, 5]);
        assert!(natural_break_penalty("Stop! Now", 6) < natural_break_penalty("go and", 3));
        assert!(natural_break_penalty("go and", 3) < natural_break_penalty("hello world", 6));
        assert!(natural_break_penalty("hello world", 6) < natural_break_penalty("the word", 4));
        assert!(has_internal_natural_pause("Wait~ Next"));
        assert!(natural_pause_at("Wait~ Next", 6));
        assert!(has_internal_natural_pause("Wait... Next"));
        assert!(!has_internal_natural_pause("Finished..."));
    }

    #[test]
    fn comic_breaks_use_a_coherent_block_in_both_writing_directions() {
        let segments = [LineBreakMeasure {
            advance: 40.0,
            trailing_advance: 0.0,
            break_suffix_advance: 0.0,
            break_penalty: 0.0,
            is_mandatory: false,
        }; 4];
        let balloon = comic_balloon(
            100.0,
            200.0,
            vec![(0.0, 0.0), (100.0, 0.0), (100.0, 200.0), (0.0, 200.0)],
            0.0,
        );

        let horizontal = comic_line_breaks(
            &LineMeasurements::additive(&segments),
            &balloon,
            WritingMode::Horizontal,
            24.0,
            InkBand {
                before: 10.0,
                after: 10.0,
            },
            (0.0, 0.0),
            HyphenationPolicy::Disabled,
        );
        let vertical_balloon = comic_balloon(
            200.0,
            100.0,
            vec![(0.0, 0.0), (200.0, 0.0), (200.0, 100.0), (0.0, 100.0)],
            0.0,
        );
        let vertical = comic_line_breaks(
            &LineMeasurements::additive(&segments),
            &vertical_balloon,
            WritingMode::VerticalRl,
            24.0,
            InkBand {
                before: 10.0,
                after: 10.0,
            },
            (0.0, 0.0),
            HyphenationPolicy::Disabled,
        );

        assert_eq!(horizontal.breaks, [2, 4]);
        assert_eq!(vertical.breaks, [2, 4]);
    }

    #[test]
    fn last_resort_hyphenation_is_used_only_to_avoid_overflow() {
        let fits_without_hyphen = [
            LineBreakMeasure {
                advance: 30.0,
                trailing_advance: 0.0,
                break_suffix_advance: 5.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 30.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
        ];
        let unbroken = line_breaks_with_policy(
            &LineMeasurements::additive(&fits_without_hyphen),
            70.0,
            HyphenationPolicy::LastResort,
        );
        assert_eq!(unbroken.breaks, [2]);
        assert!(!unbroken.overflowed);

        let needs_hyphen = [
            LineBreakMeasure {
                advance: 55.0,
                trailing_advance: 0.0,
                break_suffix_advance: 10.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 55.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
        ];
        let hyphenated = line_breaks_with_policy(
            &LineMeasurements::additive(&needs_hyphen),
            70.0,
            HyphenationPolicy::LastResort,
        );
        assert_eq!(hyphenated.breaks, [1, 2]);
        assert!(!hyphenated.overflowed);
    }

    #[test]
    fn comic_auto_fit_uses_hyphenation_to_preserve_readable_size() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "A healthy mind dwells in a healthy body. Exercise is important!";
        let layout = |policy| {
            TextLayout::new(&font)
                .with_max_font_size(36.0)
                .with_min_font_size(8.0)
                .with_line_height(1.2)
                .with_language_tag("en")
                .with_hyphenation_policy(policy)
                .with_alignment(TextAlign::Center)
                .with_max_width(125.0)
                .with_max_height(220.0)
                .with_comic_balloon(
                    125.0,
                    220.0,
                    vec![(0.0, 0.0), (125.0, 0.0), (125.0, 220.0), (0.0, 220.0)],
                    4.0,
                )
                .run(text)
        };
        let unhyphenated = layout(HyphenationPolicy::Disabled)?;
        let hyphenated = layout(HyphenationPolicy::LastResort)?;

        assert!(!hyphenated.overflowed());
        assert!(hyphenated.font_size + 0.01 >= unhyphenated.font_size);
        assert!(hyphenated.lines.windows(2).any(|lines| {
            let before = text[..lines[0].range.end].chars().next_back();
            let after = text[lines[1].range.start..].chars().next();
            matches!((before, after), (Some(left), Some(right)) if left.is_alphabetic() && right.is_alphabetic())
        }));
        Ok(())
    }

    #[test]
    fn comic_rectangular_preflight_preserves_alternate_legal_line_counts() -> anyhow::Result<()> {
        let font = any_system_font();
        let minimum = 10.0;
        let maximum = 48.0;
        let spacing = -2.0;
        let preflight_line = "letter letter letter letter";
        let measured = TextLayout::new(&font)
            .with_font_size(minimum)
            .with_spacing(spacing, 0.0)
            .run(preflight_line)?;
        let width = (measured.lines[0].advance + measured.width) * 0.5;
        assert!(measured.lines[0].advance < width && width < measured.width);
        let height = 240.0;
        let text = "letter letter letter letter letter letter letter letter";
        let rectangle = vec![(0.0, 0.0), (width, 0.0), (width, height), (0.0, height)];
        let base = || {
            TextLayout::new(&font)
                .with_max_width(width)
                .with_max_height(height)
                .with_spacing(spacing, 0.0)
                .with_hyphenation_policy(HyphenationPolicy::Disabled)
        };
        let uniform = base()
            .with_max_font_size(maximum)
            .with_min_font_size(minimum)
            .run(text)?;
        assert_eq!(uniform.font_size, minimum);
        assert!(uniform.overflowed());

        let comic = base().with_comic_balloon(width, height, rectangle, 0.0);
        let minimum_comic = comic.clone().with_font_size(minimum).run(text)?;
        let envelope = comic.comic_rectangular_search_maximum(
            text,
            minimum,
            maximum,
            HyphenationPolicy::Disabled,
        )?;
        let fitted = comic
            .with_max_font_size(maximum)
            .with_min_font_size(minimum)
            .run(text)?;

        assert!(
            !fitted.overflowed(),
            "measured advance {}, measured ink {}, width {}, minimum comic {}x{} lines={} overflow={}, fitted size {} lines={}",
            measured.lines[0].advance,
            measured.width,
            width,
            minimum_comic.width,
            minimum_comic.height,
            minimum_comic.lines.len(),
            minimum_comic.overflowed(),
            fitted.font_size,
            fitted.lines.len(),
        );
        assert!(fitted.font_size.floor() > minimum);
        assert!(envelope + f32::EPSILON >= fitted.font_size.ceil());
        Ok(())
    }

    #[test]
    fn comic_rectangular_preflight_encloses_the_inset_physical_wall() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "Lettering";
        let target_size = 20.0;
        let measured = TextLayout::new(&font)
            .with_font_size(target_size)
            .with_hyphenation_policy(HyphenationPolicy::Disabled)
            .run(text)?;
        let width = measured.width.max(measured.lines[0].advance) + 0.25;
        let height = 160.0;
        let wall_extension = 32.0;
        let minimum = 10.0;
        let maximum = 40.0;
        let contour = vec![
            (-wall_extension, -wall_extension),
            (width + wall_extension, -wall_extension),
            (width + wall_extension, height + wall_extension),
            (-wall_extension, height + wall_extension),
        ];
        let layout = TextLayout::new(&font)
            .with_max_width(width)
            .with_max_height(height)
            .with_hyphenation_policy(HyphenationPolicy::Disabled)
            .with_comic_balloon(width, height, contour, 4.0);
        let fits = |candidate: &LayoutRun<'_>| {
            !candidate.overflowed()
                && candidate.width <= width + f32::EPSILON
                && candidate.height <= height + f32::EPSILON
        };
        let exhaustive = largest_fitting_font_size(
            minimum,
            maximum,
            |size| layout.run_with_size(text, size),
            fits,
        )?
        .expect("the physical contour has a fitting candidate");
        let envelope = layout.comic_rectangular_search_maximum(
            text,
            minimum,
            maximum,
            HyphenationPolicy::Disabled,
        )?;

        assert!(exhaustive.font_size + f32::EPSILON >= target_size);
        assert!(
            envelope + f32::EPSILON >= exhaustive.font_size.ceil(),
            "preflight {envelope} excluded exhaustive fit {}",
            exhaustive.font_size,
        );
        Ok(())
    }

    #[test]
    fn comic_auto_fit_hyphenates_near_the_readability_floor() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "For the villagers, it's a common occurrence.";
        let minimum = 9.0;
        let layout = |policy| {
            TextLayout::new(&font)
                .with_max_font_size(80.0)
                .with_min_font_size(minimum)
                .with_line_height(1.2)
                .with_language_tag("en")
                .with_hyphenation_policy(policy)
                .with_alignment(TextAlign::Center)
                .with_max_width(60.0)
                .with_max_height(180.0)
                .with_comic_balloon(
                    60.0,
                    180.0,
                    vec![(0.0, 0.0), (60.0, 0.0), (60.0, 180.0), (0.0, 180.0)],
                    4.0,
                )
                .run(text)
        };
        let clean = layout(HyphenationPolicy::Disabled)?;
        let last_resort = layout(HyphenationPolicy::LastResort)?;

        assert!(
            clean.overflowed() || clean.font_size <= minimum * 1.25 + f32::EPSILON,
            "the clean layout no longer exercises the readability boundary: minimum {minimum}, clean size {}",
            clean.font_size
        );
        assert!(!last_resort.overflowed());
        assert!(
            clean.overflowed() || last_resort.font_size.floor() > clean.font_size.floor(),
            "clean size {} ({} lines), LastResort size {} ({} lines)",
            clean.font_size,
            clean.lines.len(),
            last_resort.font_size,
            last_resort.lines.len()
        );
        assert!(last_resort.lines.windows(2).any(|lines| {
            let before = text[..lines[0].range.end].chars().next_back();
            let after = text[lines[1].range.start..].chars().next();
            matches!((before, after), (Some(left), Some(right)) if left.is_alphabetic() && right.is_alphabetic())
        }));
        Ok(())
    }

    #[test]
    fn turkish_comic_auto_fit_uses_hyphenation_to_preserve_readability() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "Artan hayat pahalılığı karşısında sorumluluklarımızı düşünmeliyiz.";
        let layout = |policy| {
            TextLayout::new(&font)
                .with_max_font_size(80.0)
                .with_min_font_size(9.0)
                .with_line_height(1.2)
                .with_language_tag("tr-TR")
                .with_hyphenation_policy(policy)
                .with_alignment(TextAlign::Center)
                .with_max_width(68.0)
                .with_max_height(180.0)
                .with_comic_balloon(
                    68.0,
                    180.0,
                    vec![(0.0, 0.0), (68.0, 0.0), (68.0, 180.0), (0.0, 180.0)],
                    4.0,
                )
                .run(text)
        };
        let clean = layout(HyphenationPolicy::Disabled)?;
        let last_resort = layout(HyphenationPolicy::LastResort)?;
        let discretionary_hyphens = last_resort.discretionary_hyphen_count();

        assert!(!last_resort.overflowed());
        assert!(
            discretionary_hyphens > 0,
            "the Turkish layout did not use a discretionary hyphen"
        );
        assert!(
            clean.overflowed() || last_resort.font_size.floor() > clean.font_size.floor(),
            "hyphenation neither resolved overflow nor recovered a visible font pixel: clean size {} (overflow={}), LastResort size {}",
            clean.font_size,
            clean.overflowed(),
            last_resort.font_size
        );
        Ok(())
    }

    #[test]
    fn short_comic_text_keeps_clean_words_when_an_unhyphenated_setting_fits() -> anyhow::Result<()>
    {
        let font = any_system_font();
        let text = "A cat tower?";
        let layout = |policy| {
            TextLayout::new(&font)
                .with_max_font_size(24.0)
                .with_min_font_size(8.0)
                .with_language_tag("en")
                .with_hyphenation_policy(policy)
                .with_alignment(TextAlign::Center)
                .with_max_width(220.0)
                .with_max_height(120.0)
                .with_comic_balloon(
                    220.0,
                    120.0,
                    vec![(0.0, 0.0), (220.0, 0.0), (220.0, 120.0), (0.0, 120.0)],
                    4.0,
                )
                .run(text)
        };
        let unhyphenated = layout(HyphenationPolicy::Disabled)?;
        let hyphenated = layout(HyphenationPolicy::LastResort)?;

        assert!(!hyphenated.overflowed());
        assert_eq!(unhyphenated.font_size, 24.0);
        assert!(
            (hyphenated.font_size - unhyphenated.font_size).abs() < 0.01,
            "clean size {} ({} lines), LastResort size {} ({} lines)",
            unhyphenated.font_size,
            unhyphenated.lines.len(),
            hyphenated.font_size,
            hyphenated.lines.len()
        );
        let tower = text.find("tower").unwrap();
        assert!(
            !hyphenated
                .lines
                .iter()
                .any(|line| (tower + 1..tower + "tower".len()).contains(&line.range.end))
        );
        Ok(())
    }

    #[test]
    fn mandatory_breaks_are_respected_by_the_global_balloon_profile() {
        let segments = [
            LineBreakMeasure {
                advance: 20.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: true,
            },
            LineBreakMeasure {
                advance: 20.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 5.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
        ];
        let profiles = vec![
            LineProfile {
                width: 50.0,
                center_offset: 0.0,
                block_baseline: 0.0,
            },
            LineProfile {
                width: 25.0,
                center_offset: 0.0,
                block_baseline: 0.0,
            },
        ];

        let result =
            exact_profiled_line_breaks(&LineMeasurements::additive(&segments), profiles, true)
                .unwrap();

        assert_eq!(result.breaks, [1, 3]);
    }

    #[test]
    fn a_discretionary_suffix_does_not_hide_a_later_unbroken_fit() {
        let segments = [
            LineBreakMeasure {
                advance: 30.0,
                trailing_advance: 0.0,
                break_suffix_advance: 5.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 25.0,
                trailing_advance: 0.0,
                break_suffix_advance: 20.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
            LineBreakMeasure {
                advance: 5.0,
                trailing_advance: 0.0,
                break_suffix_advance: 0.0,
                break_penalty: 0.0,
                is_mandatory: false,
            },
        ];

        let result = line_breaks_with_policy(
            &LineMeasurements::additive(&segments),
            65.0,
            HyphenationPolicy::Normal,
        );

        assert_eq!(result.breaks, [3]);
        assert!(!result.overflowed);
    }

    #[test]
    fn comic_balloon_reports_when_relative_air_cannot_fit() -> anyhow::Result<()> {
        let font = any_system_font();
        let layout = TextLayout::new(&font)
            .with_font_size(16.0)
            .with_max_width(100.0)
            .with_max_height(10.0)
            .with_comic_balloon(
                100.0,
                10.0,
                vec![(0.0, 0.0), (100.0, 0.0), (100.0, 10.0), (0.0, 10.0)],
                4.0,
            )
            .run("Hi")?;

        assert!(layout.overflowed());
        Ok(())
    }

    #[test]
    fn comic_balloon_air_preserves_the_representative_extent_and_explicit_floor() {
        let balloon = comic_balloon(100.0, 100.0, Vec::new(), 4.0);

        assert_approx_eq(balloon.air(12.0), 12.0);
        assert_approx_eq(balloon.air(24.0), 24.0);
        assert_approx_eq(balloon.air(3.0), 4.0);
    }

    #[test]
    fn comic_wall_air_uses_axis_specific_font_extents() -> anyhow::Result<()> {
        let mut fonts = FontSystem::new();
        let font = ["Arial", "DejaVu Sans", "Liberation Sans"]
            .into_iter()
            .find_map(|family| fonts.query_family(family).ok())
            .expect("a proportional Latin test font should be available");
        let font_size = 24.0;

        let horizontal_plain = TextLayout::new(&font).with_font_size(font_size).run("H")?;
        let horizontal_advance = horizontal_plain.lines[0].advance;
        let horizontal_width = horizontal_advance * 2.0 + COMIC_LINE_RASTER_TOLERANCE;
        let horizontal_height = font_size * 5.0;
        let horizontal = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_max_width(horizontal_width)
            .with_max_height(horizontal_height)
            .with_comic_balloon(
                horizontal_width,
                horizontal_height,
                vec![
                    (0.0, 0.0),
                    (horizontal_width, 0.0),
                    (horizontal_width, horizontal_height),
                    (0.0, horizontal_height),
                ],
                0.0,
            )
            .run("H")?;
        assert!(!horizontal.overflowed());

        let vertical_layout = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_writing_mode(WritingMode::VerticalRl);
        let vertical_plain = vertical_layout.clone().run("H")?;
        let vertical_advance = vertical_plain.lines[0].advance;
        let (vertical_ink_start, vertical_ink_end) = vertical_layout
            .inline_ink_bounds(font_size, &vertical_plain.lines[0])
            .expect("test glyph should have vertical ink bounds");
        let vertical_ink_extent = vertical_ink_end - vertical_ink_start;
        assert!(vertical_advance > horizontal_advance + COMIC_LINE_RASTER_TOLERANCE * 3.0);
        let vertical_width = font_size * 5.0;
        // Reusing the horizontal advance as total vertical wall air would leave
        // enough room for the painted glyph. Axis-specific vertical air leaves
        // less than the measured ink extent even after raster tolerance.
        let insufficient_height =
            horizontal_advance + vertical_ink_extent + COMIC_LINE_RASTER_TOLERANCE;
        let constrained = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_writing_mode(WritingMode::VerticalRl)
            .with_max_width(vertical_width)
            .with_max_height(insufficient_height)
            .with_comic_balloon(
                vertical_width,
                insufficient_height,
                vec![
                    (0.0, 0.0),
                    (vertical_width, 0.0),
                    (vertical_width, insufficient_height),
                    (0.0, insufficient_height),
                ],
                0.0,
            )
            .run("H")?;
        assert!(constrained.overflowed());

        let sufficient_height = vertical_advance * 2.0 + COMIC_LINE_RASTER_TOLERANCE;
        let vertical = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_writing_mode(WritingMode::VerticalRl)
            .with_max_width(vertical_width)
            .with_max_height(sufficient_height)
            .with_comic_balloon(
                vertical_width,
                sufficient_height,
                vec![
                    (0.0, 0.0),
                    (vertical_width, 0.0),
                    (vertical_width, sufficient_height),
                    (0.0, sufficient_height),
                ],
                0.0,
            )
            .run("H")?;
        assert!(!vertical.overflowed());
        Ok(())
    }

    #[test]
    #[ignore = "skipping for now"]
    fn layout_baselines_horizontal_follow_font_metrics() -> anyhow::Result<()> {
        let font = any_system_font();
        let font_size = 16.0;
        let layout = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_writing_mode(WritingMode::Horizontal)
            .run("A\nB\nC")?;

        assert!(layout.lines.len() >= 2);

        let metrics = font
            .skrifa_ref()?
            .metrics(Size::new(font_size), font.location());
        let ascent = metrics.ascent;
        let descent = -metrics.descent;
        let line_height = (ascent + descent + metrics.leading).max(font_size);

        let base_x = layout.lines[0].baseline.0;
        for line in &layout.lines {
            assert_approx_eq(line.baseline.0, base_x);
        }
        for i in 1..layout.lines.len() {
            let dy = layout.lines[i].baseline.1 - layout.lines[i - 1].baseline.1;
            assert_approx_eq(dy, line_height);
        }

        Ok(())
    }

    #[test]
    fn mandatory_newlines_are_not_shaped_as_glyphs() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "A\nB\nC";
        let layout = TextLayout::new(&font)
            .with_font_size(16.0)
            .with_writing_mode(WritingMode::Horizontal)
            .run(text)?;

        assert_eq!(layout.lines.len(), 3);
        for (line, expected) in layout.lines.iter().zip(["A", "B", "C"]) {
            assert_eq!(&text[line.range.clone()], expected);
            assert_eq!(line.glyphs.len(), 1);
        }

        Ok(())
    }

    #[test]
    #[ignore = "skipping for now"]
    fn layout_baselines_vertical_follow_font_metrics() -> anyhow::Result<()> {
        let font = any_system_font();
        let font_size = 16.0;
        let layout = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_writing_mode(WritingMode::VerticalRl)
            .run("A\nB\nC")?;

        assert!(layout.lines.len() >= 2);

        let metrics = font
            .skrifa_ref()?
            .metrics(Size::new(font_size), font.location());
        let ascent = metrics.ascent;
        let descent = -metrics.descent;
        let line_height = (ascent + descent + metrics.leading).max(font_size);
        let base_y = layout.lines[0].baseline.1;
        for line in &layout.lines {
            assert_approx_eq(line.baseline.1, base_y);
        }

        for i in 1..layout.lines.len() {
            let dx = layout.lines[i - 1].baseline.0 - layout.lines[i].baseline.0;
            assert_approx_eq(dx, line_height);
        }

        Ok(())
    }

    #[test]
    fn vertical_layout_preserves_original_source_offsets() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "！！A";
        let layout = TextLayout::new(&font)
            .with_font_size(16.0)
            .with_writing_mode(WritingMode::VerticalRl)
            .run(text)?;

        assert_eq!(layout.lines[0].range, 0..text.len());
        assert!(layout.lines[0].glyphs.iter().all(|glyph| {
            let cluster = glyph.cluster as usize;
            cluster <= text.len() && text.is_char_boundary(cluster)
        }));
        Ok(())
    }

    #[test]
    fn vertical_alignment_keeps_one_layout_box() -> anyhow::Result<()> {
        let font = any_system_font();
        let max_height = 100.0;
        let layout = |alignment| {
            TextLayout::new(&font)
                .with_font_size(16.0)
                .with_writing_mode(WritingMode::VerticalRl)
                .with_max_width(60.0)
                .with_max_height(max_height)
                .with_alignment(alignment)
                .run("AAAA\ng")
        };
        let start = layout(TextAlign::Left)?;
        let center = layout(TextAlign::Center)?;
        let end = layout(TextAlign::Right)?;

        assert!(start.height < max_height);
        assert_approx_eq(start.height, center.height);
        assert_approx_eq(center.height, end.height);
        assert_approx_eq(start.width, end.width);
        assert_approx_eq(start.placement_offset_y(), center.placement_offset_y());
        assert_approx_eq(center.placement_offset_y(), end.placement_offset_y());
        let metrics = TextLayout::new(&font);
        let line_top = |layout: &LayoutRun<'_>| {
            metrics
                .ink_bounds(16.0, std::slice::from_ref(&layout.lines[1]))
                .unwrap()
                .1
        };
        let start_top = line_top(&start);
        let center_top = line_top(&center);
        let end_top = line_top(&end);
        assert!(start_top < center_top);
        assert!(center_top < end_top);

        Ok(())
    }

    #[test]
    fn vertical_start_alignment_shares_one_visual_top() -> anyhow::Result<()> {
        let font = any_system_font();
        let layout = TextLayout::new(&font)
            .with_font_size(16.0)
            .with_writing_mode(WritingMode::VerticalRl)
            .with_alignment(TextAlign::Left)
            .with_max_width(80.0)
            .with_max_height(160.0)
            .with_comic_balloon(
                80.0,
                160.0,
                vec![(0.0, 0.0), (80.0, 0.0), (80.0, 160.0), (0.0, 160.0)],
                0.0,
            )
            .run("A\ng\nE")?;
        let metrics = TextLayout::new(&font);
        let tops = layout
            .lines
            .iter()
            .map(|line| {
                metrics
                    .ink_bounds(16.0, std::slice::from_ref(line))
                    .unwrap()
                    .1
            })
            .collect::<Vec<_>>();

        assert_eq!(tops.len(), 3);
        for top in &tops[1..] {
            assert_approx_eq(*top, tops[0]);
        }
        let (_, ink_top, _, ink_bottom) = metrics.ink_bounds(16.0, &layout.lines).unwrap();
        assert_approx_eq(ink_top, layout.height - ink_bottom);
        assert!(layout.height < 160.0);
        assert_approx_eq(layout.placement_offset_y(), 0.0);

        Ok(())
    }

    #[test]
    fn horizontal_alignment_keeps_one_layout_box() -> anyhow::Result<()> {
        let font = any_system_font();
        let max_width = 100.0;
        let layout = |alignment| {
            TextLayout::new(&font)
                .with_font_size(16.0)
                .with_max_width(max_width)
                .with_alignment(alignment)
                .run("AAAA\ng")
        };
        let start = layout(TextAlign::Left)?;
        let center = layout(TextAlign::Center)?;
        let end = layout(TextAlign::Right)?;
        let metrics = TextLayout::new(&font);
        let ink_left = |layout: &LayoutRun<'_>| {
            metrics
                .ink_bounds(16.0, std::slice::from_ref(&layout.lines[1]))
                .unwrap()
                .0
        };

        assert!(start.width < max_width);
        assert_approx_eq(start.width, center.width);
        assert_approx_eq(center.width, end.width);
        assert_approx_eq(start.placement_offset_x(), center.placement_offset_x());
        assert_approx_eq(center.placement_offset_x(), end.placement_offset_x());
        assert!(ink_left(&start) < ink_left(&center));
        assert!(ink_left(&center) < ink_left(&end));

        Ok(())
    }

    #[test]
    fn horizontal_center_alignment_centres_short_lines() -> anyhow::Result<()> {
        // Two lines of clearly different widths — a wide "HELLOWORLD" and
        // a narrow "HI". In a max_width wider than the long line, the
        // narrow line should be offset so its centre matches the long
        // line's centre (and the sprite centre).
        let font = any_system_font();
        let max_width = 400.0;
        let layout = TextLayout::new(&font)
            .with_font_size(20.0)
            .with_max_width(max_width)
            .with_alignment(TextAlign::Center)
            .run("HELLOWORLD\nHI")?;

        assert_eq!(layout.lines.len(), 2);
        let w0 = layout.lines[0].advance;
        let w1 = layout.lines[1].advance;
        let c0 = layout.lines[0].baseline.0 + w0 * 0.5;
        let c1 = layout.lines[1].baseline.0 + w1 * 0.5;
        // Line centres must coincide (within rounding / float slack).
        assert!(
            (c0 - c1).abs() < 1.0,
            "expected line centres to match, got c0={c0} c1={c1}",
        );
        Ok(())
    }

    #[test]
    fn unused_discretionary_breaks_preserve_whole_line_shaping() -> anyhow::Result<()> {
        let mut fonts = FontSystem::new();
        let font = fonts
            .query_family("DejaVu Serif")
            .or_else(|_| fonts.query_family("Times New Roman"))?;
        for text in ["office affinity", "AVATAR typography", "efficiently"] {
            let builder = TextLayout::new(&font)
                .with_font_size(32.0)
                .with_language_tag("en-US");
            let plain = builder
                .clone()
                .with_hyphenation_policy(HyphenationPolicy::Disabled)
                .run(text)?;
            let discretionary = builder
                .with_hyphenation_policy(HyphenationPolicy::Normal)
                .run(text)?;
            let glyphs = |layout: &LayoutRun<'_>| {
                layout.lines[0]
                    .glyphs
                    .iter()
                    .map(|glyph| {
                        (
                            glyph.glyph_id,
                            glyph.cluster,
                            glyph.x_advance,
                            glyph.x_offset,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(glyphs(&plain), glyphs(&discretionary), "text={text}");
        }
        Ok(())
    }

    #[test]
    fn candidate_line_widths_include_whole_words_and_break_suffixes() -> anyhow::Result<()> {
        let mut fonts = FontSystem::new();
        let font = fonts
            .query_family("DejaVu Serif")
            .or_else(|_| fonts.query_family("Times New Roman"))?;
        let text = "office affinity efficiently typography";
        let base = TextLayout::new(&font)
            .with_font_size(32.0)
            .with_language_tag("en-US");
        let full = base
            .clone()
            .with_hyphenation_policy(HyphenationPolicy::Disabled)
            .run(text)?;
        for ratio in [0.22, 0.45, 0.7] {
            let width = full.lines[0].advance * ratio;
            let layout = base.clone().with_max_width(width).run(text)?;
            for line in &layout.lines {
                let mut displayed = text[line.range.clone()].to_owned();
                if let Some(suffix) = line.break_suffix {
                    displayed.push_str(suffix.as_str());
                }
                let expected = base
                    .clone()
                    .with_hyphenation_policy(HyphenationPolicy::Disabled)
                    .run(&displayed)?;
                assert_eq!(
                    line.glyphs
                        .iter()
                        .map(|g| (g.glyph_id, g.cluster as usize - line.range.start))
                        .collect::<Vec<_>>(),
                    expected.lines[0]
                        .glyphs
                        .iter()
                        .map(|g| (g.glyph_id, g.cluster as usize))
                        .collect::<Vec<_>>(),
                    "{displayed}"
                );
                assert_approx_eq(line.advance, expected.lines[0].advance);
                assert!(
                    line.advance <= width + 0.001,
                    "{displayed}: {} > {width}",
                    line.advance
                );
            }
        }
        Ok(())
    }

    #[test]
    fn candidate_line_widths_need_not_grow_at_discretionary_boundaries() {
        let measures = LineMeasurements {
            segments: vec![
                BreakPoint {
                    hyphenated: true,
                    break_penalty: 0.0,
                    is_mandatory: false,
                },
                BreakPoint {
                    hyphenated: false,
                    break_penalty: 0.0,
                    is_mandatory: false,
                },
            ],
            // The shorter candidate includes a suffix; the complete word does not.
            advances: vec![vec![70.0, 60.0], vec![20.0]],
        };
        let uniform = optimal_uniform_line_breaks(&measures, 64.0, true);
        let profiled = exact_profiled_line_breaks(
            &measures,
            vec![LineProfile {
                width: 64.0,
                center_offset: 0.0,
                block_baseline: 0.0,
            }],
            true,
        )
        .unwrap();
        for result in [uniform, profiled] {
            assert!(!result.overflowed);
            assert_eq!(result.breaks, [2]);
        }
    }

    #[test]
    fn horizontal_layout_hyphenates_long_words() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "antidisestablishmentarianism";
        let font_size = 24.0;
        let unwrapped = TextLayout::new(&font).with_font_size(font_size).run(text)?;
        let max_width = (unwrapped.lines[0].advance * 0.45).max(font_size * 4.0);

        let layout = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_max_width(max_width)
            .run(text)?;

        assert!(
            layout.lines.len() > 1,
            "expected hyphenation to wrap long word, got {layout:?}"
        );
        for line in layout.lines.iter().take(layout.lines.len() - 1) {
            assert!(
                line.advance <= max_width + 1.0,
                "hyphenated line should fit max width {max_width}, got {}",
                line.advance
            );
        }
        assert!(
            layout
                .lines
                .iter()
                .take(layout.lines.len() - 1)
                .any(|line| line
                    .glyphs
                    .iter()
                    .any(|glyph| glyph.cluster as usize == line.range.end)),
            "expected a synthetic hyphen glyph at a discretionary break"
        );

        Ok(())
    }

    #[test]
    fn horizontal_layout_wraps_chinese_on_jieba_word_boundaries() -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "\u{5357}\u{4eac}\u{5e02}\u{957f}\u{6c5f}\u{5927}\u{6865}";
        let font_size = 24.0;
        let unwrapped = TextLayout::new(&font).with_font_size(font_size).run(text)?;
        let layout = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_max_width(unwrapped.lines[0].advance * 0.5)
            .run(text)?;

        assert!(
            layout.lines.len() > 1,
            "expected Chinese text to wrap, got {layout:?}"
        );
        assert_eq!(
            &text[layout.lines[0].range.clone()],
            "\u{5357}\u{4eac}\u{5e02}"
        );

        Ok(())
    }

    #[test]
    fn cjk_emphasis_normalization_only_expands_ascii_marks() {
        let source = "A!?！？⁉⁈‼⁇";
        let (normalized, cluster_map) = normalize_cjk_emphasis_punctuation(source).unwrap();

        assert_eq!(normalized, "A！？！？⁉⁈‼⁇");
        assert_eq!(
            normalized
                .char_indices()
                .map(|(offset, _)| cluster_map[offset])
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 6, 9, 12, 15, 18]
        );
        assert!(normalize_cjk_emphasis_punctuation("！？⁉⁈‼⁇").is_none());
    }

    #[test]
    fn cjk_emphasis_layout_keeps_glyphs_and_groups_at_most_three_per_vertical_row()
    -> anyhow::Result<()> {
        let font = any_system_font();
        let text = "中!?A!!!B????C⁉⁈‼⁇";
        let layout = TextLayout::new(&font)
            .with_font_size(32.0)
            .with_writing_mode(WritingMode::VerticalRl)
            .with_cjk_punctuation_layout(true)
            .run(text)?;
        let glyphs = layout
            .lines
            .iter()
            .flat_map(|line| &line.glyphs)
            .collect::<Vec<_>>();

        for (offset, _) in text.char_indices() {
            assert!(
                glyphs.iter().any(|glyph| glyph.cluster == offset as u32),
                "missing source glyph at byte offset {offset}"
            );
        }
        let y_advance = |cluster| {
            glyphs
                .iter()
                .find(|glyph| glyph.cluster == cluster)
                .unwrap()
                .y_advance
        };
        assert_approx_eq(y_advance(3), 0.0);
        assert!(y_advance(4) < 0.0);
        assert_approx_eq(y_advance(6), 0.0);
        assert_approx_eq(y_advance(7), 0.0);
        assert!(y_advance(8) < 0.0);
        assert_approx_eq(y_advance(10), 0.0);
        assert_approx_eq(y_advance(11), 0.0);
        assert!(y_advance(12) < 0.0);
        assert!(y_advance(13) < 0.0);
        Ok(())
    }

    #[test]
    fn cjk_emphasis_layout_applies_to_horizontal_text() -> anyhow::Result<()> {
        let font = any_system_font();
        let font_size = 32.0;
        let text = "中!?A!!!";
        let layout = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_cjk_punctuation_layout(true)
            .run(text)?;
        let glyphs = &layout.lines[0].glyphs;
        let punctuation = [3, 4, 6, 7, 8].map(|cluster| {
            glyphs
                .iter()
                .find(|glyph| glyph.cluster == cluster)
                .unwrap()
        });

        for glyph in punctuation {
            assert!(glyph.x_advance > 0.0);
        }
        let first = glyphs.iter().find(|glyph| glyph.cluster == 3).unwrap();
        let metrics = first
            .font
            .skrifa_ref()?
            .glyph_metrics(Size::new(font_size), first.font.location());
        let bounds = metrics
            .bounds(skrifa::GlyphId::new(first.glyph_id))
            .unwrap();
        assert_approx_eq(
            first.x_advance - (bounds.x_max - bounds.x_min),
            font_size * 0.04,
        );
        Ok(())
    }

    #[test]
    fn vertical_punctuation_centering_enabled_by_default() {
        let font = any_system_font();
        let layout = TextLayout::new(&font).with_font_size(16.0);
        assert!(layout.center_vertical_punctuation);
    }

    #[test]
    fn centered_x_offset_uses_absolute_center() {
        assert_approx_eq(centered_x_offset(2.0, 6.0), -4.0);
        assert_approx_eq(centered_x_offset(-3.0, 1.0), 1.0);
    }

    #[test]
    fn all_vertical_punctuation_is_centered_in_its_advance_cell() -> anyhow::Result<()> {
        let font = any_system_font();
        let font_size = 32.0;
        let text = "A,A.A;A:A!A?A-A_A(A)A[A]A{A}A，A。A！A？A—A“A”A";
        let layout = TextLayout::new(&font)
            .with_font_size(font_size)
            .with_writing_mode(WritingMode::VerticalRl)
            .run(text)?;
        let categories = CodePointMapData::<GeneralCategory>::new();
        let punctuation_offsets = text
            .char_indices()
            .filter_map(|(offset, character)| {
                GeneralCategoryGroup::Punctuation
                    .contains(categories.get(character))
                    .then_some(offset as u32)
            })
            .collect::<Vec<_>>();
        assert_eq!(punctuation_offsets.len(), 21);

        for cluster in punctuation_offsets {
            let punctuation = layout.lines[0]
                .glyphs
                .iter()
                .find(|glyph| glyph.cluster == cluster)
                .unwrap_or_else(|| panic!("missing punctuation glyph at byte offset {cluster}"));
            let font_ref = punctuation.font.skrifa_ref()?;
            let metrics = font_ref.glyph_metrics(Size::new(font_size), punctuation.font.location());
            let bounds = metrics
                .bounds(skrifa::GlyphId::new(punctuation.glyph_id))
                .expect("punctuation glyph has no bounds");

            let ink_center_x = punctuation.x_offset + (bounds.x_min + bounds.x_max) * 0.5;
            let ink_center_y = -punctuation.y_offset - (bounds.y_min + bounds.y_max) * 0.5;
            assert_approx_eq(ink_center_x, 0.0);
            assert_approx_eq(ink_center_y, -punctuation.y_advance * 0.5);
        }
        Ok(())
    }

    #[test]
    fn horizontal_center_alignment_with_overflow_is_aligned_relative_to_widest()
    -> anyhow::Result<()> {
        let font = any_system_font();
        // A very narrow container.
        let max_width = 20.0;
        // A very long word that is guaranteed to overflow 20px in any font.
        let text = "LONGWORDTHATWILLOVERFLOW,\nHI";
        let layout = TextLayout::new(&font)
            .with_font_size(20.0)
            .with_max_width(max_width)
            .with_alignment(TextAlign::Center)
            .run(text)?;

        let w0 = layout.lines[0].advance;
        let w1 = layout.lines[1].advance;

        // Ensure we are actually testing the overflow case.
        assert!(
            w0 > max_width,
            "Test error: widest line {w0} did not overflow max_width {max_width}"
        );

        let c0 = layout.lines[0].baseline.0 + w0 * 0.5;
        let c1 = layout.lines[1].baseline.0 + w1 * 0.5;

        // In a fixed system, the center of the short line should match the center
        // of the overflowing line, NOT the center of the original max_width constraint.
        assert!(
            (c0 - c1).abs() < 1.0,
            "expected line centres to match even with overflow, got c0={c0} c1={c1} (max_width={max_width})",
        );
        Ok(())
    }
}
