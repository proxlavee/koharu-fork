---
title: Install Koharu
description: Install a release build, launch Koharu, and keep it updated.
---

# Install Koharu

Use a release build unless you intend to modify Koharu itself. This fork publishes only a 64-bit Windows installer.

## Download a release

Open the [latest GitHub release](https://github.com/proxlavee/koharu-fork/releases/latest) and run the NSIS installer. The installer is not code-signed and may trigger Microsoft Defender SmartScreen. This fork does not publish macOS or Linux application binaries.

## First launch

Koharu opens a project browser after the native runtime is ready. The first launch can take longer because Koharu may need to download native runtime packages. Individual model files are resolved when the selected model is first used.

Downloads require access to GitHub release assets and, for model weights, Hugging Face. Progress appears in the activity center. Do not close the application while a package is being published to the local cache.

## Updates

This unsigned build does not enable Tauri's signed updater. To update, close Koharu and run the newer installer from this fork's GitHub Releases page.

## Next step

Create a project and process a page in [Translate your first project](/getting-started/first-project/). For hardware selection and cache behavior, see [Runtimes, models, and hardware](/getting-started/runtime-models-and-hardware/).

To build from source instead, use the [development setup](/development/setup/).
