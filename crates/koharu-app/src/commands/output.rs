use anyhow::{Context as _, Result};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use image::{
    ExtendedColorType, ImageEncoder as _, RgbaImage,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use koharu_psd::{PsdExportOptions, export_page};
use koharu_rasterizer::{Raster, RasterOptions, Rasterizer};
use koharu_renderer::{Frame, Renderer};
use koharu_scene::{AssetRole, EntityId, Snapshot};
use serde::Deserialize;
use specta::Type;
use std::sync::Arc;
use tauri::{Cef, State, WebviewWindow, ipc::IpcResponse};

use super::{Error, project::CurrentProject};
use koharu_desktop::Desktop;

const THUMBNAIL_EDGE: u32 = 128;

#[derive(Type)]
#[specta(transparent)]
pub(crate) struct ThumbnailBytes(#[specta(type = Vec<u8>)] Vec<u8>);

impl IpcResponse for ThumbnailBytes {
    fn body(self) -> tauri::Result<tauri::ipc::InvokeResponseBody> {
        Ok(self.0.into())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Png,
    Psd,
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "export",
    skip_all,
    fields(origin = "user", format = ?format),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn export_pages(
    window: WebviewWindow<Cef>,
    pages: Vec<EntityId>,
    format: ExportFormat,
    project: State<'_, CurrentProject>,
    desktop: State<'_, Desktop>,
) -> std::result::Result<(), Error> {
    let snapshot = {
        let project = project.project.lock().await;
        let project = project.as_ref().context("no project is open")?;
        project.snapshot()
    };
    let Some(directory) = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .pick_folder()
        .await
        .map(|directory| directory.path().to_owned())
    else {
        return Ok(());
    };
    let pages = if pages.is_empty() {
        snapshot.pages().map(|page| page.id()).collect()
    } else {
        pages
    };
    if pages.is_empty() {
        return Err(anyhow::anyhow!("there are no pages to export").into());
    }
    let renderer = desktop.renderer();
    let rasterizer = desktop.rasterizer().await?;
    let jobs = pages
        .into_iter()
        .enumerate()
        .map(|(index, page_id)| {
            let page = snapshot.page(page_id)?.page()?;
            let name = page
                .label
                .trim()
                .trim_end_matches(|character: char| character == '.' || character.is_whitespace());
            let name = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
            let name = name
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
            let stem = format!(
                "{:04}_{}",
                index + 1,
                if name.is_empty() { "page" } else { &name }
            );
            Ok::<_, anyhow::Error>((page_id, stem))
        })
        .collect::<Result<Vec<_>>>()?;
    stream::iter(jobs)
        .map(|(page_id, stem)| {
            let renderer = renderer.clone();
            let rasterizer = Arc::clone(&rasterizer);
            let snapshot = snapshot.clone();
            let directory = directory.clone();
            async move {
                let frame = renderer.render(&snapshot, page_id).await?;
                match format {
                    ExportFormat::Png => {
                        let image =
                            rasterize(Arc::clone(&rasterizer), &frame, RasterOptions::default())
                                .await?
                                .image;
                        tokio::task::spawn_blocking(move || -> Result<()> {
                            let file =
                                std::fs::File::create(directory.join(format!("{stem}.png")))?;
                            encode_png(file, image)?;
                            Ok(())
                        })
                        .await
                        .context("PNG export worker stopped unexpectedly")??;
                    }
                    ExportFormat::Psd => {
                        let bytes = export_page(
                            Arc::clone(&rasterizer),
                            &snapshot,
                            &frame,
                            &PsdExportOptions::default(),
                        )
                        .await?;
                        tokio::fs::write(directory.join(format!("{stem}.psd")), bytes).await?;
                    }
                }
                tracing::info!(
                    target: "koharu_metrics",
                    metric = "page_exported",
                    format = ?format,
                );
                Ok::<_, anyhow::Error>(())
            }
        })
        .buffer_unordered(4)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(())
}

fn encode_png(writer: impl std::io::Write, image: RgbaImage) -> image::ImageResult<()> {
    let (width, height) = image.dimensions();
    let mut pixels = image.into_raw();
    let (mut opaque, mut grayscale) = (true, true);
    for pixel in pixels.chunks_exact(4) {
        opaque &= pixel[3] == u8::MAX;
        grayscale &= pixel[0] == pixel[1] && pixel[1] == pixel[2];
        if !opaque && !grayscale {
            break;
        }
    }
    let color_type = match (grayscale, opaque) {
        (true, true) => ExtendedColorType::L8,
        (true, false) => ExtendedColorType::La8,
        (false, true) => ExtendedColorType::Rgb8,
        (false, false) => ExtendedColorType::Rgba8,
    };
    let channels = usize::from(color_type.channel_count());
    if channels < 4 {
        let pixel_count = pixels.len() / 4;
        for index in 0..pixel_count {
            let source = index * 4;
            let target = index * channels;
            match color_type {
                ExtendedColorType::L8 => pixels[target] = pixels[source],
                ExtendedColorType::La8 => {
                    pixels[target] = pixels[source];
                    pixels[target + 1] = pixels[source + 3];
                }
                ExtendedColorType::Rgb8 => {
                    pixels.copy_within(source..source + 3, target);
                }
                ExtendedColorType::Rgba8 => unreachable!(),
                _ => unreachable!("export only selects 8-bit PNG color types"),
            }
        }
        pixels.truncate(pixel_count * channels);
    }
    PngEncoder::new_with_quality(writer, CompressionType::Best, FilterType::Adaptive)
        .write_image(&pixels, width, height, color_type)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_thumbnail(
    page: EntityId,
    project: State<'_, CurrentProject>,
) -> std::result::Result<ThumbnailBytes, Error> {
    let snapshot = project
        .project
        .lock()
        .await
        .as_ref()
        .context("no project is open")?
        .snapshot();
    snapshot.page(page)?;
    let blob = snapshot
        .asset(page, &AssetRole::new("source")?)?
        .with_context(|| format!("page {page} has no source image"))?
        .blob;
    let bytes = snapshot.read_blob(blob).await?;
    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let image = image::load_from_memory(&bytes).context("failed to decode source image")?;
        if image.width() == 0 || image.height() == 0 {
            return Err(anyhow::anyhow!("source image is empty"));
        }
        let image = image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE).to_rgba8();
        let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
        Ok(encoder.encode(80.0).to_vec())
    })
    .await
    .context("thumbnail worker stopped unexpectedly")??;
    Ok(ThumbnailBytes(bytes))
}

pub(crate) async fn rendered_preview(
    renderer: &Renderer,
    rasterizer: Arc<Rasterizer>,
    snapshot: &Snapshot,
    page: EntityId,
) -> Result<Vec<u8>> {
    snapshot.page(page)?;
    let frame = renderer.render(snapshot, page).await?;
    let image = rasterize(rasterizer, &frame, RasterOptions::default())
        .await?
        .image;
    tokio::task::spawn_blocking(move || {
        let image = image::DynamicImage::ImageRgba8(image)
            .resize(1024, 1024, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
        Ok::<_, anyhow::Error>(encoder.encode(85.0).to_vec())
    })
    .await
    .context("preview encode worker stopped unexpectedly")?
}

async fn rasterize(
    rasterizer: Arc<Rasterizer>,
    frame: &Frame,
    options: RasterOptions,
) -> Result<Raster> {
    let frame = frame.raster_frame()?;
    tokio::task::spawn_blocking(move || rasterizer.rasterize(&frame, options))
        .await
        .context("rasterizer worker stopped unexpectedly")?
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{ColorType, ImageDecoder as _, Rgba, codecs::png::PngDecoder};

    use super::*;

    #[test]
    fn png_export_uses_the_smallest_lossless_color_type() {
        let fixtures = [
            (
                RgbaImage::from_fn(3, 2, |x, y| {
                    let value = 10 + (y * 3 + x) as u8;
                    Rgba([value, value, value, 255])
                }),
                ColorType::L8,
            ),
            (
                RgbaImage::from_fn(3, 2, |x, y| {
                    let value = 10 + (y * 3 + x) as u8;
                    Rgba([value, value, value, 120 + value])
                }),
                ColorType::La8,
            ),
            (
                RgbaImage::from_fn(3, 2, |x, y| {
                    let value = 10 + (y * 3 + x) as u8;
                    Rgba([value, value + 10, value + 20, 255])
                }),
                ColorType::Rgb8,
            ),
            (
                RgbaImage::from_fn(3, 2, |x, y| {
                    let value = 10 + (y * 3 + x) as u8;
                    Rgba([value, value + 10, value + 20, 120 + value])
                }),
                ColorType::Rgba8,
            ),
        ];
        for (original, expected) in fixtures {
            let mut encoded = Vec::new();
            encode_png(&mut encoded, original.clone()).unwrap();

            let decoder = PngDecoder::new(Cursor::new(&encoded)).unwrap();
            assert_eq!(decoder.color_type(), expected);
            let decoded = image::load_from_memory_with_format(&encoded, image::ImageFormat::Png)
                .unwrap()
                .to_rgba8();
            assert_eq!(decoded, original);
        }
    }
}
