use std::{
    env, fs,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use thiserror::Error;
use tracing_subscriber::fmt::MakeWriter;

use crate::{
    config::config_directory,
    connection::{DesktopSize, ResizeProtocolOutcome},
    fallback::{FallbackError, FallbackPreferences},
    model::VmId,
    session::{PublicError, SessionPhase},
};

pub const LOG_FILE_NAME: &str = "diagnostics.log";
pub const LOG_BACKUP_FILE_NAME: &str = "diagnostics.log.1";
pub const DEFAULT_LOG_MAX_BYTES: u64 = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLogEvent {
    OpenRequested {
        vmid: VmId,
        dynamic_resolution: bool,
        view_only: bool,
        clipboard_enabled: bool,
    },
    PhaseChanged {
        vmid: VmId,
        from: SessionPhase,
        to: SessionPhase,
        duration: Duration,
    },
    GuestSize {
        vmid: VmId,
        size: DesktopSize,
    },
    ResizeRequested {
        vmid: VmId,
        size: DesktopSize,
    },
    ResizeOutcome {
        vmid: VmId,
        outcome: ResizeProtocolOutcome,
    },
    ResizeTimedOut {
        vmid: VmId,
        size: DesktopSize,
    },
    Terminal {
        vmid: VmId,
        failure: Option<PublicError>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackLogEvent {
    OpenRequested {
        vmid: VmId,
        preferences: FallbackPreferences,
    },
    Opened {
        vmid: VmId,
    },
    OpenFailed {
        vmid: VmId,
        failure: PublicError,
    },
    Terminal {
        vmid: VmId,
        failure: Option<FallbackError>,
    },
}

/// Emits fallback lifecycle fields whose types cannot carry paths, tickets,
/// guest data, or inherited environment values.
pub fn emit_fallback_event(event: FallbackLogEvent) {
    match event {
        FallbackLogEvent::OpenRequested { vmid, preferences } => tracing::info!(
            event = "fallback_open_requested",
            vmid = vmid.get(),
            fullscreen = preferences.fullscreen,
            view_only = preferences.view_only,
            "TigerVNC fallback requested"
        ),
        FallbackLogEvent::Opened { vmid } => tracing::info!(
            event = "fallback_opened",
            vmid = vmid.get(),
            "TigerVNC fallback opened"
        ),
        FallbackLogEvent::OpenFailed { vmid, failure } => tracing::warn!(
            event = "fallback_open_failed",
            vmid = vmid.get(),
            error_category = ?failure.kind(),
            cleanup_failed = failure.has_cleanup_failure(),
            "TigerVNC fallback could not open"
        ),
        FallbackLogEvent::Terminal {
            vmid,
            failure: None,
        } => tracing::info!(
            event = "fallback_disconnected_cleanly",
            vmid = vmid.get(),
            "TigerVNC fallback disconnected"
        ),
        FallbackLogEvent::Terminal {
            vmid,
            failure: Some(failure),
        } => tracing::warn!(
            event = "fallback_terminal_error",
            vmid = vmid.get(),
            error_kind = ?failure.kind(),
            cleanup_failed = failure.has_cleanup_failure(),
            "TigerVNC fallback ended with a typed error"
        ),
    }
}

/// Emits only fields whose types cannot carry raw transport or guest data.
pub fn emit_session_event(event: SessionLogEvent) {
    match event {
        SessionLogEvent::OpenRequested {
            vmid,
            dynamic_resolution,
            view_only,
            clipboard_enabled,
        } => tracing::info!(
            event = "session_open_requested",
            vmid = vmid.get(),
            dynamic_resolution,
            view_only,
            clipboard_enabled,
            "native session requested"
        ),
        SessionLogEvent::PhaseChanged {
            vmid,
            from,
            to,
            duration,
        } => tracing::info!(
            event = "session_phase_changed",
            vmid = vmid.get(),
            from = ?from,
            to = ?to,
            duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            "native session phase changed"
        ),
        SessionLogEvent::GuestSize { vmid, size } => tracing::info!(
            event = "guest_size_observed",
            vmid = vmid.get(),
            width = size.width,
            height = size.height,
            "guest framebuffer size observed"
        ),
        SessionLogEvent::ResizeRequested { vmid, size } => tracing::info!(
            event = "resize_requested",
            vmid = vmid.get(),
            width = size.width,
            height = size.height,
            "dynamic resolution request queued"
        ),
        SessionLogEvent::ResizeOutcome { vmid, outcome } => tracing::info!(
            event = "resize_protocol_outcome",
            vmid = vmid.get(),
            outcome = ?outcome,
            "dynamic resolution protocol outcome observed"
        ),
        SessionLogEvent::ResizeTimedOut { vmid, size } => tracing::warn!(
            event = "resize_timed_out",
            vmid = vmid.get(),
            width = size.width,
            height = size.height,
            "dynamic resolution request timed out; the guest VirtIO resize helper may be missing or blocked"
        ),
        SessionLogEvent::Terminal {
            vmid,
            failure: None,
        } => tracing::info!(
            event = "session_disconnected_cleanly",
            vmid = vmid.get(),
            "native session disconnected"
        ),
        SessionLogEvent::Terminal {
            vmid,
            failure: Some(failure),
        } => {
            if let Some(detail) = failure.rfb_failure() {
                if let Some(io_kind) = detail.io_kind() {
                    tracing::warn!(
                        event = "session_terminal_error",
                        vmid = vmid.get(),
                        error_category = ?failure.kind(),
                        cleanup_failed = failure.has_cleanup_failure(),
                        rfb_phase = ?detail.phase(),
                        rfb_kind = ?detail.kind(),
                        io_kind = ?io_kind,
                        cleanup_io_kind = ?detail.cleanup_io_kind(),
                        "native session ended with a typed RFB error"
                    );
                } else {
                    tracing::warn!(
                        event = "session_terminal_error",
                        vmid = vmid.get(),
                        error_category = ?failure.kind(),
                        cleanup_failed = failure.has_cleanup_failure(),
                        rfb_phase = ?detail.phase(),
                        rfb_kind = ?detail.kind(),
                        cleanup_io_kind = ?detail.cleanup_io_kind(),
                        "native session ended with a typed RFB error"
                    );
                }
            } else {
                tracing::warn!(
                    event = "session_terminal_error",
                    vmid = vmid.get(),
                    error_category = ?failure.kind(),
                    cleanup_failed = failure.has_cleanup_failure(),
                    "native session ended with a typed error"
                );
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum LogError {
    #[error("could not determine the home directory")]
    MissingHomeDirectory,
    #[error("diagnostic log size limit must be nonzero")]
    InvalidSizeLimit,
    #[error("diagnostic log directory must be a private regular directory")]
    InsecureDirectory,
    #[error("diagnostic log must be a private regular file")]
    InsecureFile,
    #[error("diagnostic log I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub fn log_path(home: &Path) -> PathBuf {
    config_directory(home).join(LOG_FILE_NAME)
}

pub fn default_log_path() -> Result<PathBuf, LogError> {
    let home = env::var_os("HOME").ok_or(LogError::MissingHomeDirectory)?;
    Ok(log_path(Path::new(&home)))
}

pub struct PrivateRollingLog {
    path: PathBuf,
    backup_path: PathBuf,
    file: File,
    bytes_written: u64,
    max_bytes: u64,
}

impl PrivateRollingLog {
    pub fn open(path: &Path, max_bytes: u64) -> Result<Self, LogError> {
        if max_bytes == 0 {
            return Err(LogError::InvalidSizeLimit);
        }
        let directory = path.parent().ok_or_else(|| {
            LogError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic log path must have a parent",
            ))
        })?;
        ensure_private_directory(directory)?;
        let backup_path = directory.join(LOG_BACKUP_FILE_NAME);
        ensure_private_file_if_present(path)?;
        ensure_private_file_if_present(&backup_path)?;

        if fs::metadata(path).is_ok_and(|metadata| metadata.len() >= max_bytes) {
            rotate_path(path, &backup_path)?;
        }
        let file = open_private_append(path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            path: path.to_owned(),
            backup_path,
            file,
            bytes_written,
            max_bytes,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_data()?;
        rotate_path(&self.path, &self.backup_path).map_err(log_error_to_io)?;
        self.file = open_private_append(&self.path).map_err(log_error_to_io)?;
        self.bytes_written = 0;
        Ok(())
    }
}

impl Write for PrivateRollingLog {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let incoming = u64::try_from(buffer.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic record is too large",
            )
        })?;
        if incoming > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic record exceeds the log bound",
            ));
        }
        if self.bytes_written != 0
            && self
                .bytes_written
                .checked_add(incoming)
                .is_none_or(|total| total > self.max_bytes)
        {
            self.rotate()?;
        }
        let written = self.file.write(buffer)?;
        self.bytes_written = self
            .bytes_written
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), LogError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(LogError::InsecureDirectory);
            }
            #[cfg(unix)]
            if metadata.mode() & 0o777 != 0o700 {
                return Err(LogError::InsecureDirectory);
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_private_file_if_present(path: &Path) -> Result<(), LogError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(LogError::InsecureFile);
            }
            #[cfg(unix)]
            if metadata.mode() & 0o777 != 0o600 {
                return Err(LogError::InsecureFile);
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn open_private_append(path: &Path) -> Result<File, LogError> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn rotate_path(path: &Path, backup_path: &Path) -> Result<(), LogError> {
    if fs::symlink_metadata(backup_path).is_ok() {
        fs::remove_file(backup_path)?;
    }
    match fs::rename(path, backup_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn log_error_to_io(error: LogError) -> io::Error {
    match error {
        LogError::Io(error) => error,
        other => io::Error::new(io::ErrorKind::PermissionDenied, other),
    }
}

#[derive(Clone)]
struct TeeMakeWriter {
    log: Arc<Mutex<PrivateRollingLog>>,
}

impl TeeMakeWriter {
    fn new(log: PrivateRollingLog) -> Self {
        Self {
            log: Arc::new(Mutex::new(log)),
        }
    }
}

struct TeeWriter<'a> {
    stderr: io::Stderr,
    log: MutexGuard<'a, PrivateRollingLog>,
}

impl Write for TeeWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let log_result = self.log.write_all(buffer);
        let stderr_result = self.stderr.write_all(buffer);
        match (log_result, stderr_result) {
            (Ok(()), Ok(())) => Ok(buffer.len()),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let log_result = self.log.flush();
        let stderr_result = self.stderr.flush();
        log_result.and(stderr_result)
    }
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        Self::Writer {
            stderr: io::stderr(),
            log: self
                .log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }
}

pub fn emit_application_started(persistent_log: bool) {
    tracing::info!(
        event = "application_started",
        version = env!("CARGO_PKG_VERSION"),
        persistent_log,
        "RustedOutClient started"
    );
}

fn persistent_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::new("off,rustedoutclient::logging=info")
}

/// Installs a synchronous stderr subscriber and, when private storage can be
/// established, tees the same payload-free events into the rolling log.
pub fn initialize_tracing() -> bool {
    // Do not allow RUST_LOG or an unrelated crate module to enter the durable
    // stream. Only this module's typed, reviewed event surface is eligible.
    let persistent =
        default_log_path().and_then(|path| PrivateRollingLog::open(&path, DEFAULT_LOG_MAX_BYTES));
    match persistent {
        Ok(log) => tracing_subscriber::fmt()
            .with_env_filter(persistent_filter())
            .with_ansi(false)
            .with_writer(TeeMakeWriter::new(log))
            .try_init()
            .is_ok(),
        Err(_) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(persistent_filter())
                .with_ansi(false)
                .with_writer(io::stderr)
                .try_init();
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use tracing_subscriber::fmt::MakeWriter;

    use super::{emit_application_started, persistent_filter};

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuffer {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn persistent_filter_admits_only_the_typed_logging_module() {
        let output = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(output.clone())
            .with_env_filter(persistent_filter())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            emit_application_started(true);
            tracing::warn!(
                target: "rustedoutclient::future_unreviewed_module",
                raw_material = "SECRET FILTER SENTINEL",
                "unreviewed event"
            );
        });

        let rendered = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        assert!(rendered.contains("application_started"));
        assert!(!rendered.contains("SECRET FILTER SENTINEL"));
        assert!(!rendered.contains("future_unreviewed_module"));
    }
}
