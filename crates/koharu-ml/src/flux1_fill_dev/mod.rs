//! FLUX.1 Fill Dev inpainting.
//!
//! Component and sampling behavior follows stable-diffusion.cpp at commit
//! cc734292286f85f9c48305d94d7fd22f42838522:
//! https://github.com/leejet/stable-diffusion.cpp/blob/cc734292286f85f9c48305d94d7fd22f42838522/docs/flux.md

mod model;
mod processor;

use anyhow::{Context, Result, ensure};
use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, RgbImage};
use koharu_diffusion::{
    GuidanceParams, ImageGenerationParams, SampleMethod, SampleParams, Scheduler,
};

use self::{
    model::{Model, ModelPaths},
    processor::Flux1ImageProcessor,
};

pub use self::processor::Flux1FillDevInpaintOptions;

model_repository!("YarvixPA/FLUX.1-Fill-dev-GGUF" @ "78b83f1da140a4dcd6466580516544e1f6effe3e" {
    TRANSFORMER_WEIGHTS = "flux1-fill-dev-Q4_K_S.gguf"
});
model_repository!("city96/t5-v1_1-xxl-encoder-gguf" @ "005a6ea51a7d0b84d677b3e633bb52a8c85a83d9" {
    T5XXL_WEIGHTS = "t5-v1_1-xxl-encoder-Q5_K_M.gguf"
});
model_repository!("comfyanonymous/flux_text_encoders" @ "6af2a98e3f615bdfa612fbd85da93d1ed5f69ef5" {
    CLIP_L_WEIGHTS = "clip_l.safetensors"
});
model_repository!("Comfy-Org/Lumina_Image_2.0_Repackaged" @ "22e393d707f2d13e736b1a461c958644258cd9d9" {
    VAE_WEIGHTS = "split_files/vae/ae.safetensors"
});

#[derive(Debug)]
pub struct Flux1FillDevInpaint {
    model: Model,
}

impl Flux1FillDevInpaint {
    pub async fn load(device: crate::Device) -> Result<Self> {
        let (transformer, clip_l, t5xxl, vae) = tokio::try_join!(
            TRANSFORMER_WEIGHTS.resolve(),
            CLIP_L_WEIGHTS.resolve(),
            T5XXL_WEIGHTS.resolve(),
            VAE_WEIGHTS.resolve(),
        )
        .context("failed to resolve FLUX.1 Fill Dev model assets")?;
        let model = Model::new(
            &device,
            ModelPaths {
                transformer,
                clip_l,
                t5xxl,
                vae,
            },
        )?;
        Ok(Self { model })
    }

    pub fn inference(
        &self,
        prompt: &str,
        image: &DynamicImage,
        mask_image: &DynamicImage,
        options: &Flux1FillDevInpaintOptions,
    ) -> Result<DynamicImage> {
        ensure!(
            image.width() == mask_image.width() && image.height() == mask_image.height(),
            "image/mask dimensions differ: image={}x{}, mask={}x{}",
            image.width(),
            image.height(),
            mask_image.width(),
            mask_image.height()
        );
        ensure!(
            !prompt.contains('\0'),
            "prompt contains an interior NUL byte"
        );
        ensure!(
            options.strength > 0.0 && options.strength <= 1.0,
            "FLUX.1 inpaint strength must be greater than zero and at most one"
        );
        ensure!(
            options.num_inference_steps > 0,
            "num_inference_steps must be greater than zero"
        );

        let mut image = image.clone();
        if u64::from(image.width()) * u64::from(image.height()) > 1024 * 1024 {
            image = Flux1ImageProcessor::_resize_to_target_area(&image, 1024 * 1024);
        }
        let width = (image.width() / 16) * 16;
        let height = (image.height() / 16) * 16;
        ensure!(width > 0 && height > 0);
        let source = image.to_rgb8();
        let mut image = RgbImage::new(width, height);
        Resizer::new()
            .resize(
                &source,
                &mut image,
                &ResizeOptions::new()
                    .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
                    .use_alpha(false),
            )
            .expect("source and destination images have the same pixel type");
        let source_mask = mask_image.to_luma8();
        let mut mask_image = image::GrayImage::new(width, height);
        Resizer::new()
            .resize(
                &source_mask,
                &mut mask_image,
                &ResizeOptions::new()
                    .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
                    .use_alpha(false),
            )
            .expect("source and destination masks have the same pixel type");

        let crop_coords = options.padding_mask_crop.and_then(|padding| {
            Flux1ImageProcessor::get_crop_region(&mask_image, width, height, padding)
        });
        let (init_image, mut native_mask) = if let Some((x1, y1, x2, y2)) = crop_coords {
            let image_crop = DynamicImage::ImageRgb8(
                image::imageops::crop_imm(&image, x1, y1, x2 - x1, y2 - y1).to_image(),
            );
            let mask_crop = DynamicImage::ImageLuma8(
                image::imageops::crop_imm(&mask_image, x1, y1, x2 - x1, y2 - y1).to_image(),
            );
            (
                Flux1ImageProcessor::_resize_and_fill(&image_crop, width, height).to_rgb8(),
                Flux1ImageProcessor::_resize_and_fill(&mask_crop, width, height).to_luma8(),
            )
        } else {
            (image.clone(), mask_image.clone())
        };
        Flux1ImageProcessor::binarize(&mut native_mask);

        let generated = self
            .model
            .forward(&ImageGenerationParams {
                prompt: prompt.to_owned(),
                width: i32::try_from(width)?,
                height: i32::try_from(height)?,
                init_image: Some(init_image),
                mask_image: Some(native_mask),
                sample: sample_params(options)?,
                seed: options.seed,
                batch_count: 1,
                strength: options.strength as f32,
                ..ImageGenerationParams::default()
            })?
            .into_iter()
            .next()
            .context("FLUX.1 Fill Dev returned no inpainted image")?;

        let generated =
            Flux1ImageProcessor::apply_overlay(&mask_image, &image, generated, crop_coords)?;
        Ok(DynamicImage::ImageRgb8(generated))
    }
}

fn sample_params(options: &Flux1FillDevInpaintOptions) -> Result<SampleParams> {
    Ok(SampleParams {
        guidance: GuidanceParams {
            text_cfg: 1.0,
            distilled_guidance: 30.0,
            ..GuidanceParams::default()
        },
        scheduler: Scheduler::Flux,
        sample_method: SampleMethod::Euler,
        sample_steps: i32::try_from(options.num_inference_steps)?,
        ..SampleParams::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use koharu_runtime::HuggingFaceFile;

    #[test]
    fn model_components_are_commit_pinned() {
        assert_eq!(
            TRANSFORMER_WEIGHTS,
            HuggingFaceFile::pinned(
                "YarvixPA/FLUX.1-Fill-dev-GGUF",
                "78b83f1da140a4dcd6466580516544e1f6effe3e",
                "flux1-fill-dev-Q4_K_S.gguf",
            )
        );
        assert_eq!(
            T5XXL_WEIGHTS,
            HuggingFaceFile::pinned(
                "city96/t5-v1_1-xxl-encoder-gguf",
                "005a6ea51a7d0b84d677b3e633bb52a8c85a83d9",
                "t5-v1_1-xxl-encoder-Q5_K_M.gguf",
            )
        );
        assert_eq!(
            CLIP_L_WEIGHTS,
            HuggingFaceFile::pinned(
                "comfyanonymous/flux_text_encoders",
                "6af2a98e3f615bdfa612fbd85da93d1ed5f69ef5",
                "clip_l.safetensors",
            )
        );
        assert_eq!(
            VAE_WEIGHTS,
            HuggingFaceFile::pinned(
                "Comfy-Org/Lumina_Image_2.0_Repackaged",
                "22e393d707f2d13e736b1a461c958644258cd9d9",
                "split_files/vae/ae.safetensors",
            )
        );
    }

    #[test]
    fn sampling_defaults_match_flux_fill_dev() {
        let sample = sample_params(&Flux1FillDevInpaintOptions::default()).unwrap();

        assert_eq!(sample.guidance.text_cfg, 1.0);
        assert_eq!(sample.guidance.distilled_guidance, 30.0);
        assert_eq!(sample.scheduler, Scheduler::Flux);
        assert_eq!(sample.sample_method, SampleMethod::Euler);
        assert_eq!(sample.sample_steps, 50);
    }
}
