use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use tempfile::NamedTempFile;
use thiserror::Error;

use crate::ssh::InventorySnapshot;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("cache directory must have mode 0700")]
    InsecureDirectory,
    #[error("cache file must have mode 0600")]
    InsecureFile,
    #[error("cache I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("cache JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct InventoryCache {
    path: PathBuf,
}

impl InventoryCache {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<InventorySnapshot, CacheError> {
        let directory = parent_directory(&self.path)?;
        ensure_private_existing_directory(directory)?;
        ensure_private_existing_file(&self.path)?;

        let mut snapshot: InventorySnapshot = serde_json::from_slice(&fs::read(&self.path)?)?;
        snapshot.stale = true;
        snapshot.vms.retain(|item| !item.template);
        snapshot.vms.sort_by_key(|item| item.vmid);
        Ok(snapshot)
    }

    pub fn save(&self, snapshot: &InventorySnapshot) -> Result<(), CacheError> {
        let payload = serde_json::to_vec(snapshot)?;
        let directory = parent_directory(&self.path)?;
        ensure_private_directory(directory)?;
        ensure_private_file_if_present(&self.path)?;

        let mut temporary = NamedTempFile::new_in(directory)?;
        set_private_file_mode(temporary.as_file())?;
        temporary.write_all(&payload)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&self.path)
            .map_err(|error| CacheError::Io(error.error))?;
        File::open(directory)?.sync_all()?;
        Ok(())
    }
}

fn parent_directory(path: &Path) -> Result<&Path, CacheError> {
    path.parent().ok_or_else(|| {
        CacheError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache path must have a parent directory",
        ))
    })
}

fn ensure_private_directory(path: &Path) -> Result<(), CacheError> {
    match fs::metadata(path) {
        Ok(_) => return ensure_private_existing_directory(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(CacheError::Io(error)),
    }

    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn ensure_private_existing_directory(path: &Path) -> Result<(), CacheError> {
    let metadata = fs::metadata(path)?;
    #[cfg(unix)]
    if !metadata.is_dir() || metadata.mode() & 0o777 != 0o700 {
        return Err(CacheError::InsecureDirectory);
    }
    Ok(())
}

fn ensure_private_file_if_present(path: &Path) -> Result<(), CacheError> {
    match fs::metadata(path) {
        Ok(_) => ensure_private_existing_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CacheError::Io(error)),
    }
}

fn ensure_private_existing_file(path: &Path) -> Result<(), CacheError> {
    let metadata = fs::metadata(path)?;
    #[cfg(unix)]
    if !metadata.is_file() || metadata.mode() & 0o777 != 0o600 {
        return Err(CacheError::InsecureFile);
    }
    Ok(())
}

fn set_private_file_mode(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}
