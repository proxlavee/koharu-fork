use serde::{Deserialize, Serialize};
use specta::Type;

/// Source-anchored sizing anchor (llm3.md §6).
///
/// `Source` makes auto-fit treat the detected source glyph scale as a target it
/// never exceeds — it only shrinks when balloon geometry forces it. `Off` keeps
/// the legacy "grow to fill the bubble" objective so the two can be compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, Type, Default)]
#[serde(rename_all = "lowercase")]
pub enum SizeAnchor {
    #[default]
    Source,
    Off,
}

/// How the auto-fit font-size search is run.
///
/// `SizeFirst` picks the largest integer size that fits and accepts whatever line
/// count the line-breaker naturally produces (llm3.md §1.4). `KStratified`
/// enumerates candidate line counts K and solves for the largest size per K, then
/// keeps whichever K produces the best outcome — it can find a deliberate 2-line
/// split at a larger size than the size-first walk ever probes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, Type, Default)]
#[serde(rename_all = "snake_case")]
pub enum FontSearchStrategy {
    #[default]
    SizeFirst,
    KStratified,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, Type)]
#[serde(default)]
pub struct TypesettingConfig {
    pub font_families: Vec<String>,
    /// Anchor auto-fit to the source lettering's visual size (llm3.md §6).
    pub size_anchor: SizeAnchor,
    /// Round axis-aligned text blocks to integer device pixels (llm3.md §5).
    pub pixel_snap: bool,
    /// Font-size search strategy for auto-fit (llm3.md §1.4 vs K-stratified).
    pub font_search_strategy: FontSearchStrategy,
}

impl Default for TypesettingConfig {
    fn default() -> Self {
        Self {
            font_families: vec!["CCWildWords".to_owned(), "Adobe 黑体 Std".to_owned()],
            size_anchor: SizeAnchor::Source,
            pixel_snap: true,
            font_search_strategy: FontSearchStrategy::SizeFirst,
        }
    }
}

impl TypesettingConfig {
    pub fn load() -> anyhow::Result<koharu_config::Config<Self>> {
        koharu_config::load("typesetting")
    }
}
