use anyhow::{Context as _, Result, bail};
use koharu_desktop::Desktop;
use koharu_scene::{EntityId, LanguageTag, Snapshot};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{Cef, State, WebviewWindow};

use super::{
    ChannelExt as _, Error,
    canvas::CanvasChannel,
    project::{CurrentProject, TranslationUpdate},
};

const FORMAT: &str = "dev.koharu.context-translation";
const FORMAT_VERSION: u32 = 2;
const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
const INSTRUCTION: &str = "Read every page first. Replace only the strings in pages with tr-TR translations, preserve the exact nested array shape and order, and return the complete JSON only.";

#[derive(Clone, Debug, Serialize, Type)]
pub struct TranslationPackageSummary {
    pub page_count: u32,
    pub segment_count: u32,
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct TranslationImportResult {
    pub page_count: u32,
    pub translation_count: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranslationPackage {
    format: String,
    format_version: u32,
    target_language: String,
    instruction: String,
    pages: Vec<Vec<String>>,
}

#[derive(Clone, Debug)]
struct OrderedTextSegment {
    layer: EntityId,
    source: String,
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn export_translation_package(
    window: WebviewWindow<Cef>,
    target_language: String,
    project: State<'_, CurrentProject>,
) -> std::result::Result<Option<TranslationPackageSummary>, Error> {
    let (snapshot, project_name) = {
        let project = project.project.lock().await;
        let project = project.as_ref().context("no project is open")?;
        (project.snapshot(), project.name.clone())
    };
    let package = build_package(&snapshot, &target_language)?;
    let summary = package_summary(&package)?;
    let file_name = format!("{}-context-translation.txt", safe_file_stem(&project_name));
    let Some(file) = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .add_filter("Koharu translation text", &["txt"])
        .set_file_name(file_name)
        .save_file()
        .await
    else {
        return Ok(None);
    };
    let mut contents = serde_json::to_string_pretty(&package)?;
    contents.push('\n');
    tokio::fs::write(file.path(), contents)
        .await
        .with_context(|| format!("failed to write {}", file.path().display()))?;
    Ok(Some(summary))
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn import_translation_package(
    window: WebviewWindow<Cef>,
    target_language: String,
    desktop: State<'_, Desktop>,
    project: State<'_, CurrentProject>,
    canvas_channel: State<'_, CanvasChannel>,
) -> std::result::Result<Option<TranslationImportResult>, Error> {
    let Some(file) = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .add_filter("Koharu translation text", &["txt"])
        .pick_file()
        .await
    else {
        return Ok(None);
    };
    let metadata = tokio::fs::metadata(file.path())
        .await
        .with_context(|| format!("failed to inspect {}", file.path().display()))?;
    if metadata.len() > MAX_PACKAGE_BYTES {
        return Err(anyhow::anyhow!("translation package is larger than 64 MiB").into());
    }
    let contents = tokio::fs::read_to_string(file.path())
        .await
        .with_context(|| format!("failed to read {}", file.path().display()))?;
    let package = parse_package(&contents)?;
    let (commit, active_page, result) = {
        let mut project = project.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let updates = validate_package(&project.snapshot(), &target_language, &package)?;
        let result = TranslationImportResult {
            page_count: u32::try_from(package.pages.len()).context("too many package pages")?,
            translation_count: u32::try_from(updates.len())
                .context("too many package translations")?,
        };
        let commit = project.set_translations(updates).await?;
        project.record_commit(&commit);
        (commit, project.active_page(), result)
    };
    desktop
        .synchronize(&commit.snapshot, active_page, &commit)
        .await?;
    canvas_channel.channel.publish(desktop.canvas_state());
    Ok(Some(result))
}

fn build_package(snapshot: &Snapshot, target_language: &str) -> Result<TranslationPackage> {
    LanguageTag::new(target_language)?;
    let pages = ordered_text_segments(snapshot)?;
    if pages.iter().all(Vec::is_empty) {
        bail!("the project has no OCR text; run detection and OCR before exporting");
    }
    Ok(TranslationPackage {
        format: FORMAT.to_owned(),
        format_version: FORMAT_VERSION,
        target_language: target_language.to_owned(),
        instruction: INSTRUCTION.replacen("tr-TR", target_language, 1),
        pages: pages
            .into_iter()
            .map(|page| page.into_iter().map(|segment| segment.source).collect())
            .collect(),
    })
}

fn ordered_text_segments(snapshot: &Snapshot) -> Result<Vec<Vec<OrderedTextSegment>>> {
    snapshot
        .pages()
        .map(|page| {
            let mut segments = Vec::new();
            if let Some(group) = page.text_group()? {
                for layer in group.text_layers()? {
                    let content = layer.content()?;
                    let Some(source) = content.source()? else {
                        continue;
                    };
                    if source.text.value.trim().is_empty() {
                        continue;
                    }
                    segments.push(OrderedTextSegment {
                        layer: layer.id(),
                        source: source.text.value,
                    });
                }
            }
            Ok(segments)
        })
        .collect()
}

fn parse_package(contents: &str) -> Result<TranslationPackage> {
    let contents = contents.trim();
    let json = if let Some(fenced) = contents.strip_prefix("```") {
        let (_, body) = fenced
            .split_once('\n')
            .context("the translation package Markdown fence has no body")?;
        body.strip_suffix("```")
            .context("the translation package Markdown fence is not closed")?
            .trim()
    } else {
        contents
    };
    serde_json::from_str(json).context("translation package is not valid JSON")
}

fn validate_package(
    snapshot: &Snapshot,
    target_language: &str,
    package: &TranslationPackage,
) -> Result<Vec<TranslationUpdate>> {
    if package.format != FORMAT || package.format_version != FORMAT_VERSION {
        bail!("unsupported Koharu translation package format or version");
    }
    LanguageTag::new(target_language)?;
    if package.target_language != target_language {
        bail!(
            "translation package targets {}, but the current translation target is {}",
            package.target_language,
            target_language
        );
    }
    let expected = ordered_text_segments(snapshot)?;
    if package.pages.len() != expected.len() {
        bail!("translation package does not contain every project page");
    }
    let language = LanguageTag::new(target_language)?;
    let mut updates = Vec::new();
    for (expected_page, translated_page) in expected.iter().zip(&package.pages) {
        if translated_page.len() != expected_page.len() {
            bail!("translation package has missing or extra text segments");
        }
        for (segment, translation) in expected_page.iter().zip(translated_page) {
            if translation.trim().is_empty() {
                bail!("every text segment must have a translation");
            }
            updates.push(TranslationUpdate {
                layer: segment.layer,
                text: translation.clone(),
                language: language.clone(),
            });
        }
    }
    Ok(updates)
}

fn package_summary(package: &TranslationPackage) -> Result<TranslationPackageSummary> {
    Ok(TranslationPackageSummary {
        page_count: u32::try_from(package.pages.len()).context("too many package pages")?,
        segment_count: u32::try_from(package.pages.iter().map(Vec::len).sum::<usize>())
            .context("too many package segments")?,
    })
}

fn safe_file_stem(value: &str) -> String {
    let stem = value
        .trim()
        .chars()
        .map(|character| {
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    if stem.is_empty() {
        "koharu".to_owned()
    } else {
        stem
    }
}

#[cfg(test)]
mod tests {
    use koharu_scene::{
        At, Authored, Origin, PageDraft, Session, SourceText, TextLayout, TextLayoutKind,
    };

    use super::*;

    async fn sample_snapshot() -> Snapshot {
        let mut session = Session::memory().await.unwrap();
        let snapshot = session.snapshot();
        let patch = snapshot
            .patch(|edit| {
                for (label, lines) in [
                    ("Page 1", ["Who are you?", "I'm Koharu."]),
                    ("Page 2", ["Koharu, wait!", "I can't."]),
                ] {
                    let page = edit.add_page(PageDraft::new(label, 800.0, 1200.0), At::End)?;
                    for line in lines {
                        let content = edit.add_text_content(page, At::End)?;
                        edit.set(
                            content,
                            &SourceText {
                                text: Authored::user(line.to_owned()),
                                language: Some(LanguageTag::new("en-US")?),
                            },
                        )?;
                        edit.add_text_layer(
                            page,
                            At::End,
                            content,
                            &TextLayout {
                                origin: Origin::User,
                                kind: TextLayoutKind::Paragraph,
                            },
                        )?;
                    }
                }
                edit.add_page(PageDraft::new("Page 3", 800.0, 1200.0), At::End)?;
                Ok(())
            })
            .unwrap();
        session.commit(patch).await.unwrap().snapshot
    }

    #[tokio::test]
    async fn package_exports_only_ordered_source_strings() {
        let snapshot = sample_snapshot().await;
        let package = build_package(&snapshot, "tr-TR").unwrap();
        assert_eq!(package.pages.len(), 3);
        assert_eq!(
            package.pages,
            vec![
                ["Who are you?", "I'm Koharu."].map(str::to_owned).to_vec(),
                ["Koharu, wait!", "I can't."].map(str::to_owned).to_vec(),
                Vec::new(),
            ]
        );
        assert_eq!(package.instruction, INSTRUCTION);

        let serialized = serde_json::to_value(&package).unwrap();
        let object = serialized.as_object().unwrap();
        assert_eq!(object.len(), 5);
        for key in [
            "format",
            "format_version",
            "target_language",
            "instruction",
            "pages",
        ] {
            assert!(object.contains_key(key));
        }
        for removed_key in ["project", "exported_revision", "context", "segments"] {
            assert!(!object.contains_key(removed_key));
        }
        assert_eq!(package_summary(&package).unwrap().segment_count, 4);
        assert_eq!(package_summary(&package).unwrap().page_count, 3);
    }

    #[tokio::test]
    async fn package_applies_translations_by_page_and_text_order() {
        let snapshot = sample_snapshot().await;
        let mut package = build_package(&snapshot, "tr-TR").unwrap();
        package.pages = vec![
            vec!["Sen kimsin?".to_owned(), "Ben Koharu.".to_owned()],
            vec!["Koharu, bekle!".to_owned(), "Yapamam.".to_owned()],
            Vec::new(),
        ];

        let updates = validate_package(&snapshot, "tr-TR", &package).unwrap();
        assert_eq!(updates.len(), 4);
        assert_eq!(
            updates
                .iter()
                .map(|update| update.text.as_str())
                .collect::<Vec<_>>(),
            ["Sen kimsin?", "Ben Koharu.", "Koharu, bekle!", "Yapamam."]
        );
        assert!(
            updates
                .iter()
                .all(|update| update.language.as_str() == "tr-TR")
        );

        let expected_layers = ordered_text_segments(&snapshot)
            .unwrap()
            .into_iter()
            .flatten()
            .map(|segment| segment.layer)
            .collect::<Vec<_>>();
        assert_eq!(
            updates
                .iter()
                .map(|update| update.layer)
                .collect::<Vec<_>>(),
            expected_layers
        );
    }

    #[tokio::test]
    async fn package_rejects_changed_shape_incomplete_text_and_old_version() {
        let snapshot = sample_snapshot().await;
        let mut package = build_package(&snapshot, "tr-TR").unwrap();
        for text in package.pages.iter_mut().flatten() {
            *text = "Çeviri".to_owned();
        }

        let mut missing_page = package.clone();
        missing_page.pages.pop();
        assert!(
            validate_package(&snapshot, "tr-TR", &missing_page)
                .unwrap_err()
                .to_string()
                .contains("every project page")
        );

        let mut missing_segment = package.clone();
        missing_segment.pages[0].pop();
        assert!(
            validate_package(&snapshot, "tr-TR", &missing_segment)
                .unwrap_err()
                .to_string()
                .contains("missing or extra text segments")
        );

        let mut incomplete = package.clone();
        incomplete.pages[0][0] = "  ".to_owned();
        assert!(
            validate_package(&snapshot, "tr-TR", &incomplete)
                .unwrap_err()
                .to_string()
                .contains("must have a translation")
        );

        let mut wrong_target = package.clone();
        wrong_target.target_language = "de-DE".to_owned();
        assert!(
            validate_package(&snapshot, "tr-TR", &wrong_target)
                .unwrap_err()
                .to_string()
                .contains("current translation target")
        );

        package.format_version = 1;
        assert!(
            validate_package(&snapshot, "tr-TR", &package)
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
    }

    #[tokio::test]
    async fn parser_accepts_a_single_gemini_json_fence() {
        let snapshot = sample_snapshot().await;
        let package = build_package(&snapshot, "tr-TR").unwrap();
        let json = serde_json::to_string(&package).unwrap();
        let parsed = parse_package(&format!("```json\n{json}\n```")).unwrap();
        assert_eq!(parsed.pages, package.pages);
    }
}
