//! Native FLUX.1 Fill Dev model assembly.
//!
//! The component mapping and module backend names follow stable-diffusion.cpp
//! at commit cc734292286f85f9c48305d94d7fd22f42838522:
//! https://github.com/leejet/stable-diffusion.cpp/blob/cc734292286f85f9c48305d94d7fd22f42838522/docs/backend.md

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, ensure};
use koharu_diffusion::{
    CancelMode, Context, ContextParams, ImageGenerationParams, Progress, RgbImage, VaeFormat,
    clear_progress_callback, set_progress_callback,
};

use crate::{Backend, InferenceControl, InferenceProgress};

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
struct ContextPlan {
    paths: ModelPaths,
    use_accelerator: bool,
    max_vram: Option<String>,
    stream_layers: bool,
    backend: String,
    params_backend: Option<String>,
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
        context.cancel(CancelMode::Reset);
        context
            .generate_image(params)
            .context("FLUX.1 Fill Dev inference failed")
    }

    pub fn forward_with_control(
        &self,
        params: &ImageGenerationParams,
        control: &InferenceControl,
    ) -> Result<Vec<RgbImage>> {
        let mut context = self
            .context
            .lock()
            .map_err(|_| anyhow!("FLUX.1 Fill Dev context lock was poisoned"))?;
        context.cancel(CancelMode::Reset);

        let progress_callback = NativeProgressCallback::install(control.clone())?;
        let cancellation = CancellationMonitor::start(context.cancel_handle(), control.clone())?;
        let result = context
            .generate_image(params)
            .context("FLUX.1 Fill Dev inference failed");
        drop(cancellation);
        context.cancel(CancelMode::Reset);
        progress_callback.clear()?;
        result
    }
}

struct NativeProgressCallback {
    active: bool,
}

impl NativeProgressCallback {
    fn install(control: InferenceControl) -> Result<Self> {
        set_progress_callback(move |Progress { step, steps, .. }| {
            let total = usize::try_from(steps.max(0)).unwrap_or_default();
            let completed = usize::try_from(step.max(0)).unwrap_or_default().min(total);
            control.report(InferenceProgress { completed, total });
        })
        .context("failed to install FLUX.1 progress callback")?;
        Ok(Self { active: true })
    }

    fn clear(mut self) -> Result<()> {
        clear_progress_callback().context("failed to clear FLUX.1 progress callback")?;
        self.active = false;
        Ok(())
    }
}

impl Drop for NativeProgressCallback {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = clear_progress_callback()
        {
            tracing::warn!(%error, "failed to clear FLUX.1 progress callback during cleanup");
        }
    }
}

struct CancellationMonitor {
    finished: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CancellationMonitor {
    fn start(cancel: koharu_diffusion::CancelHandle, control: InferenceControl) -> Result<Self> {
        let finished = Arc::new(AtomicBool::new(false));
        let monitor_finished = Arc::clone(&finished);
        let thread = thread::Builder::new()
            .name("flux1-cancellation".to_owned())
            .spawn(move || {
                while !monitor_finished.load(Ordering::Acquire) {
                    if control.cancellation_requested() {
                        cancel.cancel(CancelMode::All);
                        return;
                    }
                    thread::park_timeout(Duration::from_millis(50));
                }
            })
            .context("failed to start FLUX.1 cancellation monitor")?;
        Ok(Self {
            finished,
            thread: Some(thread),
        })
    }
}

impl Drop for CancellationMonitor {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            if thread.join().is_err() {
                tracing::warn!("FLUX.1 cancellation monitor panicked");
            }
        }
    }
}

fn context_params(device: &crate::Device, paths: ModelPaths) -> ContextParams {
    let plan = context_plan(device, paths);
    ContextParams {
        diffusion_model_path: Some(plan.paths.transformer),
        clip_l_path: Some(plan.paths.clip_l),
        t5xxl_path: Some(plan.paths.t5xxl),
        vae_path: Some(plan.paths.vae),
        enable_mmap: true,
        flash_attention: plan.use_accelerator,
        diffusion_flash_attention: plan.use_accelerator,
        vae_format: VaeFormat::Flux,
        max_vram: plan.max_vram,
        stream_layers: plan.stream_layers,
        backend: Some(plan.backend),
        params_backend: plan.params_backend,
        ..ContextParams::default()
    }
}

fn context_plan(device: &crate::Device, paths: ModelPaths) -> ContextPlan {
    let use_accelerator = device.backend != Backend::Cpu;
    let backend = device.name.to_ascii_lowercase();
    let module_backends =
        use_accelerator.then(|| format!("te=cpu,diffusion={backend},vae={backend}"));
    let max_vram = vram_budget(device).map(|budget| format!("{backend}={budget}"));
    let stream_layers = use_accelerator && max_vram.is_some();
    // Unspecified parameter backends follow their module runtime backend.
    // Keeping diffusion parameters on CPU would transfer them for every layer
    // and defeat max-VRAM residency and prefetching.
    let params_backends = use_accelerator.then(|| "te=cpu".to_owned());
    ContextPlan {
        paths,
        use_accelerator,
        max_vram,
        stream_layers,
        backend: module_backends.unwrap_or_else(|| "cpu".to_owned()),
        params_backend: params_backends,
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
    fn maps_flux_components_to_the_context_plan() {
        let plan = context_plan(&crate::Device::cpu(), paths());

        assert_eq!(
            plan.paths.transformer.as_path(),
            Path::new("transformer.gguf")
        );
        assert_eq!(plan.paths.clip_l.as_path(), Path::new("clip_l.safetensors"));
        assert_eq!(plan.paths.t5xxl.as_path(), Path::new("t5xxl.gguf"));
        assert_eq!(plan.paths.vae.as_path(), Path::new("ae.safetensors"));
    }

    #[test]
    fn reserves_one_quarter_of_accelerator_memory_for_the_pipeline() {
        let mut device = crate::Device::cuda(0);
        device.memory_total = 16 * GIB;

        let plan = context_plan(&device, paths());

        assert_eq!(plan.backend, "te=cpu,diffusion=cuda0,vae=cuda0");
        assert_eq!(plan.params_backend.as_deref(), Some("te=cpu"));
        assert_eq!(plan.max_vram.as_deref(), Some("cuda0=12.0"));
        assert!(plan.stream_layers);
        assert!(plan.use_accelerator);
    }

    #[test]
    fn cpu_context_does_not_enable_accelerator_streaming() {
        let plan = context_plan(&crate::Device::cpu(), paths());

        assert_eq!(plan.backend, "cpu");
        assert!(plan.params_backend.is_none());
        assert!(plan.max_vram.is_none());
        assert!(!plan.stream_layers);
        assert!(!plan.use_accelerator);
    }
}
