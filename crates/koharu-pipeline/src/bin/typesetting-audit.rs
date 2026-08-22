use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use koharu_config::Config;
use koharu_pipeline::{
    Committer, OcrModel, Operation, Pipeline, PipelineConfig, Progress, Request, Scope, Stage,
    StageOutput,
};
use koharu_rasterizer::{RasterOptions, Rasterizer};
use koharu_renderer::{
    Frame, LayerKind, RenderBounds, RenderDiagnostic, Renderer, TypesettingConfig,
};
use koharu_runtime::{Device, Feature, Runtime};
use koharu_scene::{
    AssetInput, AssetMetadata, AssetRole, At, Authored, EntityId, LanguageTag, PageDraft, Session,
    Snapshot, Translation,
};
use koharu_translator::ProvidersConfig;
use serde::{Deserialize, Serialize};

const FIXTURE_FORMAT: &str = "dev.koharu.typesetting-audit";
const FIXTURE_VERSION: u32 = 1;
const REPORT_FORMAT: &str = "dev.koharu.typesetting-audit-report";
const REPORT_VERSION: u32 = 1;
const RENDERER_READABILITY_FLOOR: f32 = 9.0;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Audit translated bubble sizing through Koharu's real detection, OCR, and renderer"
)]
struct Arguments {
    /// One image or a directory containing PNG, JPEG, and WebP pages.
    #[arg(short, long, value_name = "PATH")]
    input: PathBuf,

    /// Directory for the translation template, rendered pages, and JSON report.
    #[arg(short, long, value_name = "DIRECTORY")]
    output: PathBuf,

    /// Completed translation fixture produced by the first audit pass.
    #[arg(long, value_name = "FILE")]
    translations: Option<PathBuf>,

    /// Languages placed in a newly generated translation template.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "en-US,tr-TR",
        value_name = "LANGUAGES"
    )]
    template_languages: Vec<String>,

    #[arg(long, value_enum, default_value = "paddleocr-vl-1.6")]
    ocr: OcrChoice,

    /// Also run inpainting before rendering the translated pages.
    #[arg(long)]
    include_inpainting: bool,

    /// Require translated balloon text to render at or above this size.
    #[arg(long, default_value_t = 12.0)]
    minimum_font_size: f32,

    /// Require translated balloon text to retain this fraction of its detected source size.
    #[arg(long, default_value_t = 0.5)]
    minimum_source_ratio: f32,

    /// Maximum attempts to initialize native runtimes before failing the audit.
    #[arg(long, default_value_t = 3)]
    runtime_attempts: u32,

    #[arg(long)]
    cpu: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OcrChoice {
    #[value(name = "paddleocr-vl-1.6")]
    PaddleOcrVl1_6,
    #[value(name = "manga-ocr")]
    MangaOcr,
    #[value(name = "baberu-ocr")]
    BaberuOcr,
}

impl From<OcrChoice> for OcrModel {
    fn from(value: OcrChoice) -> Self {
        match value {
            OcrChoice::PaddleOcrVl1_6 => Self::PaddleOcrVl1_6,
            OcrChoice::MangaOcr => Self::MangaOcr,
            OcrChoice::BaberuOcr => Self::BaberuOcr,
        }
    }
}

struct SessionCommitter<'a>(&'a mut Session);

#[async_trait::async_trait]
impl Committer for SessionCommitter<'_> {
    async fn commit(&mut self, output: StageOutput) -> Result<Snapshot> {
        Ok(self.0.commit(output.patch).await?.snapshot)
    }
}

#[derive(Clone)]
struct InputPage {
    path: PathBuf,
    name: String,
    page: EntityId,
}

#[derive(Clone)]
struct TextSegment {
    layer: EntityId,
    content: EntityId,
    source: String,
    source_font_size: Option<f32>,
    balloon: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationFixture {
    format: String,
    format_version: u32,
    inputs: Vec<String>,
    translations: BTreeMap<String, Vec<Vec<String>>>,
}

#[derive(Debug, Serialize)]
struct AuditReport {
    format: String,
    format_version: u32,
    minimum_font_size: f32,
    minimum_source_ratio: f32,
    passed: bool,
    translated_balloon_count: usize,
    languages: Vec<LanguageAudit>,
}

#[derive(Debug, Serialize)]
struct LanguageAudit {
    language: String,
    passed: bool,
    pages: Vec<PageAudit>,
}

#[derive(Debug, Serialize)]
struct PageAudit {
    input: String,
    output: String,
    passed: bool,
    texts: Vec<TextAudit>,
}

#[derive(Debug, Serialize)]
struct TextAudit {
    order: usize,
    layer: String,
    balloon: bool,
    source: String,
    translation: String,
    source_font_size: Option<f32>,
    rendered_font_size: f32,
    source_ratio: Option<f32>,
    layout_bounds: BoundsAudit,
    rendered_bounds: BoundsAudit,
    overflow: bool,
    at_renderer_readability_floor: bool,
    renderer_readability_diagnostic: bool,
    below_audit_minimum: bool,
    below_source_ratio: bool,
    passed: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct BoundsAudit {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl From<RenderBounds> for BoundsAudit {
    fn from(value: RenderBounds) -> Self {
        Self {
            x: value.x,
            y: value.y,
            width: value.width,
            height: value.height,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    validate_arguments(&arguments)?;
    let input_paths = discover_images(&arguments.input)?;
    fs::create_dir_all(&arguments.output)
        .with_context(|| format!("failed to create {}", arguments.output.display()))?;

    let device = initialize_runtime(arguments.runtime_attempts).await?;
    let mut session = Session::memory().await?;
    let pages = add_pages(&mut session, &input_paths).await?;
    run_analysis_pipeline(&arguments, device, &mut session, &pages).await?;
    let segments = collect_segments(&session.snapshot(), &pages)?;
    ensure!(
        segments.iter().any(|page| !page.is_empty()),
        "detection and OCR produced no text segments"
    );

    let fixture = if let Some(path) = &arguments.translations {
        read_fixture(path)?
    } else {
        let fixture = translation_template(&arguments.template_languages, &pages, &segments)?;
        let path = arguments.output.join("typesetting-translations.json");
        write_json(&path, &fixture)?;
        eprintln!(
            "wrote {}; replace the ordered source strings for each language, then rerun with --translations {}",
            path.display(),
            path.display()
        );
        return Ok(());
    };
    validate_fixture(&fixture, &pages, &segments)?;

    let renderer = Renderer::from_config(Config::memory(TypesettingConfig::default()))?;
    let rasterizer = Rasterizer::new()?;
    let mut language_audits = Vec::new();
    let mut translated_balloon_count = 0;
    for (language, translations) in &fixture.translations {
        apply_translations(&mut session, &segments, language, translations).await?;
        let language_directory = arguments.output.join(safe_component(language));
        fs::create_dir_all(&language_directory)
            .with_context(|| format!("failed to create {}", language_directory.display()))?;
        let snapshot = session.snapshot();
        let mut page_audits = Vec::new();
        for (page_index, ((page, page_segments), page_translations)) in
            pages.iter().zip(&segments).zip(translations).enumerate()
        {
            let frame = renderer.render(&snapshot, page.page).await?;
            let output = language_directory.join(format!(
                "{:03}-{}.png",
                page_index + 1,
                safe_component(&page.name)
            ));
            save_frame(&rasterizer, &frame, &output)?;
            let audit = audit_page(
                &frame,
                page,
                page_segments,
                page_translations,
                &output,
                arguments.minimum_font_size,
                arguments.minimum_source_ratio,
            )?;
            translated_balloon_count += audit.texts.iter().filter(|text| text.balloon).count();
            page_audits.push(audit);
        }
        language_audits.push(LanguageAudit {
            language: language.clone(),
            passed: page_audits.iter().all(|page| page.passed),
            pages: page_audits,
        });
    }

    let passed =
        translated_balloon_count > 0 && language_audits.iter().all(|language| language.passed);
    let report = AuditReport {
        format: REPORT_FORMAT.to_owned(),
        format_version: REPORT_VERSION,
        minimum_font_size: arguments.minimum_font_size,
        minimum_source_ratio: arguments.minimum_source_ratio,
        passed,
        translated_balloon_count,
        languages: language_audits,
    };
    let report_path = arguments.output.join("typesetting-report.json");
    write_json(&report_path, &report)?;
    ensure!(
        report.passed,
        "typesetting audit failed; inspect {} and the rendered pages",
        report_path.display()
    );
    eprintln!(
        "typesetting audit passed; report: {}",
        report_path.display()
    );
    Ok(())
}

fn validate_arguments(arguments: &Arguments) -> Result<()> {
    ensure!(
        arguments.minimum_font_size.is_finite() && arguments.minimum_font_size > 0.0,
        "minimum font size must be finite and positive"
    );
    ensure!(
        arguments.minimum_source_ratio.is_finite()
            && (0.0..=1.0).contains(&arguments.minimum_source_ratio),
        "minimum source ratio must be between zero and one"
    );
    ensure!(
        arguments.runtime_attempts > 0,
        "runtime attempts must be greater than zero"
    );
    Ok(())
}

fn discover_images(input: &Path) -> Result<Vec<PathBuf>> {
    if input.is_file() {
        ensure!(
            is_supported_image(input),
            "unsupported input image {}",
            input.display()
        );
        return Ok(vec![input.to_owned()]);
    }
    ensure!(input.is_dir(), "input {} does not exist", input.display());
    let mut paths = fs::read_dir(input)
        .with_context(|| format!("failed to read {}", input.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|path| path.is_file() && is_supported_image(path));
    sort_input_paths(&mut paths);
    ensure!(
        !paths.is_empty(),
        "input directory {} contains no supported images",
        input.display()
    );
    let mut names = HashSet::new();
    for path in &paths {
        let name = input_name(path)?;
        ensure!(names.insert(name), "input image names must be unique");
    }
    Ok(paths)
}

fn sort_input_paths(paths: &mut [PathBuf]) {
    alphanumeric_sort::sort_slice_by_os_str_key(paths, |path| {
        path.file_name().unwrap_or_else(|| path.as_os_str())
    });
}

fn is_supported_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "webp")
    )
}

async fn initialize_runtime(attempts: u32) -> Result<Device> {
    let mut delay = Duration::from_secs(1);
    for attempt in 1..=attempts {
        let initialized = match Runtime::discover([Feature::Torch]) {
            Ok(runtime) => runtime.initialize().await,
            Err(error) => Err(error),
        };
        match initialized {
            Ok(device) => return Ok(device),
            Err(error) if attempt == attempts => {
                return Err(error).context(format!(
                    "failed to initialize native runtimes after {attempts} attempts"
                ));
            }
            Err(error) => {
                eprintln!(
                    "runtime initialization attempt {attempt} failed: {error}; retrying in {:.1}s",
                    delay.as_secs_f64()
                );
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(Duration::from_secs(30));
            }
        }
    }
    unreachable!("positive runtime attempts either return or fail")
}

async fn add_pages(session: &mut Session, paths: &[PathBuf]) -> Result<Vec<InputPage>> {
    let mut pages = Vec::with_capacity(paths.len());
    for path in paths {
        let source =
            fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let decoded = image::load_from_memory(&source)
            .with_context(|| format!("failed to decode {}", path.display()))?;
        let name = input_name(path)?;
        let mut page = None;
        let patch = session.snapshot().patch(|edit| {
            let id = edit.add_page(
                PageDraft::new(
                    name.clone(),
                    f64::from(decoded.width()),
                    f64::from(decoded.height()),
                ),
                At::End,
            )?;
            edit.set_asset(
                id,
                &AssetRole::new("source")?,
                AssetInput::new(
                    Arc::<[u8]>::from(source),
                    image_media_type(path),
                    AssetMetadata {
                        width: Some(decoded.width()),
                        height: Some(decoded.height()),
                        attributes: BTreeMap::new(),
                    },
                ),
            )?;
            page = Some(id);
            Ok(())
        })?;
        session.commit(patch).await?;
        pages.push(InputPage {
            path: path.clone(),
            name,
            page: page.expect("page ID is assigned by the edit"),
        });
    }
    Ok(pages)
}

async fn run_analysis_pipeline(
    arguments: &Arguments,
    runtime_device: Device,
    session: &mut Session,
    pages: &[InputPage],
) -> Result<()> {
    let mut config = PipelineConfig::default();
    config.ocr = arguments.ocr.into();
    let pipeline = Pipeline::from_config(
        Config::memory(config),
        Config::memory(ProvidersConfig::default()),
        if arguments.cpu {
            Device::cpu()
        } else {
            runtime_device
        },
    )?;
    let operation = if arguments.include_inpainting {
        Operation::Stages {
            stages: vec![Stage::Detection, Stage::Ocr, Stage::Inpainting],
        }
    } else {
        Operation::Through { stage: Stage::Ocr }
    };
    let mut committer = SessionCommitter(session);
    let snapshot = committer.0.snapshot();
    let report = pipeline
        .execute(
            snapshot,
            Request {
                operation,
                scope: Scope::Pages(pages.iter().map(|page| page.page).collect()),
                progress: Some(Arc::new(|event| {
                    if let Progress::Finished {
                        page,
                        stage,
                        elapsed,
                        ..
                    } = event
                    {
                        eprintln!("{page} {stage} finished in {:.2}s", elapsed.as_secs_f64());
                    }
                })),
                ..Request::default()
            },
            &mut committer,
        )
        .await?;
    eprintln!(
        "analysis pipeline finished in {:.2}s",
        report.elapsed.as_secs_f64()
    );
    Ok(())
}

fn collect_segments(snapshot: &Snapshot, pages: &[InputPage]) -> Result<Vec<Vec<TextSegment>>> {
    pages
        .iter()
        .map(|page| {
            let mut segments = Vec::new();
            if let Some(group) = snapshot.page(page.page)?.text_group()? {
                for layer in group.text_layers()? {
                    let content = layer.content()?;
                    let Some(source) = content.source()? else {
                        continue;
                    };
                    if source.text.value.trim().is_empty() {
                        continue;
                    }
                    segments.push(TextSegment {
                        layer: layer.id(),
                        content: content.id(),
                        source: source.text.value,
                        source_font_size: layer.typography()?.and_then(|value| value.size),
                        balloon: layer.balloon_target()?.is_some(),
                    });
                }
            }
            Ok(segments)
        })
        .collect()
}

fn translation_template(
    languages: &[String],
    pages: &[InputPage],
    segments: &[Vec<TextSegment>],
) -> Result<TranslationFixture> {
    let mut translations = BTreeMap::new();
    for language in languages {
        LanguageTag::new(language.clone())?;
        ensure!(
            translations
                .insert(
                    language.clone(),
                    segments
                        .iter()
                        .map(|page| page.iter().map(|segment| segment.source.clone()).collect())
                        .collect(),
                )
                .is_none(),
            "template languages must be unique"
        );
    }
    ensure!(
        !translations.is_empty(),
        "at least one template language is required"
    );
    Ok(TranslationFixture {
        format: FIXTURE_FORMAT.to_owned(),
        format_version: FIXTURE_VERSION,
        inputs: pages.iter().map(|page| page.name.clone()).collect(),
        translations,
    })
}

fn read_fixture(path: &Path) -> Result<TranslationFixture> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("{} is not a valid audit fixture", path.display()))
}

fn validate_fixture(
    fixture: &TranslationFixture,
    pages: &[InputPage],
    segments: &[Vec<TextSegment>],
) -> Result<()> {
    ensure!(
        fixture.format == FIXTURE_FORMAT && fixture.format_version == FIXTURE_VERSION,
        "unsupported typesetting audit fixture"
    );
    let expected_inputs = pages
        .iter()
        .map(|page| page.name.clone())
        .collect::<Vec<_>>();
    ensure!(
        fixture.inputs == expected_inputs,
        "audit fixture inputs do not match the sorted input pages"
    );
    ensure!(
        !fixture.translations.is_empty(),
        "audit fixture contains no translation languages"
    );
    for (language, translated_pages) in &fixture.translations {
        LanguageTag::new(language.clone())?;
        ensure!(
            translated_pages.len() == segments.len(),
            "{language} does not contain every input page"
        );
        for (page_index, (translations, expected)) in
            translated_pages.iter().zip(segments).enumerate()
        {
            ensure!(
                translations.len() == expected.len(),
                "{language} page {} has missing or extra translations",
                page_index + 1
            );
            ensure!(
                translations.iter().all(|text| !text.trim().is_empty()),
                "{language} page {} contains an empty translation",
                page_index + 1
            );
        }
    }
    Ok(())
}

async fn apply_translations(
    session: &mut Session,
    segments: &[Vec<TextSegment>],
    language: &str,
    translated_pages: &[Vec<String>],
) -> Result<()> {
    let language = LanguageTag::new(language.to_owned())?;
    let patch = session.snapshot().patch(|edit| {
        for (page, translations) in segments.iter().zip(translated_pages) {
            for (segment, translation) in page.iter().zip(translations) {
                edit.set(
                    segment.content,
                    &Translation {
                        text: Authored::user(translation.clone()),
                        language: Some(language.clone()),
                    },
                )?;
            }
        }
        Ok(())
    })?;
    session.commit(patch).await?;
    Ok(())
}

fn save_frame(rasterizer: &Rasterizer, frame: &Frame, output: &Path) -> Result<()> {
    let raster = rasterizer.rasterize(&frame.raster_frame()?, RasterOptions::default())?;
    raster
        .image
        .save(output)
        .with_context(|| format!("failed to write {}", output.display()))
}

fn audit_page(
    frame: &Frame,
    page: &InputPage,
    segments: &[TextSegment],
    translations: &[String],
    output: &Path,
    minimum_font_size: f32,
    minimum_source_ratio: f32,
) -> Result<PageAudit> {
    let mut texts = Vec::with_capacity(segments.len());
    for (order, (segment, translation)) in segments.iter().zip(translations).enumerate() {
        let layer = frame
            .layer(segment.layer)
            .with_context(|| format!("rendered frame is missing text layer {}", segment.layer))?;
        let LayerKind::Text(metadata) = layer.kind() else {
            bail!("layer {} did not render as text", segment.layer);
        };
        ensure!(
            metadata.text == *translation,
            "layer {} rendered a translation from the wrong order",
            segment.layer
        );
        let overflow = frame.diagnostics().iter().any(|diagnostic| {
            matches!(
                diagnostic,
                RenderDiagnostic::TextOverflow { entity, .. } if *entity == segment.layer
            )
        });
        let renderer_readability_diagnostic = frame.diagnostics().iter().any(|diagnostic| {
            matches!(
                diagnostic,
                RenderDiagnostic::TextBelowReadableSize { entity, .. } if *entity == segment.layer
            )
        });
        let at_renderer_readability_floor =
            segment.balloon && metadata.font_size <= RENDERER_READABILITY_FLOOR + f32::EPSILON;
        let source_ratio = segment
            .source_font_size
            .filter(|size| size.is_finite() && *size > 0.0)
            .map(|size| metadata.font_size / size);
        let below_audit_minimum =
            segment.balloon && metadata.font_size + f32::EPSILON < minimum_font_size;
        let below_source_ratio = segment.balloon
            && source_ratio.is_some_and(|ratio| ratio + f32::EPSILON < minimum_source_ratio);
        let passed = !overflow
            && !renderer_readability_diagnostic
            && !below_audit_minimum
            && !below_source_ratio;
        texts.push(TextAudit {
            order: order + 1,
            layer: segment.layer.to_string(),
            balloon: segment.balloon,
            source: segment.source.clone(),
            translation: translation.clone(),
            source_font_size: segment.source_font_size,
            rendered_font_size: metadata.font_size,
            source_ratio,
            layout_bounds: metadata.layout_bounds.into(),
            rendered_bounds: metadata.rendered_bounds.into(),
            overflow,
            at_renderer_readability_floor,
            renderer_readability_diagnostic,
            below_audit_minimum,
            below_source_ratio,
            passed,
        });
    }
    Ok(PageAudit {
        input: page.path.display().to_string(),
        output: output.display().to_string(),
        passed: texts.iter().all(|text| text.passed),
        texts,
    })
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut contents = serde_json::to_string_pretty(value)?;
    contents.push('\n');
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn input_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .with_context(|| format!("input path has no UTF-8 file name: {}", path.display()))
}

fn image_media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

fn safe_component(value: &str) -> String {
    let component = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if component.is_empty() || matches!(component.as_str(), "." | "..") {
        "output".to_owned()
    } else {
        component
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pages_and_segments() -> (Vec<InputPage>, Vec<Vec<TextSegment>>) {
        (
            vec![InputPage {
                path: PathBuf::from("16.webp"),
                name: "16.webp".to_owned(),
                page: EntityId::new(),
            }],
            vec![vec![TextSegment {
                layer: EntityId::new(),
                content: EntityId::new(),
                source: "待って".to_owned(),
                source_font_size: Some(24.0),
                balloon: true,
            }]],
        )
    }

    #[test]
    fn template_keeps_page_and_text_order_for_english_and_turkish() {
        let (pages, segments) = sample_pages_and_segments();
        let fixture =
            translation_template(&["en-US".to_owned(), "tr-TR".to_owned()], &pages, &segments)
                .unwrap();

        assert_eq!(fixture.inputs, vec!["16.webp".to_owned()]);
        assert_eq!(
            fixture.translations["en-US"],
            vec![vec!["待って".to_owned()]]
        );
        assert_eq!(
            fixture.translations["tr-TR"],
            vec![vec!["待って".to_owned()]]
        );
        validate_fixture(&fixture, &pages, &segments).unwrap();
    }

    #[test]
    fn fixture_rejects_reordered_or_incomplete_shapes() {
        let (pages, segments) = sample_pages_and_segments();
        let mut fixture = translation_template(&["en-US".to_owned()], &pages, &segments).unwrap();
        fixture.inputs[0] = "other.webp".to_owned();
        assert!(validate_fixture(&fixture, &pages, &segments).is_err());

        fixture.inputs[0] = "16.webp".to_owned();
        fixture.translations.get_mut("en-US").unwrap()[0].clear();
        assert!(validate_fixture(&fixture, &pages, &segments).is_err());
    }

    #[test]
    fn output_components_cannot_escape_the_audit_directory() {
        assert_eq!(safe_component("tr-TR"), "tr-TR");
        assert_eq!(safe_component("../bad name"), ".._bad_name");
        assert_eq!(safe_component(".."), "output");
    }

    #[test]
    fn input_pages_follow_the_same_natural_order_as_the_application() {
        let mut paths = vec![
            PathBuf::from("10.webp"),
            PathBuf::from("2.webp"),
            PathBuf::from("1.webp"),
        ];
        sort_input_paths(&mut paths);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("1.webp"),
                PathBuf::from("2.webp"),
                PathBuf::from("10.webp"),
            ]
        );
    }
}
