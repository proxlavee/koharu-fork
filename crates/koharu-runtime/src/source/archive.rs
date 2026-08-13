use std::{
    fs::{File, create_dir_all},
    io::copy,
    path::Path,
};

use anyhow::{Context, Result};
use fast_glob::glob_match;
use flate2::read::GzDecoder;

pub(crate) fn extract(archive: &Path, destination: &Path, patterns: &[&str]) -> Result<()> {
    match archive.extension().and_then(|value| value.to_str()) {
        Some("zip" | "whl") => unzip(archive, destination, patterns),
        Some("gz") => untar(archive, destination, patterns),
        _ => anyhow::bail!("unsupported archive {}", archive.display()),
    }
}

fn selected(path: &Path, patterns: &[&str]) -> bool {
    let path = path.to_string_lossy();
    patterns
        .iter()
        .any(|pattern| glob_match(pattern, path.as_bytes()))
}

fn unzip(archive: &Path, destination: &Path, patterns: &[&str]) -> Result<()> {
    let archive_name = archive.display().to_string();
    let archive_file =
        File::open(archive).with_context(|| format!("failed to open {archive_name}"))?;
    let mut archive = zip::ZipArchive::new(archive_file)
        .with_context(|| format!("failed to read ZIP archive {archive_name}"))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("failed to read entry {index} from {archive_name}"))?;
        let Some(path) = entry.enclosed_name() else {
            continue;
        };
        if entry.is_dir() || !selected(&path, patterns) {
            continue;
        }
        let output = destination.join(path);
        let parent = output.parent().context("archive entry has no parent")?;
        create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
        let mut file = File::create(&output)
            .with_context(|| format!("failed to create extracted file {}", output.display()))?;
        copy(&mut entry, &mut file)
            .with_context(|| format!("failed to extract {}", output.display()))?;
    }
    Ok(())
}

fn untar(archive: &Path, destination: &Path, patterns: &[&str]) -> Result<()> {
    let archive_name = archive.display().to_string();
    let archive_file =
        File::open(archive).with_context(|| format!("failed to open {archive_name}"))?;
    let mut archive = tar::Archive::new(GzDecoder::new(archive_file));
    let entries = archive
        .entries()
        .with_context(|| format!("failed to read TAR archive {archive_name}"))?;
    for (index, entry) in entries.enumerate() {
        let mut entry =
            entry.with_context(|| format!("failed to read entry {index} from {archive_name}"))?;
        let path = entry
            .path()
            .with_context(|| format!("entry {index} in {archive_name} has an invalid path"))?
            .into_owned();
        if selected(&path, patterns) {
            entry.unpack_in(destination).with_context(|| {
                format!("failed to extract {} from {archive_name}", path.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_are_path_aware() {
        assert!(selected(Path::new("bin/runtime.dll"), &["**/*.dll"]));
        assert!(!selected(Path::new("bin/runtime.so"), &["**/*.dll"]));
    }
}
