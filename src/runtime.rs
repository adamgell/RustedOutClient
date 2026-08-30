use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tempfile::{Builder, TempDir};

/// A process-owned private directory for short Unix-domain socket paths.
pub struct RuntimeDir {
    _directory: TempDir,
    path: PathBuf,
    control_socket: PathBuf,
}

impl RuntimeDir {
    pub fn create() -> io::Result<Self> {
        let directory = Builder::new().prefix("roc-").tempdir_in("/tmp")?;
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let filename = directory.path().file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "runtime directory has no name")
        })?;
        let path = Path::new("/tmp").join(filename);
        let control_socket = path.join("c");
        Ok(Self {
            _directory: directory,
            path,
            control_socket,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn control_socket(&self) -> &Path {
        &self.control_socket
    }
}
