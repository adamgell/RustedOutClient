use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use rustix::fs::{
    mkdirat, open, openat, renameat, statat, unlinkat, AtFlags, FileType, Mode, OFlags,
};
use thiserror::Error;

use crate::ssh::{normalize_inventory_snapshot, InventorySnapshot};

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
    #[error("cache inventory failed validation")]
    InvalidInventory,
    #[error("cache temporary-file cleanup failed")]
    CleanupFailed,
    #[error("cache replacement committed but directory durability is unknown")]
    CommittedDurabilityUnknown,
}

pub struct InventoryCache {
    path: PathBuf,
}

impl InventoryCache {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<InventorySnapshot, CacheError> {
        let location = open_location(&self.path, false, &NoDirectoryCreateObserver)?;
        let mut file = open_existing_file(&location)?.ok_or_else(|| {
            CacheError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "cache file not found",
            ))
        })?;
        let mut payload = Vec::new();
        file.read_to_end(&mut payload)?;
        let snapshot: InventorySnapshot = serde_json::from_slice(&payload)?;
        normalize_inventory_snapshot(snapshot, true).map_err(|_| CacheError::InvalidInventory)
    }

    pub fn save(&self, snapshot: &InventorySnapshot) -> Result<(), CacheError> {
        self.save_inner(snapshot, &NoFaults, &NoDirectoryCreateObserver)
    }

    fn save_inner(
        &self,
        snapshot: &InventorySnapshot,
        faults: &dyn SaveFaultInjector,
        observer: &dyn DirectoryCreateObserver,
    ) -> Result<(), CacheError> {
        faults.check(SavePoint::Serialization)?;
        let payload = serde_json::to_vec(snapshot)?;
        let location = open_location(&self.path, true, observer)?;
        drop(open_existing_file(&location)?);

        faults.check(SavePoint::TemporaryCreate)?;
        let (temporary_name, mut temporary_file) = create_temporary_file(&location)?;
        let mut committed = false;
        let result = (|| -> Result<(), CacheError> {
            #[cfg(unix)]
            temporary_file.set_permissions(fs::Permissions::from_mode(0o600))?;
            faults.check(SavePoint::TemporaryWrite)?;
            temporary_file.write_all(&payload)?;
            faults.check(SavePoint::TemporarySync)?;
            temporary_file.sync_all()?;
            faults.check(SavePoint::Rename)?;
            renameat(
                &location.directory,
                temporary_name.as_os_str(),
                &location.directory,
                location.filename.as_os_str(),
            )
            .map_err(errno_to_io)?;
            committed = true;

            if faults.check(SavePoint::DirectorySync).is_err()
                || location.directory.sync_all().is_err()
            {
                return Err(CacheError::CommittedDurabilityUnknown);
            }
            Ok(())
        })();

        if !committed
            && unlinkat(
                &location.directory,
                temporary_name.as_os_str(),
                AtFlags::empty(),
            )
            .is_err()
        {
            return Err(CacheError::CleanupFailed);
        }
        result
    }

    #[cfg(test)]
    fn save_with_fault(
        &self,
        snapshot: &InventorySnapshot,
        fault: SaveFault,
    ) -> Result<(), CacheError> {
        self.save_inner(snapshot, &fault, &NoDirectoryCreateObserver)
    }

    #[cfg(test)]
    fn save_with_directory_create_hook<F>(
        &self,
        snapshot: &InventorySnapshot,
        hook: F,
    ) -> Result<(), CacheError>
    where
        F: Fn(&File, &std::ffi::OsStr),
    {
        struct HookObserver<F>(F);

        impl<F> DirectoryCreateObserver for HookObserver<F>
        where
            F: Fn(&File, &std::ffi::OsStr),
        {
            fn after_create(&self, parent: &File, leaf: &std::ffi::OsStr) {
                (self.0)(parent, leaf);
            }
        }

        let observer = HookObserver(hook);
        self.save_inner(snapshot, &NoFaults, &observer)
    }
}

struct CacheLocation {
    directory: File,
    filename: OsString,
}

trait DirectoryCreateObserver {
    fn after_create(&self, _parent: &File, _leaf: &std::ffi::OsStr) {}
}

struct NoDirectoryCreateObserver;

impl DirectoryCreateObserver for NoDirectoryCreateObserver {}

fn open_location(
    path: &Path,
    create: bool,
    observer: &dyn DirectoryCreateObserver,
) -> Result<CacheLocation, CacheError> {
    let directory_path = path.parent().ok_or_else(|| {
        CacheError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache path must have a parent directory",
        ))
    })?;
    let filename = path.file_name().ok_or_else(|| {
        CacheError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache path must have a filename",
        ))
    })?;

    let mut directory = File::from(
        open(
            if directory_path.is_absolute() {
                Path::new("/")
            } else {
                Path::new(".")
            },
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(errno_to_io)?,
    );

    let mut components = Vec::new();
    for component in directory_path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(component) => components.push(component.to_owned()),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(CacheError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cache path cannot traverse parent or prefix components",
                )))
            }
        }
    }

    for (index, component) in components.iter().enumerate() {
        let is_leaf = index + 1 == components.len();
        match openat(
            &directory,
            component.as_os_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(next) => directory = File::from(next),
            Err(error) if error == rustix::io::Errno::NOENT && create && is_leaf => {
                mkdirat(
                    &directory,
                    component.as_os_str(),
                    Mode::RUSR | Mode::WUSR | Mode::XUSR,
                )
                .map_err(errno_to_io)?;
                observer.after_create(&directory, component.as_os_str());
                let next = openat(
                    &directory,
                    component.as_os_str(),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(errno_to_io)?;
                let next = File::from(next);
                #[cfg(unix)]
                next.set_permissions(fs::Permissions::from_mode(0o700))?;
                directory = next;
            }
            Err(error) => return Err(CacheError::Io(errno_to_io(error))),
        }
    }

    let metadata = directory.metadata()?;
    #[cfg(unix)]
    if !metadata.is_dir() || metadata.mode() & 0o777 != 0o700 {
        return Err(CacheError::InsecureDirectory);
    }

    Ok(CacheLocation {
        directory,
        filename: filename.to_owned(),
    })
}

fn open_existing_file(location: &CacheLocation) -> Result<Option<File>, CacheError> {
    let stat = match statat(
        &location.directory,
        location.filename.as_os_str(),
        AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(stat) => stat,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => return Err(CacheError::Io(errno_to_io(error))),
    };
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(CacheError::InsecureFile);
    }

    let file = File::from(
        openat(
            &location.directory,
            location.filename.as_os_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(errno_to_io)?,
    );
    let metadata = file.metadata()?;
    #[cfg(unix)]
    if !metadata.is_file() || metadata.mode() & 0o777 != 0o600 {
        return Err(CacheError::InsecureFile);
    }
    Ok(Some(file))
}

fn create_temporary_file(location: &CacheLocation) -> Result<(OsString, File), CacheError> {
    for _ in 0..32 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| io::Error::other("cache temporary-name generation failed"))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let name = OsString::from(format!(".inventory-v1.{suffix}.tmp"));
        match openat(
            &location.directory,
            name.as_os_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(file) => return Ok((name, File::from(file))),
            Err(error) if error == rustix::io::Errno::EXIST => continue,
            Err(error) => return Err(CacheError::Io(errno_to_io(error))),
        }
    }
    Err(CacheError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create unique cache temporary file",
    )))
}

fn errno_to_io(error: rustix::io::Errno) -> io::Error {
    error.into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SavePoint {
    Serialization,
    TemporaryCreate,
    TemporaryWrite,
    TemporarySync,
    Rename,
    DirectorySync,
}

trait SaveFaultInjector {
    fn check(&self, point: SavePoint) -> Result<(), CacheError>;
}

struct NoFaults;

impl SaveFaultInjector for NoFaults {
    fn check(&self, _point: SavePoint) -> Result<(), CacheError> {
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SaveFault {
    Serialization,
    TemporaryCreate,
    TemporaryWrite,
    TemporarySync,
    Rename,
    DirectorySync,
}

#[cfg(test)]
impl SaveFaultInjector for SaveFault {
    fn check(&self, point: SavePoint) -> Result<(), CacheError> {
        let matches = matches!(
            (self, point),
            (Self::Serialization, SavePoint::Serialization)
                | (Self::TemporaryCreate, SavePoint::TemporaryCreate)
                | (Self::TemporaryWrite, SavePoint::TemporaryWrite)
                | (Self::TemporarySync, SavePoint::TemporarySync)
                | (Self::Rename, SavePoint::Rename)
                | (Self::DirectorySync, SavePoint::DirectorySync)
        );
        if matches {
            Err(CacheError::Io(io::Error::other(
                "synthetic cache save fault",
            )))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        fs::{self, File},
        path::Path,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{tempdir_in, TempDir};

    use rustix::fs::{symlinkat, unlinkat, AtFlags};

    use super::{CacheError, InventoryCache, SaveFault};
    use crate::{
        model::{NodeName, VmId},
        ssh::{InventorySnapshot, VmInventoryItem, VmStatus},
    };

    fn private_tempdir() -> TempDir {
        let temporary_root = std::env::temp_dir().canonicalize().unwrap();
        let directory = tempdir_in(temporary_root).unwrap();
        #[cfg(unix)]
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn snapshot(observed_at_unix_ms: u64, vmid: u32) -> InventorySnapshot {
        InventorySnapshot::new(
            observed_at_unix_ms,
            false,
            vec![VmInventoryItem {
                vmid: VmId::new(vmid).unwrap(),
                name: format!("fixture-{vmid}"),
                node: NodeName::parse("pve2").unwrap(),
                status: VmStatus::Running,
                template: false,
            }],
        )
    }

    fn filenames(directory: &Path) -> Vec<String> {
        let mut names = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn every_precommit_fault_preserves_the_previous_destination_and_cleans_temp() {
        for fault in [
            SaveFault::Serialization,
            SaveFault::TemporaryCreate,
            SaveFault::TemporaryWrite,
            SaveFault::TemporarySync,
            SaveFault::Rename,
        ] {
            let directory = private_tempdir();
            let path = directory.path().join("inventory-v1.json");
            let cache = InventoryCache::new(path.clone());
            cache.save(&snapshot(1, 107)).unwrap();
            let previous = fs::read(&path).unwrap();

            assert!(cache.save_with_fault(&snapshot(2, 205), fault).is_err());

            assert_eq!(fs::read(&path).unwrap(), previous, "fault: {fault:?}");
            assert_eq!(filenames(directory.path()), vec!["inventory-v1.json"]);
        }
    }

    #[test]
    fn postrename_sync_failure_reports_committed_durability_unknown() {
        let directory = private_tempdir();
        let path = directory.path().join("inventory-v1.json");
        let cache = InventoryCache::new(path);
        cache.save(&snapshot(1, 107)).unwrap();

        let error = cache
            .save_with_fault(&snapshot(2, 205), SaveFault::DirectorySync)
            .unwrap_err();

        assert!(matches!(error, CacheError::CommittedDurabilityUnknown));
        let loaded = cache.load().unwrap();
        assert_eq!(loaded.observed_at_unix_ms, 2);
        assert_eq!(loaded.vms[0].vmid.get(), 205);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_symlink_between_leaf_create_and_open_is_rejected() {
        let root = private_tempdir();
        let stable_parent = root.path().join("stable-parent");
        fs::create_dir(&stable_parent).unwrap();
        fs::set_permissions(&stable_parent, fs::Permissions::from_mode(0o700)).unwrap();
        let attacker = root.path().join("attacker");
        fs::create_dir(&attacker).unwrap();
        fs::set_permissions(&attacker, fs::Permissions::from_mode(0o700)).unwrap();
        let leaf = stable_parent.join("private-cache");
        let cache = InventoryCache::new(leaf.join("inventory-v1.json"));

        let result = cache.save_with_directory_create_hook(
            &snapshot(1, 107),
            |parent: &File, leaf_name: &OsStr| {
                unlinkat(parent, leaf_name, AtFlags::REMOVEDIR).unwrap();
                symlinkat(&attacker, parent, leaf_name).unwrap();
            },
        );

        assert!(result.is_err());
        assert!(fs::symlink_metadata(&leaf)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!attacker.join("inventory-v1.json").exists());
    }
}
