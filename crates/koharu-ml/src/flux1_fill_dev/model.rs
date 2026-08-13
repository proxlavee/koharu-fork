//! Native FLUX.1 Fill Dev model assembly.
//!
//! The component mapping and module backend names follow stable-diffusion.cpp
//! at commit cc734292286f85f9c48305d94d7fd22f42838522:
//! https://github.com/leejet/stable-diffusion.cpp/blob/cc734292286f85f9c48305d94d7fd22f42838522/docs/backend.md

use std::{path::PathBuf, sync::Mutex};

use anyhow::{Context as _, Result, anyhow, ensure};
use koharu_diffusion::{Context, ContextParams, ImageGenerationParams, RgbImage, VaeFormat};

use crate::Backend;

const VRAM_BUDGET_PERCENT: usize = 75;
const MIB: usize = 1024 * 1024;

#[derive(Debug)]
pub(super) struct ModelPaths {
    pub transformer: PathBuf,
    pub clip_l: PathBuf,
    pub t5xxl: PathBuf,
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
    let backend = device.name.to_ascii_lowercase();
    let module_backends =
        use_accelerator.then(|| format!("te=cpu,diffusion={backend},vae={backend}"));
    let max_vram = vram_budget(device).map(|budget| format!("{backend}={budget}"));
    let stream_layers = use_accelerator && max_vram.is_some();
    let params_backends = use_accelerator.then(|| {
        let diffusion = if stream_layers { "cpu" } else { &backend };
        // Layer streaming requires diffusion parameters on CPU. Keeping the
        // VAE on the accelerator avoids placing every component in host RAM.
        format!("te=cpu,diffusion={diffusion},vae={backend}")
    });
    ContextParams {
        diffusion_model_path: Some(paths.transformer),
        clip_l_path: Some(paths.clip_l),
        t5xxl_path: Some(paths.t5xxl),
        vae_path: Some(paths.vae),
        enable_mmap: true,
        flash_attention: use_accelerator,
        diffusion_flash_attention: use_accelerator,
        vae_format: VaeFormat::Flux,
        max_vram,
        stream_layers,
        backend: module_backends.or_else(|| Some("cpu".to_owned())),
        params_backend: params_backends,
        ..ContextParams::default()
    }
}

fn vram_budget(device: &crate::Device) -> Option<String> {
    if device.backend == Backend::Cpu || device.memory_total == 0 {
        return None;
    }
    let budget_mib = device
        .memory_total
        .saturating_mul(VRAM_BUDGET_PERCENT)
        .saturating_div(100 * MIB);
    if budget_mib == 0 {
        return None;
    }
    let whole_gib = budget_mib / 1024;
    let tenth_gib = (budget_mib % 1024) * 10 / 1024;
    Some(format!("{whole_gib}.{tenth_gib}"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const GIB: usize = 1024 * 1024 * 1024;

    fn paths() -> ModelPaths {
        ModelPaths {
            transformer: PathBuf::from("transformer.gguf"),
            clip_l: PathBuf::from("clip_l.safetensors"),
            t5xxl: PathBuf::from("t5xxl.gguf"),
            vae: PathBuf::from("ae.safetensors"),
        }
    }

    #[test]
    fn maps_flux_components_to_native_context_fields() {
        let params = context_params(&crate::Device::cpu(), paths());

        assert_eq!(
            params.diffusion_model_path.as_deref(),
            Some(Path::new("transformer.gguf"))
        );
        assert_eq!(
            params.clip_l_path.as_deref(),
            Some(Path::new("clip_l.safetensors"))
        );
        assert_eq!(params.t5xxl_path.as_deref(), Some(Path::new("t5xxl.gguf")));
        assert!(params.llm_path.is_none());
        assert_eq!(
            params.vae_path.as_deref(),
            Some(Path::new("ae.safetensors"))
        );
        assert_eq!(params.vae_format, VaeFormat::Flux);
    }

    #[test]
    fn reserves_one_quarter_of_accelerator_memory_for_the_pipeline() {
        let mut device = crate::Device::cuda(0);
        device.memory_total = 16 * GIB;

        let params = context_params(&device, paths());

        assert_eq!(
            params.backend.as_deref(),
            Some("te=cpu,diffusion=cuda0,vae=cuda0")
        );
        assert_eq!(
            params.params_backend.as_deref(),
            Some("te=cpu,diffusion=cpu,vae=cuda0")
        );
        assert_eq!(params.max_vram.as_deref(), Some("cuda0=12.0"));
        assert!(params.stream_layers);
    }

    #[test]
    fn cpu_context_does_not_enable_accelerator_streaming() {
        let params = context_params(&crate::Device::cpu(), paths());

        assert_eq!(params.backend.as_deref(), Some("cpu"));
        assert!(params.params_backend.is_none());
        assert!(params.max_vram.is_none());
        assert!(!params.stream_layers);
    }
}
