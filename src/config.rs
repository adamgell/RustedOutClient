use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{NodeName, PveProfile, ScaleMode, SshTarget, VmId};

pub const SCHEMA_VERSION: u32 = 1;
const MIN_INVENTORY_REFRESH_SECONDS: u64 = 5;
const MAX_INVENTORY_REFRESH_SECONDS: u64 = 300;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub schema_version: u32,
    pub profile: PveProfile,
    pub inventory_refresh_seconds: u64,
    pub fallback_viewer: Option<PathBuf>,
    pub clipboard_enabled: bool,
    pub favorites: Vec<FavoriteVm>,
    pub display: DisplayPreferences,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FavoriteVm {
    pub vmid: VmId,
    pub alias: Option<String>,
    pub scale_mode: ScaleMode,
    pub view_only: bool,
    pub sort_position: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DisplayPreferences {
    pub scale_mode: ScaleMode,
    pub view_only: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("unsupported configuration schema version")]
    UnsupportedSchemaVersion,
    #[error("inventory refresh must be between {MIN_INVENTORY_REFRESH_SECONDS} and {MAX_INVENTORY_REFRESH_SECONDS} seconds")]
    InvalidRefreshInterval,
    #[error("fallback viewer must be an absolute path")]
    RelativeFallbackViewer,
    #[error("could not determine the home directory")]
    MissingHomeDirectory,
    #[error("configuration I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("configuration JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

impl AppConfig {
    pub fn new(profile: PveProfile) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            profile,
            inventory_refresh_seconds: 15,
            fallback_viewer: None,
            clipboard_enabled: false,
            favorites: Vec::new(),
            display: DisplayPreferences {
                scale_mode: ScaleMode::Fit,
                view_only: false,
            },
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchemaVersion);
        }
        if !(MIN_INVENTORY_REFRESH_SECONDS..=MAX_INVENTORY_REFRESH_SECONDS)
            .contains(&self.inventory_refresh_seconds)
        {
            return Err(ConfigError::InvalidRefreshInterval);
        }
        if self
            .fallback_viewer
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(ConfigError::RelativeFallbackViewer);
        }

        Ok(())
    }
}

#[derive(Deserialize)]
struct LegacyConfig {
    ssh_target: SshTarget,
    node: NodeName,
    viewer: Option<PathBuf>,
}

pub fn import_legacy_json(json: &str) -> Result<AppConfig, ConfigError> {
    let legacy: LegacyConfig = serde_json::from_str(json)?;
    let mut config = AppConfig::new(PveProfile {
        name: "Imported Proxmox".to_owned(),
        ssh_target: legacy.ssh_target,
        node: legacy.node,
    });
    config.fallback_viewer = legacy.viewer;
    config.validate()?;
    Ok(config)
}

pub fn import_legacy_file(path: &Path) -> Result<AppConfig, ConfigError> {
    import_legacy_json(&fs::read_to_string(path)?)
}

pub fn config_directory(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("RustedOutClient")
}

pub fn config_path(home: &Path) -> PathBuf {
    config_directory(home).join("config.json")
}

pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    let home = env::var_os("HOME").ok_or(ConfigError::MissingHomeDirectory)?;
    Ok(config_path(Path::new(&home)))
}

pub fn load_config_from_path(path: &Path) -> Result<AppConfig, ConfigError> {
    let config: AppConfig = serde_json::from_slice(&fs::read(path)?)?;
    config.validate()?;
    Ok(config)
}

pub fn save_config_to_path(config: &AppConfig, path: &Path) -> Result<(), ConfigError> {
    save_config_to_path_with_renamer(config, path, &StdRenamer)
}

pub trait AtomicRenamer {
    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()>;
}

struct StdRenamer;

impl AtomicRenamer for StdRenamer {
    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
        fs::rename(source, destination)
    }
}

pub fn save_config_to_path_with_renamer(
    config: &AppConfig,
    path: &Path,
    renamer: &dyn AtomicRenamer,
) -> Result<(), ConfigError> {
    config.validate()?;
    let payload = serde_json::to_vec_pretty(config)?;
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration path must have a parent directory",
        )
    })?;
    ensure_private_directory(directory)?;

    let (temporary_path, mut temporary_file) = create_temporary_file(directory, path)?;
    let result = (|| -> io::Result<()> {
        set_private_file_mode(&temporary_file)?;
        temporary_file.write_all(&payload)?;
        temporary_file.sync_all()?;
        drop(temporary_file);
        renamer.rename(&temporary_path, path)?;
        sync_directory(directory)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }

    result.map_err(ConfigError::Io)
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn set_private_file_mode(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn create_temporary_file(directory: &Path, destination: &Path) -> io::Result<(PathBuf, File)> {
    let filename = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let process_id = std::process::id();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..16 {
        let temporary_path = directory.join(format!(
            ".{filename}.{process_id}.{timestamp}.{attempt}.tmp"
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique configuration temporary file",
    ))
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
