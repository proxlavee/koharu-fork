//! Reusable native WGPU readback and export supersampling.

use std::sync::mpsc;

use anyhow::{Context, Result as AnyResult, anyhow, bail};
use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::RgbaImage;
use parking_lot::Mutex;
use vello::{
    AaConfig, AaSupport, RenderParams, RendererOptions, Scene,
    kurbo::Affine,
    peniko::Color,
    util::RenderContext,
    wgpu::{
        self, Buffer, BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Extent3d,
        TexelCopyBufferInfo, Texture, TextureDescriptor, TextureFormat, TextureUsages, TextureView,
    },
};

use crate::{CompositionCommand, Error, Frame, GpuCompositor, RasterDraw, Result};

const MAX_SUPERSAMPLING_FACTOR: u32 = 4;
// Bound export working sets even when an adapter exposes very large textures.
// The final CPU image retains the authored dimensions; only GPU work is tiled.
const MAX_RASTER_TILE_DIMENSION: u32 = 4_096;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DownsampleFilter {
    Nearest,
    Triangle,
    CatmullRom,
    Gaussian,
    #[default]
    Lanczos3,
}

impl From<DownsampleFilter> for ResizeAlg {
    fn from(value: DownsampleFilter) -> Self {
        match value {
            DownsampleFilter::Nearest => ResizeAlg::Nearest,
            DownsampleFilter::Triangle => ResizeAlg::Convolution(FilterType::Bilinear),
            DownsampleFilter::CatmullRom => ResizeAlg::Convolution(FilterType::CatmullRom),
            DownsampleFilter::Gaussian => ResizeAlg::Convolution(FilterType::Gaussian),
            DownsampleFilter::Lanczos3 => ResizeAlg::Convolution(FilterType::Lanczos3),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterOptions {
    pub supersampling_factor: u32,
    pub downsample_filter: DownsampleFilter,
}

impl RasterOptions {
    #[must_use]
    pub fn supersampled(factor: u32) -> Self {
        Self {
            supersampling_factor: factor,
            ..Default::default()
        }
    }

    fn scale(self) -> u32 {
        self.supersampling_factor.clamp(1, MAX_SUPERSAMPLING_FACTOR)
    }
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self {
            supersampling_factor: 1,
            downsample_filter: DownsampleFilter::Lanczos3,
        }
    }
}

#[derive(Debug)]
pub struct Raster {
    pub image: RgbaImage,
    pub left: i32,
    pub top: i32,
}

struct GpuState {
    context: RenderContext,
    device_id: usize,
    renderer: vello::Renderer,
    compositor: GpuCompositor,
    targets: Vec<RenderTarget>,
}

struct RenderTarget {
    width: u32,
    height: u32,
    padded_width: u32,
    texture: Texture,
    view: TextureView,
    readback: Buffer,
}

/// Reusable headless Vello renderer with a bounded readback-target pool.
pub struct Rasterizer {
    gpu: Mutex<GpuState>,
}

impl Rasterizer {
    pub fn new() -> Result<Self> {
        Self::try_new().map_err(Error::backend)
    }

    fn try_new() -> AnyResult<Self> {
        let mut context = RenderContext::new();
        let device_id = pollster::block_on(context.device(None))
            .context("no WGPU adapter supports Vello's required features")?;
        let renderer = vello::Renderer::new(
            &context.devices[device_id].device,
            RendererOptions {
                antialiasing_support: AaSupport::area_only(),
                ..Default::default()
            },
        )
        .map_err(|error| anyhow!("failed to create Vello renderer: {error:?}"))?;
        let compositor = GpuCompositor::new(&context.devices[device_id].device);
        Ok(Self {
            gpu: Mutex::new(GpuState {
                context,
                device_id,
                renderer,
                compositor,
                targets: Vec::new(),
            }),
        })
    }

    pub fn rasterize(&self, frame: &Frame, options: RasterOptions) -> Result<Raster> {
        let (width, height) = frame.size();
        let (left, top) = frame.origin();
        let image = self
            .rasterize_frame_inner(frame, width, height, options)
            .map_err(Error::backend)?;
        Ok(Raster { image, left, top })
    }

    fn rasterize_frame_inner(
        &self,
        frame: &Frame,
        width: u32,
        height: u32,
        options: RasterOptions,
    ) -> AnyResult<RgbaImage> {
        checked_surface(width, height)?;
        let scale = options.scale();
        let raster_width = width
            .checked_mul(scale)
            .context("supersampled render surface width overflow")?;
        let raster_height = height
            .checked_mul(scale)
            .context("supersampled render surface height overflow")?;
        let commands = frame.composition_commands(scale);
        let pixels = self.readback_commands(&commands, raster_width, raster_height)?;
        finish_raster(pixels, raster_width, raster_height, width, height, options)
    }

    pub fn rasterize_commands(
        &self,
        commands: &[CompositionCommand],
        width: u32,
        height: u32,
        options: RasterOptions,
    ) -> Result<RgbaImage> {
        self.rasterize_commands_inner(commands, width, height, options)
            .map_err(Error::backend)
    }

    fn rasterize_commands_inner(
        &self,
        commands: &[CompositionCommand],
        width: u32,
        height: u32,
        options: RasterOptions,
    ) -> AnyResult<RgbaImage> {
        checked_surface(width, height)?;
        let scale = options.scale();
        let raster_width = width
            .checked_mul(scale)
            .context("supersampled render surface width overflow")?;
        let raster_height = height
            .checked_mul(scale)
            .context("supersampled render surface height overflow")?;
        let scaled;
        let commands = if scale == 1 {
            commands
        } else {
            let transform = Affine::scale(f64::from(scale));
            scaled = commands
                .iter()
                .map(|command| match command {
                    CompositionCommand::Raster(draw) => CompositionCommand::Raster(RasterDraw {
                        image: draw.image.clone(),
                        transform: transform * draw.transform,
                        opacity: draw.opacity,
                        erase: draw.erase,
                    }),
                    CompositionCommand::Vector(scene) => {
                        let mut scaled = Scene::new();
                        scaled.append(scene, Some(transform));
                        CompositionCommand::Vector(Box::new(scaled))
                    }
                })
                .collect::<Vec<_>>();
            &scaled
        };
        let pixels = self.readback_commands(commands, raster_width, raster_height)?;
        finish_raster(pixels, raster_width, raster_height, width, height, options)
    }

    pub fn rasterize_scene(
        &self,
        scene: &Scene,
        width: u32,
        height: u32,
        background: [u8; 4],
        options: RasterOptions,
    ) -> Result<RgbaImage> {
        self.rasterize_scene_inner(scene, width, height, background, options)
            .map_err(Error::backend)
    }

    fn rasterize_scene_inner(
        &self,
        scene: &Scene,
        width: u32,
        height: u32,
        background: [u8; 4],
        options: RasterOptions,
    ) -> AnyResult<RgbaImage> {
        checked_surface(width, height)?;
        let scale = options.scale();
        let raster_width = width
            .checked_mul(scale)
            .context("supersampled render surface width overflow")?;
        let raster_height = height
            .checked_mul(scale)
            .context("supersampled render surface height overflow")?;
        let scaled;
        let scene = if scale == 1 {
            scene
        } else {
            scaled = {
                let mut scaled = Scene::new();
                scaled.append(scene, Some(Affine::scale(f64::from(scale))));
                scaled
            };
            &scaled
        };
        let pixels = self.readback_scene(scene, raster_width, raster_height, background)?;
        finish_raster(pixels, raster_width, raster_height, width, height, options)
    }

    fn readback_commands(
        &self,
        commands: &[CompositionCommand],
        width: u32,
        height: u32,
    ) -> AnyResult<Vec<u8>> {
        let maximum_dimension = self.maximum_tile_dimension();
        let tiles = raster_tiles(width, height, maximum_dimension)?;
        if tiles.len() == 1 {
            return self.readback_commands_tile(commands, width, height);
        }

        let mut pixels = allocate_surface(width, height)?;
        for tile in tiles {
            let translation = Affine::translate((-f64::from(tile.left), -f64::from(tile.top)));
            let translated = commands
                .iter()
                .map(|command| match command {
                    CompositionCommand::Raster(draw) => CompositionCommand::Raster(RasterDraw {
                        image: draw.image.clone(),
                        transform: translation * draw.transform,
                        opacity: draw.opacity,
                        erase: draw.erase,
                    }),
                    CompositionCommand::Vector(scene) => {
                        let mut translated = Scene::new();
                        translated.append(scene, Some(translation));
                        CompositionCommand::Vector(Box::new(translated))
                    }
                })
                .collect::<Vec<_>>();
            let tile_pixels = self.readback_commands_tile(&translated, tile.width, tile.height)?;
            copy_raster_tile(&mut pixels, width, height, tile, &tile_pixels)?;
        }
        Ok(pixels)
    }

    fn readback_commands_tile(
        &self,
        commands: &[CompositionCommand],
        width: u32,
        height: u32,
    ) -> AnyResult<Vec<u8>> {
        let size = Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let (device, submission, target) = {
            let mut gpu = self.gpu.lock();
            let GpuState {
                context,
                device_id,
                renderer,
                compositor,
                targets,
            } = &mut *gpu;
            let device = context.devices[*device_id].device.clone();
            let queue = &context.devices[*device_id].queue;
            check_device_limit(&device, width, height)?;
            let target = take_target(targets, &device, width, height)?;
            compositor
                .render(
                    &device,
                    queue,
                    renderer,
                    &target.view,
                    (width, height),
                    commands,
                    None,
                    [0, 0, 0, 0],
                    [0, 0, width, height],
                )
                .map_err(|error| anyhow!(error.to_string()))?;
            let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
                label: Some("koharu frame readback encoder"),
            });
            encoder.copy_texture_to_buffer(
                target.texture.as_image_copy(),
                TexelCopyBufferInfo {
                    buffer: &target.readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(target.padded_width),
                        rows_per_image: None,
                    },
                },
                size,
            );
            let submission = queue.submit([encoder.finish()]);
            (device, submission, target)
        };
        self.finish_readback(device, submission, target)
    }

    fn readback_scene(
        &self,
        scene: &Scene,
        width: u32,
        height: u32,
        background: [u8; 4],
    ) -> AnyResult<Vec<u8>> {
        let maximum_dimension = self.maximum_tile_dimension();
        let tiles = raster_tiles(width, height, maximum_dimension)?;
        if tiles.len() == 1 {
            return self.readback_scene_tile(scene, width, height, background);
        }

        let mut pixels = allocate_surface(width, height)?;
        for tile in tiles {
            let mut translated = Scene::new();
            translated.append(
                scene,
                Some(Affine::translate((
                    -f64::from(tile.left),
                    -f64::from(tile.top),
                ))),
            );
            let tile_pixels =
                self.readback_scene_tile(&translated, tile.width, tile.height, background)?;
            copy_raster_tile(&mut pixels, width, height, tile, &tile_pixels)?;
        }
        Ok(pixels)
    }

    fn readback_scene_tile(
        &self,
        scene: &Scene,
        width: u32,
        height: u32,
        background: [u8; 4],
    ) -> AnyResult<Vec<u8>> {
        let size = Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let (device, submission, target) = {
            let mut gpu = self.gpu.lock();
            let GpuState {
                context,
                device_id,
                renderer,
                compositor: _,
                targets,
            } = &mut *gpu;
            let device = context.devices[*device_id].device.clone();
            let queue = &context.devices[*device_id].queue;
            check_device_limit(&device, width, height)?;
            let target = take_target(targets, &device, width, height)?;
            renderer
                .render_to_texture(
                    &device,
                    queue,
                    scene,
                    &target.view,
                    &RenderParams {
                        base_color: rgba(background),
                        width,
                        height,
                        antialiasing_method: AaConfig::Area,
                    },
                )
                .map_err(|error| anyhow!("Vello rendering failed: {error:?}"))?;
            let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
                label: Some("koharu scene readback encoder"),
            });
            encoder.copy_texture_to_buffer(
                target.texture.as_image_copy(),
                TexelCopyBufferInfo {
                    buffer: &target.readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(target.padded_width),
                        rows_per_image: None,
                    },
                },
                size,
            );
            let submission = queue.submit([encoder.finish()]);
            (device, submission, target)
        };
        self.finish_readback(device, submission, target)
    }

    fn maximum_tile_dimension(&self) -> u32 {
        let gpu = self.gpu.lock();
        gpu.context.devices[gpu.device_id]
            .device
            .limits()
            .max_texture_dimension_2d
            .min(MAX_RASTER_TILE_DIMENSION)
    }

    fn finish_readback(
        &self,
        device: wgpu::Device,
        submission: wgpu::SubmissionIndex,
        target: RenderTarget,
    ) -> AnyResult<Vec<u8>> {
        let width = target.width;
        let height = target.height;
        let slice = target.readback.slice(..);
        let (sender, receiver) = mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .map_err(|error| anyhow!("WGPU device polling failed: {error:?}"))?;
        receiver
            .recv()
            .context("WGPU closed the readback channel")?
            .context("failed to map WGPU readback buffer")?;
        let mapped = slice.get_mapped_range();
        let row_len = (width * 4) as usize;
        let mut pixels = Vec::with_capacity(row_len * height as usize);
        for row in mapped
            .chunks_exact(target.padded_width as usize)
            .take(height as usize)
        {
            pixels.extend_from_slice(&row[..row_len]);
        }
        drop(mapped);
        target.readback.unmap();
        let mut gpu = self.gpu.lock();
        if gpu.targets.len() < 4 {
            gpu.targets.push(target);
        }
        Ok(pixels)
    }
}

fn checked_surface(width: u32, height: u32) -> AnyResult<()> {
    if width == 0 || height == 0 {
        bail!("invalid render surface {width}x{height}");
    }
    Ok(())
}

fn check_device_limit(device: &wgpu::Device, width: u32, height: u32) -> AnyResult<()> {
    let limit = device.limits().max_texture_dimension_2d;
    if width > limit || height > limit {
        bail!("render surface {width}x{height} exceeds the device limit {limit}");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RasterTile {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
}

fn raster_tiles(width: u32, height: u32, maximum_dimension: u32) -> AnyResult<Vec<RasterTile>> {
    if maximum_dimension == 0 {
        bail!("WGPU reported a zero texture dimension limit");
    }
    let mut tiles = Vec::new();
    let mut top = 0;
    while top < height {
        let tile_height = height.saturating_sub(top).min(maximum_dimension);
        let mut left = 0;
        while left < width {
            let tile_width = width.saturating_sub(left).min(maximum_dimension);
            tiles.push(RasterTile {
                left,
                top,
                width: tile_width,
                height: tile_height,
            });
            left = left.saturating_add(tile_width);
        }
        top = top.saturating_add(tile_height);
    }
    Ok(tiles)
}

fn allocate_surface(width: u32, height: u32) -> AnyResult<Vec<u8>> {
    let byte_len = usize::try_from(u64::from(width) * u64::from(height) * 4)
        .context("render surface allocation size exceeds usize")?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(byte_len)
        .context("failed to allocate the render surface")?;
    pixels.resize(byte_len, 0);
    Ok(pixels)
}

fn copy_raster_tile(
    destination: &mut [u8],
    width: u32,
    height: u32,
    tile: RasterTile,
    source: &[u8],
) -> AnyResult<()> {
    if tile.left.saturating_add(tile.width) > width || tile.top.saturating_add(tile.height) > height
    {
        bail!("render tile exceeds the destination surface");
    }
    let destination_row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .context("render surface row size overflow")?;
    let source_row_bytes = usize::try_from(tile.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .context("render tile row size overflow")?;
    let expected_source_len = source_row_bytes
        .checked_mul(usize::try_from(tile.height).context("render tile height overflow")?)
        .context("render tile allocation size overflow")?;
    if source.len() != expected_source_len {
        bail!(
            "render tile returned {} bytes, expected {expected_source_len}",
            source.len()
        );
    }
    let left_bytes = usize::try_from(tile.left)
        .ok()
        .and_then(|left| left.checked_mul(4))
        .context("render tile horizontal offset overflow")?;
    for row in 0..usize::try_from(tile.height).context("render tile height overflow")? {
        let source_start = row
            .checked_mul(source_row_bytes)
            .context("render tile source offset overflow")?;
        let source_end = source_start
            .checked_add(source_row_bytes)
            .context("render tile source range overflow")?;
        let destination_start = usize::try_from(tile.top)
            .ok()
            .and_then(|top| top.checked_add(row))
            .and_then(|row| row.checked_mul(destination_row_bytes))
            .and_then(|offset| offset.checked_add(left_bytes))
            .context("render tile destination offset overflow")?;
        let destination_end = destination_start
            .checked_add(source_row_bytes)
            .context("render tile destination range overflow")?;
        let destination_row = destination
            .get_mut(destination_start..destination_end)
            .context("render tile destination range exceeds the surface")?;
        destination_row.copy_from_slice(&source[source_start..source_end]);
    }
    Ok(())
}

fn take_target(
    targets: &mut Vec<RenderTarget>,
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> AnyResult<RenderTarget> {
    targets
        .iter()
        .position(|target| target.width == width && target.height == height)
        .map(|position| targets.swap_remove(position))
        .map_or_else(|| RenderTarget::new(device, width, height), Ok)
}

impl RenderTarget {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> AnyResult<Self> {
        let size = Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("koharu rasterizer target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::STORAGE_BINDING
                | TextureUsages::TEXTURE_BINDING
                | TextureUsages::RENDER_ATTACHMENT
                | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let row_bytes = width
            .checked_mul(4)
            .context("render target row size overflow")?;
        let padded_width = row_bytes.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer_size = u64::from(padded_width)
            .checked_mul(u64::from(height))
            .context("render target buffer size overflow")?;
        let readback = device.create_buffer(&BufferDescriptor {
            label: Some("koharu rasterizer readback"),
            size: buffer_size,
            usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            width,
            height,
            padded_width,
            texture,
            view,
            readback,
        })
    }
}

fn finish_raster(
    pixels: Vec<u8>,
    raster_width: u32,
    raster_height: u32,
    width: u32,
    height: u32,
    options: RasterOptions,
) -> AnyResult<RgbaImage> {
    let image = RgbaImage::from_raw(raster_width, raster_height, pixels)
        .context("WGPU returned an invalid RGBA buffer")?;
    if options.scale() == 1 {
        return Ok(image);
    }
    let mut downsampled = RgbaImage::new(width, height);
    let resize_options = ResizeOptions::new()
        .resize_alg(options.downsample_filter.into())
        .use_alpha(true);
    Resizer::new()
        .resize(&image, &mut downsampled, &resize_options)
        .context("failed to downsample WGPU render")?;
    Ok(downsampled)
}

fn rgba([r, g, b, a]: [u8; 4]) -> Color {
    Color::from_rgba8(r, g, b, a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tall_render_surfaces_are_covered_by_bounded_tiles() {
        let tiles = raster_tiles(720, 10_259, 4_096).unwrap();

        assert_eq!(
            tiles,
            vec![
                RasterTile {
                    left: 0,
                    top: 0,
                    width: 720,
                    height: 4_096,
                },
                RasterTile {
                    left: 0,
                    top: 4_096,
                    width: 720,
                    height: 4_096,
                },
                RasterTile {
                    left: 0,
                    top: 8_192,
                    width: 720,
                    height: 2_067,
                },
            ]
        );
    }

    #[test]
    fn two_dimensional_tiles_cover_each_pixel_once() {
        let tiles = raster_tiles(9, 7, 4).unwrap();
        let mut coverage = vec![0_u8; 9 * 7];
        for tile in tiles {
            assert!(tile.width <= 4 && tile.height <= 4);
            for y in tile.top..tile.top + tile.height {
                for x in tile.left..tile.left + tile.width {
                    coverage[(y * 9 + x) as usize] += 1;
                }
            }
        }
        assert!(coverage.into_iter().all(|count| count == 1));
    }

    #[test]
    fn copying_tiles_reassembles_rows_at_two_dimensional_boundaries() {
        let mut surface = allocate_surface(3, 3).unwrap();
        let tile = RasterTile {
            left: 1,
            top: 1,
            width: 2,
            height: 2,
        };
        let source = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

        copy_raster_tile(&mut surface, 3, 3, tile, &source).unwrap();

        assert_eq!(&surface[16..24], &source[..8]);
        assert_eq!(&surface[28..36], &source[8..]);
        assert!(surface[..16].iter().all(|byte| *byte == 0));
    }
}
