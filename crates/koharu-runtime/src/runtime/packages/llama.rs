use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use strum::EnumProperty;

use crate::{
    Hardware, Store,
    downloads::Transfer,
    runtime::{
        DiscoverablePackage, Package, RuntimePackage,
        graph::Component,
        loader,
        packages::{Cuda, Rocm},
        sealed,
    },
    source::extract,
};

const RELEASE: &str = "llama.cpp-b10430";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, strum::Display, strum::EnumProperty)]
pub(crate) enum Llama {
    #[strum(
        serialize = "windows-cuda",
        props(
            asset = "llama-cuda-windows-2022.tar.gz",
            libraries = "llama.dll,mtmd.dll"
        )
    )]
    WindowsCuda,
    #[strum(
        serialize = "windows-hip",
        props(
            asset = "llama-hip-windows-2022.tar.gz",
            libraries = "llama.dll,mtmd.dll"
        )
    )]
    WindowsHip,
    #[strum(
        serialize = "windows-vulkan",
        props(
            asset = "llama-vulkan-windows-2022.tar.gz",
            libraries = "llama.dll,mtmd.dll"
        )
    )]
    WindowsVulkan,
    #[strum(
        serialize = "linux-vulkan",
        props(
            asset = "llama-vulkan-ubuntu-24.04.tar.gz",
            libraries = "libllama.so,libmtmd.so"
        )
    )]
    LinuxVulkan,
    #[strum(
        serialize = "macos-metal",
        props(
            asset = "llama-metal-macos-latest.tar.gz",
            libraries = "libllama.dylib,libmtmd.dylib"
        )
    )]
    MacosMetal,
}

impl Llama {
    fn asset(self) -> &'static str {
        self.get_str("asset").expect("llama package has an asset")
    }

    fn libraries(self) -> impl Iterator<Item = &'static str> {
        self.get_str("libraries")
            .expect("llama package has libraries")
            .split(',')
    }

    fn complete(self, path: &Path) -> bool {
        self.libraries().all(|name| path.join(name).is_file())
    }

    fn fallbacks(self) -> Vec<Self> {
        match self {
            Self::WindowsCuda => vec![Self::WindowsHip, Self::WindowsVulkan],
            Self::WindowsHip => vec![Self::WindowsVulkan],
            Self::WindowsVulkan => Vec::new(),
            Self::LinuxVulkan => Vec::new(),
            Self::MacosMetal => Vec::new(),
        }
    }
}

impl sealed::Sealed for Llama {}

impl Package for Llama {
    async fn install(self) -> Result<PathBuf> {
        let target = Store::root()
            .join("llama")
            .join(RELEASE)
            .join(self.to_string());
        Store::directory(
            target,
            move |path| self.complete(path),
            move |stage| async move {
                let asset = self.asset();
                let url = format!(
                    "https://github.com/proxlavee/koharu-fork/releases/download/{RELEASE}/{asset}"
                );
                let archive = tempfile::Builder::new().suffix(".tar.gz").tempfile()?;
                Transfer::new()?.fetch(&url, archive.path()).await?;
                extract(
                    archive.path(),
                    &stage,
                    &["**/*.dll", "**/*.dylib", "**/*.so", "**/*.so.*"],
                )
            },
        )
        .await
    }
}

impl DiscoverablePackage for Llama {
    fn discover(hardware: &Hardware) -> Option<Self> {
        if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            if hardware.supports_cuda() {
                return Some(Self::WindowsCuda);
            }
            if Rocm::discover(hardware).is_ok() {
                return Some(Self::WindowsHip);
            }
            if hardware.supports_vulkan() {
                return Some(Self::WindowsVulkan);
            }
            None
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            hardware.supports_vulkan().then_some(Self::LinuxVulkan)
        } else if hardware.supports_metal() {
            Some(Self::MacosMetal)
        } else {
            None
        }
    }
}

impl RuntimePackage for Llama {
    const NAME: &'static str = "llama";

    fn dependencies(self, hardware: &Hardware) -> Result<Vec<Component>> {
        match self {
            Self::WindowsCuda => Ok(vec![
                Component::Cuda(Cuda::Runtime13),
                Component::Cuda(Cuda::Blas13),
            ]),
            Self::WindowsHip => Ok(vec![Component::Rocm(Rocm::discover(hardware)?)]),
            Self::WindowsVulkan | Self::LinuxVulkan | Self::MacosMetal => Ok(Vec::new()),
        }
    }

    async fn activate(self) -> Result<()> {
        let mut last_error = None;
        let mut candidates = std::iter::once(self).chain(self.fallbacks()).collect::<Vec<_>>();
        while let Some(package) = candidates.pop() {
            match package.activate_inner().await {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.expect("llama activation candidates cannot be empty"))
    }
}

impl Llama {
    async fn activate_inner(self) -> Result<()> {
        let root = self.install().await?;
        for library in self.libraries() {
            loader::load(root.join(library))
                .with_context(|| format!("failed to activate llama library {library}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Llama;

    #[test]
    fn windows_cuda_falls_back_to_hip_then_vulkan() {
        assert_eq!(
            Llama::WindowsCuda.fallbacks(),
            vec![Llama::WindowsHip, Llama::WindowsVulkan]
        );
        assert_eq!(Llama::WindowsHip.fallbacks(), vec![Llama::WindowsVulkan]);
        assert!(Llama::WindowsVulkan.fallbacks().is_empty());
    }
}
