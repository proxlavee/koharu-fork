use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use anyhow::{Context, Result};

static LOADED: OnceLock<Mutex<HashMap<PathBuf, libloading::Library>>> = OnceLock::new();

pub(super) fn load(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let path = dunce::canonicalize(path)
        .with_context(|| format!("dynamic library does not exist: {}", path.display()))?;
    let mut loaded = LOADED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("dynamic library registry is poisoned"))?;
    if loaded.contains_key(&path) {
        return Ok(());
    }
    #[cfg(windows)]
    if let Some(parent) = path.parent() {
        add_dll_directory(parent);
    }
    let library =
        unsafe { open(&path) }.with_context(|| format!("failed to load {}", path.display()))?;
    loaded.insert(path, library);
    Ok(())
}

#[cfg(windows)]
fn add_dll_directory(dir: &Path) {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        extern "system" {
            fn AddDllDirectory(NewDirectory: *const u16) -> *mut std::ffi::c_void;
            fn SetDllDirectoryW(lpPathName: *const u16) -> i32;
        }
        AddDllDirectory(wide.as_ptr());
        SetDllDirectoryW(wide.as_ptr());
    }
}

#[cfg(windows)]
unsafe fn open(path: &Path) -> Result<libloading::Library, libloading::Error> {
    use libloading::os::windows::{
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, Library,
    };
    unsafe {
        Library::load_with_flags(
            path.as_os_str(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
        .map(Into::into)
    }
}

#[cfg(not(windows))]
unsafe fn open(path: &Path) -> Result<libloading::Library, libloading::Error> {
    use libloading::os::unix::{Library, RTLD_LAZY, RTLD_LOCAL};
    unsafe { Library::open(Some(path), RTLD_LAZY | RTLD_LOCAL).map(Into::into) }
}
