<h1 align="center">Koharu</h1>

<p align="center">ML-powered manga translator, written in <b>Rust</b>.</p>

<p align="center">
<a href="https://github.com/proxlavee/koharu-fork/releases/latest" target="_blank"><img alt="GitHub Downloads (all assets, all releases)" src="https://img.shields.io/github/downloads/proxlavee/koharu-fork/total?style=for-the-badge&link=https%3A%2F%2Fgithub.com%2Fproxlavee%2Fkoharu-fork%2Freleases%2Flatest"></a>
</p>

<p align="center">
<a href="https://trendshift.io/repositories/20649" target="_blank"><img src="https://trendshift.io/api/badge/repositories/20649" alt="mayocream%2Fkoharu | Trendshift" style="width: 250px; height: 55px;" width="250" height="55"/></a>
</p>

<p align="center">
<a href="https://koharu.rs/getting-started/install/" target="_blank">Getting Started</a> · <a href="https://koharu.rs/" target="_blank">Docs</a> · <a href="https://github.com/mayocream/koharu/issues" target="_blank">Bug reports</a> · <a href="https://discord.gg/mHvHkxGnUY" target="_blank">Discord</a>
</p>

<p align="center">
<a href="https://koharu.rs/ja-JP/" target="_blank">日本語</a> | <a href="https://koharu.rs/zh-CN/" target="_blank">简体中文</a>
</p>

Koharu introduces a local-first workflow for manga translation, utilizing the power of ML to automate the process. It combines the capabilities of object detection, OCR, inpainting, and LLMs to create a seamless translation experience.

> [!NOTE]
> Koharu runs its vision models and LLMs **locally** on your machine to keep your data private and secure.

---

![screenshot](docs/screenshot.png)

> [!NOTE]
> Support and discussion are available on the [Discord server](https://discord.gg/mHvHkxGnUY).

## Features

- Automatic detection of text regions, speech bubbles, and cleanup masks
- OCR for manga dialogue, captions, and other page text
- Inpainting to remove source lettering from the page
- Translation with local or remote LLM backends
- Advanced text rendering with vertical CJK and RTL support
- Layered PSD export with editable text

## GPU Acceleration

The 64-bit Windows build supports CUDA, ROCm / HIP, and Vulkan acceleration. CPU fallback is available when an accelerated path cannot be used.

### CUDA

Koharu supports NVIDIA GPUs on Windows through CUDA. Ensure you have the latest NVIDIA driver installed.

### HIP / ROCm

Koharu supports AMD GPUs on Windows through ROCm and HIP. Ensure you have the latest AMD driver installed.

### Vulkan

Koharu also supports Vulkan on Windows as an alternative to CUDA and HIP.

## Machine Learning Models

Koharu uses a staged stack of vision and language models instead of trying to solve the entire page with a single network.

### Computer Vision Models

Koharu uses multiple pretrained models, each tuned for a specific part of the page pipeline.

#### Detection and Layout

Koharu uses object detection to find text regions, speech bubbles, and segmentation masks.

- [Koharu Layout RF-DETR Seg 2XL](https://huggingface.co/mayocream/koharu-layout-rfdetr-seg-2xl-1152)

#### OCR

These models recognize source text after detection.

- [PaddleOCR VL 1.6](https://huggingface.co/PaddlePaddle/PaddleOCR-VL-1.6)
- [Manga OCR](https://huggingface.co/mayocream/manga-ocr)
- [Baberu OCR](https://huggingface.co/genshiai-daichi/baberu-ocr)

#### Inpainting

These models remove source lettering before translated text is rendered back onto the page.

- [FLUX.1 Fill Dev](https://huggingface.co/YarvixPA/FLUX.1-Fill-dev-GGUF) (about 10 GiB; 16 GiB system memory minimum; [non-commercial FLUX.1 Dev license](https://huggingface.co/black-forest-labs/FLUX.1-dev/blob/main/LICENSE.md))
- [FLUX.2 Klein](https://huggingface.co/unsloth/FLUX.2-klein-4B-GGUF)
- [RORem mixed](https://huggingface.co/mayocream/RORem-mixed-GGUF)
- [LaMa](https://huggingface.co/mayocream/lama-manga)
- [AOT GAN](https://huggingface.co/mayocream/aot-inpainting)

### Large Language Models

Koharu has a flexible LLM backend that can run locally or connect to a remote API.

#### General-Purpose Local Models

- LFM 2.5: [lfm2.5-1.2b-instruct](https://huggingface.co/LiquidAI/LFM2.5-1.2B-Instruct-GGUF)
- Ministral 3: [ministral-3-8b-instruct](https://huggingface.co/mistralai/Ministral-3-8B-Instruct-2512-GGUF)
- Gemma 4 instruct (QAT): [gemma4-e2b-it](https://huggingface.co/unsloth/gemma-4-E2B-it-qat-GGUF), [gemma4-e4b-it](https://huggingface.co/unsloth/gemma-4-E4B-it-qat-GGUF), [gemma4-12b-it](https://huggingface.co/unsloth/gemma-4-12B-it-qat-GGUF), [gemma4-26b-a4b-it](https://huggingface.co/unsloth/gemma-4-26B-A4B-it-qat-GGUF), [gemma4-31b-it](https://huggingface.co/unsloth/gemma-4-31B-it-qat-GGUF)
- Qwen 3.5: [qwen3.5-0.8b](https://huggingface.co/unsloth/Qwen3.5-0.8B-GGUF), [qwen3.5-2b](https://huggingface.co/unsloth/Qwen3.5-2B-GGUF), [qwen3.5-4b](https://huggingface.co/unsloth/Qwen3.5-4B-GGUF), [qwen3.5-9b](https://huggingface.co/unsloth/Qwen3.5-9B-GGUF), [qwen3.5-27b](https://huggingface.co/unsloth/Qwen3.5-27B-GGUF), [qwen3.5-35b-a3b](https://huggingface.co/unsloth/Qwen3.5-35B-A3B-GGUF)
- Qwen 3.6: [qwen3.6-27b](https://huggingface.co/unsloth/Qwen3.6-27B-GGUF), [qwen3.6-35b-a3b](https://huggingface.co/unsloth/Qwen3.6-35B-A3B-GGUF)

#### Uncensored Local Models

- Gemma 4 uncensored: [gemma4-e2b-uncensored](https://huggingface.co/HauhauCS/Gemma-4-E2B-Uncensored-HauhauCS-Aggressive), [gemma4-e4b-uncensored](https://huggingface.co/HauhauCS/Gemma-4-E4B-Uncensored-HauhauCS-Aggressive), [gemma4-12b-uncensored](https://huggingface.co/HauhauCS/Gemma4-12B-QAT-Uncensored-HauhauCS-Balanced), [gemma4-26b-a4b-uncensored](https://huggingface.co/HauhauCS/Gemma4-26B-A4B-QAT-Uncensored-HauhauCS-Balanced-MTP), [gemma4-31b-uncensored](https://huggingface.co/HauhauCS/Gemma4-31B-QAT-Uncensored-HauhauCS-Balanced-MTP)
- Qwen 3.5 uncensored: [qwen3.5-2b-uncensored](https://huggingface.co/HauhauCS/Qwen3.5-2B-Uncensored-HauhauCS-Aggressive), [qwen3.5-4b-uncensored](https://huggingface.co/HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive), [qwen3.5-9b-uncensored](https://huggingface.co/HauhauCS/Qwen3.5-9B-Uncensored-HauhauCS-Aggressive)
- Qwen 3.6 uncensored: [qwen3.6-27b-uncensored](https://huggingface.co/HauhauCS/Qwen3.6-27B-Uncensored-HauhauCS-Balanced), [qwen3.6-35b-a3b-uncensored](https://huggingface.co/HauhauCS/Qwen3.6-35B-A3B-Uncensored-HauhauCS-Aggressive)

#### Cloud Providers

Koharu supports hosted APIs from [Atlas Cloud](https://www.atlascloud.ai/), [OpenAI](https://platform.openai.com/), [Gemini](https://ai.google.dev/), [Claude](https://www.anthropic.com/api), [DeepSeek](https://platform.deepseek.com/), and [OpenRouter](https://openrouter.ai/).

#### Machine Translation Providers

For pure machine-translation use cases, Koharu also supports [DeepL](https://www.deepl.com/), [Google Cloud Translation](https://cloud.google.com/translate), and [Caiyun](https://fanyi.caiyunapp.com/).

#### OpenAI-Compatible Providers

Koharu supports any provider that implements the OpenAI-compatible API.

## Installation

Download the latest 64-bit Windows installer from this repository's [releases page](https://github.com/proxlavee/koharu-fork/releases/latest).

The installer is not code-signed and may trigger Microsoft Defender SmartScreen. This repository does not publish macOS or Linux application binaries.

## Troubleshooting

You can also set the `RUST_LOG` environment variable to `debug` or `trace` to see more verbose logs:

```bash
# Windows (PowerShell)
$env:RUST_LOG="debug"; koharu.exe
```

## Development

To build Koharu from source, follow the steps below.

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.95 or later (Rust 2024 edition)
- [Bun](https://bun.sh/) 1.0 or later
- [LLVM](https://llvm.org/) 15 or later
- [ninja](https://ninja-build.org/) 1.11 or later

### Install dependencies

```bash
bun install
```

### Development

```bash
bun dev
```

### Build

```bash
bun run build
```

The built binaries are written to `target/release`.

## Sponsorship

If Koharu is useful in your workflow, consider sponsoring the project.

- [GitHub Sponsors](https://github.com/sponsors/mayocream)
- [Patreon](https://www.patreon.com/mayocream)

![sponsors](./.github/sponsorkit/sponsors.svg)

## Contributors ❤️

Thanks to all the contributors who have helped make Koharu better!

<a href="https://github.com/mayocream/koharu/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=mayocream/koharu" />
</a>

## License

Copyright 2025-2026 Mayo Takanashi and Koharu contributors.

Koharu is licensed under the [GNU General Public License version 3 only](LICENSE) (`GPL-3.0-only`).
