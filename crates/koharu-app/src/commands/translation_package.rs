use std::collections::HashMap;

use anyhow::{Context as _, Result, bail};
use koharu_desktop::Desktop;
use koharu_scene::{EntityId, LanguageTag, Revision, Snapshot};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{Cef, State, WebviewWindow};

use super::{
    ChannelExt as _, Error,
    canvas::CanvasChannel,
    project::{CurrentProject, TranslationUpdate},
};

const FORMAT: &str = "dev.koharu.context-translation";
const FORMAT_VERSION: u32 = 1;
const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;

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
struct TranslationPackage {
    format: String,
    format_version: u32,
    project: PackageProject,
    exported_revision: Revision,
    target_language: String,
    instructions: Vec<String>,
    context: TranslationContext,
    pages: Vec<PackagePage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PackageProject {
    id: String,
    name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct TranslationContext {
    chapter_summary: String,
    glossary: Vec<GlossaryEntry>,
    character_voices: Vec<CharacterVoice>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct GlossaryEntry {
    source: String,
    translation: String,
    note: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CharacterVoice {
    character: String,
    voice: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PackagePage {
    page_id: EntityId,
    page_number: u32,
    label: String,
    segments: Vec<PackageSegment>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PackageSegment {
    segment_id: EntityId,
    layer_id: EntityId,
    order: u32,
    role: Option<String>,
    source_language: Option<String>,
    source: String,
    translation: String,
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
    let package = build_package(&snapshot, &project_name, &target_language)?;
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
        let updates = validate_package(
            &project.snapshot(),
            &project.name,
            &target_language,
            &package,
        )?;
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

fn build_package(
    snapshot: &Snapshot,
    project_name: &str,
    target_language: &str,
) -> Result<TranslationPackage> {
    LanguageTag::new(target_language)?;
    let mut pages = Vec::new();
    for (page_index, page_ref) in snapshot.pages().enumerate() {
        let page = page_ref.page()?;
        let mut segments = Vec::new();
        if let Some(group) = page_ref.text_group()? {
            for layer in group.text_layers()? {
                let content = layer.content()?;
                let Some(source) = content.source()? else {
                    continue;
                };
                if source.text.value.trim().is_empty() {
                    continue;
                }
                segments.push(PackageSegment {
                    segment_id: content.id(),
                    layer_id: layer.id(),
                    order: next_number(segments.len(), "segments")?,
                    role: content.role()?.map(|role| role.role),
                    source_language: source.language.map(|language| language.to_string()),
                    source: source.text.value,
                    translation: String::new(),
                });
            }
        }
        pages.push(PackagePage {
            page_id: page_ref.id(),
            page_number: next_number(page_index, "pages")?,
            label: page.label,
            segments,
        });
    }
    if pages.iter().all(|page| page.segments.is_empty()) {
        bail!("the project has no OCR text; run detection and OCR before exporting");
    }
    Ok(TranslationPackage {
        format: FORMAT.to_owned(),
        format_version: FORMAT_VERSION,
        project: PackageProject {
            id: snapshot.project_id().to_string(),
            name: project_name.to_owned(),
        },
        exported_revision: snapshot.revision(),
        target_language: target_language.to_owned(),
        instructions: vec![
            "Read every page and every segment before translating anything.".to_owned(),
            "Use the complete chapter context to keep names, pronouns, terms, relationships, tone, and running jokes consistent across pages.".to_owned(),
            "Fill every translation field in the target language. Do not leave a translation empty.".to_owned(),
            "Edit only translation fields and the optional context notes. Do not change, add, remove, split, merge, or reorder pages or segments.".to_owned(),
            "Return the complete JSON document without Markdown fences or commentary.".to_owned(),
        ],
        context: TranslationContext::default(),
        pages,
    })
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
    project_name: &str,
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
    if package.project.id != snapshot.project_id().to_string()
        || package.project.name != project_name
    {
        bail!("translation package belongs to a different project");
    }
    let expected = build_package(snapshot, project_name, target_language)?;
    if package.pages.len() != expected.pages.len() {
        bail!("translation package does not contain every project page");
    }
    let mut package_pages = HashMap::with_capacity(package.pages.len());
    for page in &package.pages {
        if package_pages.insert(page.page_id, page).is_some() {
            bail!("translation package contains duplicate page IDs");
        }
    }
    let language = LanguageTag::new(target_language)?;
    let mut updates = Vec::new();
    for expected_page in &expected.pages {
        let page = package_pages
            .remove(&expected_page.page_id)
            .context("translation package is missing a project page")?;
        if page.page_number != expected_page.page_number || page.label != expected_page.label {
            bail!("project page order or labels changed after the package was exported");
        }
        if page.segments.len() != expected_page.segments.len() {
            bail!("translation package has missing or extra text segments");
        }
        let mut segments = HashMap::with_capacity(page.segments.len());
        for segment in &page.segments {
            if segments.insert(segment.segment_id, segment).is_some() {
                bail!("translation package contains duplicate segment IDs");
            }
        }
        for expected_segment in &expected_page.segments {
            let segment = segments
                .remove(&expected_segment.segment_id)
                .context("translation package is missing a text segment")?;
            if segment.layer_id != expected_segment.layer_id
                || segment.order != expected_segment.order
                || segment.role != expected_segment.role
                || segment.source_language != expected_segment.source_language
                || segment.source != expected_segment.source
            {
                bail!("source text or segment metadata changed after the package was exported");
            }
            if segment.translation.trim().is_empty() {
                bail!("every text segment must have a translation");
            }
            updates.push(TranslationUpdate {
                layer: segment.layer_id,
                text: segment.translation.clone(),
                language: language.clone(),
            });
        }
        if !segments.is_empty() {
            bail!("translation package contains unknown text segments");
        }
    }
    if !package_pages.is_empty() {
        bail!("translation package contains unknown project pages");
    }
    Ok(updates)
}

fn package_summary(package: &TranslationPackage) -> Result<TranslationPackageSummary> {
    Ok(TranslationPackageSummary {
        page_count: u32::try_from(package.pages.len()).context("too many package pages")?,
        segment_count: u32::try_from(
            package
                .pages
                .iter()
                .map(|page| page.segments.len())
                .sum::<usize>(),
        )
        .context("too many package segments")?,
    })
}

fn next_number(index: usize, what: &str) -> Result<u32> {
    u32::try_from(index)
        .ok()
        .and_then(|index| index.checked_add(1))
        .with_context(|| format!("too many {what}"))
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
                Ok(())
            })
            .unwrap();
        session.commit(patch).await.unwrap().snapshot
    }

    #[tokio::test]
    async fn package_carries_the_whole_chapter_and_accepts_reordered_output() {
        let snapshot = sample_snapshot().await;
        let mut package = build_package(&snapshot, "Chapter", "tr-TR").unwrap();
        assert_eq!(package.pages.len(), 2);
        assert_eq!(package.pages[0].segments.len(), 2);
        assert_eq!(package.pages[1].segments.len(), 2);
        assert!(
            package
                .pages
                .iter()
                .flat_map(|page| &page.segments)
                .all(|segment| segment.translation.is_empty())
        );
        for (index, segment) in package
            .pages
            .iter_mut()
            .flat_map(|page| &mut page.segments)
            .enumerate()
        {
            segment.translation = format!("Çeviri {}", index + 1);
        }
        package.pages.reverse();
        for page in &mut package.pages {
            page.segments.reverse();
        }

        let updates = validate_package(&snapshot, "Chapter", "tr-TR", &package).unwrap();
        assert_eq!(updates.len(), 4);
        assert!(
            updates
                .iter()
                .all(|update| update.language.as_str() == "tr-TR")
        );
    }

    #[tokio::test]
    async fn package_rejects_changed_source_and_incomplete_translations() {
        let snapshot = sample_snapshot().await;
        let mut package = build_package(&snapshot, "Chapter", "tr-TR").unwrap();
        for segment in package.pages.iter_mut().flat_map(|page| &mut page.segments) {
            segment.translation = "Çeviri".to_owned();
        }
        package.pages[0].segments[0].source = "Changed".to_owned();
        assert!(
            validate_package(&snapshot, "Chapter", "tr-TR", &package)
                .unwrap_err()
                .to_string()
                .contains("source text")
        );

        package = build_package(&snapshot, "Chapter", "tr-TR").unwrap();
        assert!(
            validate_package(&snapshot, "Chapter", "tr-TR", &package)
                .unwrap_err()
                .to_string()
                .contains("must have a translation")
        );
    }

    #[tokio::test]
    async fn parser_accepts_a_single_gemini_json_fence() {
        let snapshot = sample_snapshot().await;
        let package = build_package(&snapshot, "Chapter", "tr-TR").unwrap();
        let json = serde_json::to_string(&package).unwrap();
        let parsed = parse_package(&format!("```json\n{json}\n```")).unwrap();
        assert_eq!(parsed.project.id, package.project.id);
    }
}
