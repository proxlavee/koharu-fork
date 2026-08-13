---
title: Install Koharu
description: Install and launch a Koharu release build.
---

# Install Koharu

Use a release build unless you intend to modify Koharu itself. This repository publishes only a 64-bit Windows installer.

## Download a release

Open the [latest GitHub release](https://github.com/proxlavee/koharu-fork/releases/latest) and run the NSIS installer. The installer is not code-signed and may trigger Microsoft Defender SmartScreen. This repository does not publish macOS or Linux application binaries.

The release package includes the pinned Chromium Embedded Framework runtime and its resources. Koharu does not require a system browser.

## First launch

Koharu opens a project browser after the native runtime is ready. The first launch can take longer because Koharu may need to download native runtime packages. Individual model files are resolved when the selected model is first used.

Downloads require access to GitHub release assets and, for model weights, Hugging Face. Progress appears in the activity center. Do not close the application while a package is being published to the local cache.

## Updates

Koharu checks this repository's published GitHub releases and can launch the matching Windows installer from inside the application. You can also close Koharu and install the newer package manually from GitHub Releases.

## Next step

Create a project and process a page in [Translate your first project](/getting-started/first-project/). For hardware selection and cache behavior, see [Runtimes, models, and hardware](/getting-started/runtime-models-and-hardware/).

To build from source instead, use the [development setup](/development/setup/).
