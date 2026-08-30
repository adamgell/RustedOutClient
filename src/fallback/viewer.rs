use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use uuid::Uuid;

use crate::runtime::RuntimeDir;

const MAX_VIEWER_BYTES: u64 = 64 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ViewerSnapshotErrorKind {
    Source,
    Snapshot,
}

pub(super) struct ViewerSnapshotError {
    kind: ViewerSnapshotErrorKind,
    cleanup_failed: bool,
    owner: Option<ViewerSnapshot>,
}

impl ViewerSnapshotError {
    fn source() -> Self {
        Self {
            kind: ViewerSnapshotErrorKind::Source,
            cleanup_failed: false,
            owner: None,
        }
    }

    fn snapshot(owner: Option<ViewerSnapshot>, cleanup_failed: bool) -> Self {
        Self {
            kind: ViewerSnapshotErrorKind::Snapshot,
            cleanup_failed,
            owner,
        }
    }

    pub(super) fn is_source(&self) -> bool {
        self.kind == ViewerSnapshotErrorKind::Source
    }

    pub(super) fn has_cleanup_failure(&self) -> bool {
        self.cleanup_failed
    }
}

impl fmt::Debug for ViewerSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ViewerSnapshotError")
            .field("kind", &self.kind)
            .field("cleanup_failed", &self.cleanup_failed)
            .field("retains_owner", &self.owner.is_some())
            .finish()
    }
}

impl fmt::Display for ViewerSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fallback viewer snapshot operation failed")
    }
}

impl Error for ViewerSnapshotError {}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ViewerSnapshotFault {
    Create,
    Write,
    Sync,
}

pub(super) struct ViewerSnapshotPolicy {
    #[cfg(test)]
    pub(super) after_open: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    pub(super) fault: Option<ViewerSnapshotFault>,
    #[cfg(test)]
    pub(super) remove_failures: usize,
}

impl ViewerSnapshotPolicy {
    pub(super) fn production() -> Self {
        Self {
            #[cfg(test)]
            after_open: None,
            #[cfg(test)]
            fault: None,
            #[cfg(test)]
            remove_failures: 0,
        }
    }

    fn fail_create(&self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(ViewerSnapshotFault::Create)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn fail_write(&self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(ViewerSnapshotFault::Write)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn fail_sync(&self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(ViewerSnapshotFault::Sync)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn after_open(&mut self) {
        #[cfg(test)]
        if let Some(after_open) = self.after_open.take() {
            after_open();
        }
    }
}

pub(super) struct ViewerSnapshot {
    path: Option<PathBuf>,
    #[cfg(test)]
    remove_failures: usize,
}

impl ViewerSnapshot {
    pub(super) fn create(
        runtime: &RuntimeDir,
        configured_path: &Path,
        mut policy: ViewerSnapshotPolicy,
    ) -> Result<Self, ViewerSnapshotError> {
        let mut source = open_validated_source(configured_path)?;
        policy.after_open();
        if policy.fail_create() {
            return Err(ViewerSnapshotError::snapshot(None, false));
        }

        let (path, mut destination) = create_private_snapshot(runtime.path())
            .map_err(|_| ViewerSnapshotError::snapshot(None, false))?;
        let snapshot = Self {
            path: Some(path),
            #[cfg(test)]
            remove_failures: policy.remove_failures,
        };

        #[cfg(unix)]
        if destination
            .set_permissions(fs::Permissions::from_mode(0o500))
            .is_err()
        {
            return Err(snapshot_setup_error(snapshot));
        }

        let copy_result = copy_bounded(&mut source, &mut destination, policy.fail_write());
        let sync_result = if copy_result.is_ok() {
            if policy.fail_sync() {
                Err(io::Error::other("synthetic viewer snapshot sync failure"))
            } else {
                destination.sync_all()
            }
        } else {
            Ok(())
        };
        drop(destination);
        if copy_result.is_err() || sync_result.is_err() {
            return Err(snapshot_setup_error(snapshot));
        }
        Ok(snapshot)
    }

    pub(super) fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("viewer snapshot is unavailable after removal")
    }

    pub(super) fn remove(&mut self) -> Result<(), ()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        #[cfg(test)]
        if self.remove_failures > 0 {
            self.remove_failures -= 1;
            return Err(());
        }
        match fs::remove_file(path) {
            Ok(()) => {
                self.path.take();
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.path.take();
                Ok(())
            }
            Err(_) => Err(()),
        }
    }
}

impl fmt::Debug for ViewerSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ViewerSnapshot")
            .field("present", &self.path.is_some())
            .finish()
    }
}

impl Drop for ViewerSnapshot {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

#[cfg(test)]
pub(super) fn validate_viewer_path(configured_path: &Path) -> Result<(), ViewerSnapshotError> {
    open_validated_source(configured_path).map(drop)
}

fn open_validated_source(configured_path: &Path) -> Result<File, ViewerSnapshotError> {
    if !configured_path.is_absolute() {
        return Err(ViewerSnapshotError::source());
    }
    let source = File::open(configured_path).map_err(|_| ViewerSnapshotError::source())?;
    let metadata = source
        .metadata()
        .map_err(|_| ViewerSnapshotError::source())?;
    if !metadata.is_file() || metadata.len() > MAX_VIEWER_BYTES {
        return Err(ViewerSnapshotError::source());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(ViewerSnapshotError::source());
    }
    Ok(source)
}

fn create_private_snapshot(directory: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..16 {
        let path = directory.join(format!(".vnc-viewer-{}", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o500);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique fallback viewer snapshot",
    ))
}

fn copy_bounded(source: &mut File, destination: &mut File, fail_write: bool) -> io::Result<()> {
    let mut copied = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("viewer snapshot size overflow"))?;
        if copied > MAX_VIEWER_BYTES {
            return Err(io::Error::other("viewer snapshot exceeds size limit"));
        }
        if fail_write {
            let partial = read.min(1);
            destination.write_all(&buffer[..partial])?;
            return Err(io::Error::other("synthetic viewer snapshot write failure"));
        }
        destination.write_all(&buffer[..read])?;
    }
}

fn snapshot_setup_error(mut snapshot: ViewerSnapshot) -> ViewerSnapshotError {
    let cleanup_failed = snapshot.remove().is_err();
    ViewerSnapshotError::snapshot(Some(snapshot), cleanup_failed)
}
