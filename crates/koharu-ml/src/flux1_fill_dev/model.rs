//! Native FLUX.1 Fill Dev model assembly.

use std::{path::PathBuf, sync::Mutex};

use anyhow::{Context as _, Result, anyhow, ensure};
use koharu_diffusion::{Context, ContextParams, ImageGenerationParams, RgbImage, VaeFormat};

use crate::Backend;

#[derive(Debug)]
pub(super) struct ModelPaths {
    pub transformer: PathBuf,
    pub text_encoder: PathBuf,
    pub vae: PathBuf,
}

#[derive(Debug)]
pub(super) struct Model {
    context: Mutex<Context>,
}

impl Model {
    pub fn new(device: &crate::Device, paths: ModelPaths) -> Result<Self> {
        let context = Context::new(&context_params(device, paths))
            .context("failed to load FLUX.1 Fill Dev components")?;
        ensure!(
            context.supports_image_generation(),
            "the loaded FLUX.1 Fill Dev context does not support image generation"
        );
        Ok(Self {
            context: Mutex::new(context),
        })
    }

    pub fn forward(&self, params: &ImageGenerationParams) -> Result<Vec<RgbImage>> {
        let mut context = self
            .context
            .lock()
            .map_err(|_| anyhow!("FLUX.1 Fill Dev context lock was poisoned"))?;
        context
            .generate_image(params)
            .context("FLUX.1 Fill Dev inference failed")
    }
}

fn context_params(device: &crate::Device, paths: ModelPaths) -> ContextParams {
    let use_accelerator = device.backend != Backend::Cpu;
    let keep_parameters_resident = use_accelerator && device.memory_free >= 20 * 1024 * 1024 * 1024;
    ContextParams {
        diffusion_model_path: Some(paths.transformer),
        llm_path: Some(paths.text_encoder),
        vae_path: Some(paths.vae),
        enable_mmap: true,
        flash_attention: use_accelerator,
        diffusion_flash_attention: use_accelerator,
        vae_format: VaeFormat::Flux,
        backend: Some(if use_accelerator {
            device.name.to_ascii_lowercase()
        } else {
            "cpu".to_owned()
        }),
        params_backend: (use_accelerator && !keep_parameters_resident).then(|| "*=cpu".to_owned()),
        ..ContextParams::default()
    }
}
