---
title: Runtimes, Models, and Hardware
description: Understand automatic runtime selection, model downloads, caches, and CPU fallback.
---

# Runtimes, Models, and Hardware

Koharu assembles the native libraries and model files required by the features you use. These dependencies are not all embedded in the release package.

## Runtime selection

The Windows release discovers hardware during startup and selects one shared device for its ML stack:

1. CUDA when a compatible NVIDIA driver is available;
2. ROCm/HIP when a supported AMD target is discovered;
3. Vulkan when a usable Vulkan device is available;
4. CPU when no accelerator path is usable.

Availability still depends on the operating system, driver, model backend, and native package published for that platform. CPU fallback is normal and prioritizes correctness over speed.

## What gets downloaded

Koharu resolves three kinds of data on demand:

- native Torch, llama.cpp, and diffusion runtime packages;
- pinned or versioned model files;
- local GGUF quantizations selected for translation.

Runtime packages are stored under the operating system cache directory in `koharu/packages`. Project data is stored separately under `Documents/Koharu`, and settings are stored in `~/.koharu/config.toml`.

Downloads are staged and then published into the cache. A failed or interrupted download can be retried on the next launch or model use.

## Resource monitoring

The editor reports host memory, compute utilization, and model residency. Pipeline models load lazily and can remain resident for reuse. On accelerator systems, Koharu may evict an idle model when the next stage needs more memory.

Smaller local-model quantizations use less memory, usually with some quality trade-off. Begin with a moderate quantization instead of choosing the largest file your disk can hold.

Automatic inpainting expands detected text masks before inference so glyph outlines, antialiasing, and glow do not remain as silhouettes. Manual inpainting keeps the brush mask exact; paint over the full visible outline when repairing a region by hand.

## FLUX.1 Fill Dev

The optional FLUX.1 Fill Dev inpainting profile downloads about 10 GiB of pinned model files. It requires at least 16 GiB of system memory; more memory gives Windows and other applications safer headroom while the model is active.

On NVIDIA GPUs, Koharu keeps the CLIP-L and T5 text encoders on the CPU and limits the CUDA allocation to 75% of detected VRAM. On a 16 GiB RTX 5060 Ti, that budget is 12 GiB. The diffusion weights can stream from system memory, leaving GPU capacity for the VAE and for other pipeline models. Koharu still evicts idle stages when necessary.

FLUX.1 Fill Dev is governed by the [FLUX.1 Dev non-commercial license](https://huggingface.co/black-forest-labs/FLUX.1-dev/blob/main/LICENSE.md). Review that license before using generated output outside personal or evaluation workflows.

## When startup fails

Confirm that GitHub release assets and Hugging Face are reachable, update the GPU driver, and retry once. If the same package repeatedly fails, capture the full error and follow [Troubleshooting](/reference/troubleshooting/).
