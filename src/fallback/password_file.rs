use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use des::Des;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{runtime::RuntimeDir, ssh::ProxyTicket};

const TIGERVNC_PASSWORD_KEY: [u8; 8] = [0xE8, 0x4A, 0xD6, 0x60, 0xC4, 0x72, 0x1A, 0xE0];
const PASSWORD_BYTES: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PasswordFileStage {
    Create,
    Write,
    Sync,
    Remove,
}

#[derive(Error)]
#[error("temporary fallback password-file operation failed")]
pub(super) struct PasswordFileError {
    stage: PasswordFileStage,
    cleanup_failed: bool,
    owner: Option<VncPasswordFile>,
}

impl PasswordFileError {
    fn primary(stage: PasswordFileStage) -> Self {
        Self {
            stage,
            cleanup_failed: false,
            owner: None,
        }
    }

    #[cfg(test)]
    pub(super) fn stage(&self) -> PasswordFileStage {
        self.stage
    }

    pub(super) fn has_cleanup_failure(&self) -> bool {
        self.cleanup_failed
    }

    pub(super) fn retry_cleanup(&mut self) -> Result<(), ()> {
        let Some(owner) = self.owner.as_mut() else {
            return Ok(());
        };
        match owner.remove() {
            Ok(()) => {
                self.owner.take();
                Ok(())
            }
            Err(_) => {
                self.cleanup_failed = true;
                Err(())
            }
        }
    }

    #[cfg(test)]
    pub(super) fn retains_owner(&self) -> bool {
        self.owner.is_some()
    }
}

impl fmt::Debug for PasswordFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PasswordFileError")
            .field("stage", &self.stage)
            .field("cleanup_failed", &self.cleanup_failed)
            .field("retains_owner", &self.owner.is_some())
            .finish()
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PasswordFileFault {
    WriteAfter(usize),
    Sync,
}

pub(super) struct PasswordFilePolicy {
    #[cfg(test)]
    pub(super) fault: Option<PasswordFileFault>,
    #[cfg(test)]
    pub(super) remove_failures: usize,
    #[cfg(test)]
    pub(super) zeroized: Option<tokio::sync::oneshot::Sender<([u8; 8], [u8; 8])>>,
}

impl PasswordFilePolicy {
    pub(super) fn production() -> Self {
        Self {
            #[cfg(test)]
            fault: None,
            #[cfg(test)]
            remove_failures: 0,
            #[cfg(test)]
            zeroized: None,
        }
    }

    fn write_failure_after(&self) -> Option<usize> {
        #[cfg(test)]
        if let Some(PasswordFileFault::WriteAfter(written)) = self.fault {
            return Some(written);
        }
        None
    }

    fn fail_sync(&self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(PasswordFileFault::Sync)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn observe_zeroized(&mut self, _clear: &[u8; 8], _encrypted: &[u8]) {
        #[cfg(test)]
        if let Some(observer) = self.zeroized.take() {
            let mut encrypted_copy = [0_u8; 8];
            encrypted_copy.copy_from_slice(_encrypted);
            let _ = observer.send((*_clear, encrypted_copy));
        }
    }
}

pub(super) struct VncPasswordFile {
    path: Option<PathBuf>,
    #[cfg(test)]
    remove_failures: usize,
}

impl VncPasswordFile {
    pub(super) fn create(
        runtime: &RuntimeDir,
        ticket: ProxyTicket,
    ) -> Result<Self, PasswordFileError> {
        Self::create_with_policy(runtime, ticket, PasswordFilePolicy::production())
    }

    pub(super) fn create_with_policy(
        runtime: &RuntimeDir,
        ticket: ProxyTicket,
        mut policy: PasswordFilePolicy,
    ) -> Result<Self, PasswordFileError> {
        let mut clear = [0_u8; PASSWORD_BYTES];
        let secret = ticket.expose_for_auth().as_bytes();
        if secret.len() != PASSWORD_BYTES {
            clear.zeroize();
            drop(ticket);
            return Err(PasswordFileError::primary(PasswordFileStage::Write));
        }
        clear.copy_from_slice(secret);

        let cipher = Des::new(GenericArray::from_slice(&TIGERVNC_PASSWORD_KEY));
        let mut encrypted = GenericArray::clone_from_slice(&clear);
        cipher.encrypt_block(&mut encrypted);
        clear.zeroize();

        let (path, mut file) = match create_private_file(runtime.path()) {
            Ok(created) => created,
            Err(_) => {
                encrypted.as_mut_slice().zeroize();
                policy.observe_zeroized(&clear, encrypted.as_slice());
                drop(ticket);
                return Err(PasswordFileError::primary(PasswordFileStage::Create));
            }
        };
        let owner = Self {
            path: Some(path),
            #[cfg(test)]
            remove_failures: policy.remove_failures,
        };
        drop(ticket);

        #[cfg(unix)]
        if file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .is_err()
        {
            encrypted.as_mut_slice().zeroize();
            policy.observe_zeroized(&clear, encrypted.as_slice());
            drop(file);
            return Err(password_setup_error(PasswordFileStage::Create, owner));
        }

        let write_result = if let Some(written) = policy.write_failure_after() {
            file.write_all(&encrypted.as_slice()[..written.min(PASSWORD_BYTES)])
                .and_then(|()| {
                    Err(io::Error::other(
                        "synthetic fallback password write failure",
                    ))
                })
        } else {
            file.write_all(encrypted.as_slice())
        };
        let sync_result = if write_result.is_ok() {
            if policy.fail_sync() {
                Err(io::Error::other("synthetic fallback password sync failure"))
            } else {
                file.sync_data()
            }
        } else {
            Ok(())
        };
        encrypted.as_mut_slice().zeroize();
        policy.observe_zeroized(&clear, encrypted.as_slice());
        drop(file);

        if write_result.is_err() {
            return Err(password_setup_error(PasswordFileStage::Write, owner));
        }
        if sync_result.is_err() {
            return Err(password_setup_error(PasswordFileStage::Sync, owner));
        }
        Ok(owner)
    }

    pub(super) fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("password file is unavailable after removal")
    }

    pub(super) fn remove(&mut self) -> Result<(), PasswordFileError> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        #[cfg(test)]
        if self.remove_failures > 0 {
            self.remove_failures -= 1;
            return Err(PasswordFileError::primary(PasswordFileStage::Remove));
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
            Err(_) => Err(PasswordFileError::primary(PasswordFileStage::Remove)),
        }
    }

    #[cfg(test)]
    pub(super) fn fail_next_remove(&mut self) {
        self.remove_failures = self.remove_failures.max(1);
    }
}

fn password_setup_error(stage: PasswordFileStage, mut owner: VncPasswordFile) -> PasswordFileError {
    let cleanup_failed = owner.remove().is_err();
    PasswordFileError {
        stage,
        cleanup_failed,
        owner: cleanup_failed.then_some(owner),
    }
}

impl fmt::Debug for VncPasswordFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VncPasswordFile")
            .field("present", &self.path.is_some())
            .finish()
    }
}

impl Drop for VncPasswordFile {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

fn create_private_file(directory: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..16 {
        let path = directory.join(format!(".vnc-password-{}.bin", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique fallback password file",
    ))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::{PasswordFileFault, PasswordFilePolicy, PasswordFileStage, VncPasswordFile};
    use crate::{runtime::RuntimeDir, ssh::ProxyTicket};

    const TICKET: &str = "Ab12Cd34";
    const OBFUSCATED: [u8; 8] = [0xE6, 0x70, 0xFD, 0x73, 0xE2, 0x6D, 0xE5, 0xB6];

    fn password_files(runtime: &RuntimeDir) -> Vec<PathBuf> {
        fs::read_dir(runtime.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".vnc-password-"))
            })
            .collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deterministic_private_password_file_drops_ticket_and_removes_explicitly() {
        let runtime = RuntimeDir::create().unwrap();
        assert_eq!(
            fs::metadata(runtime.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let (ticket, mut dropped) = ProxyTicket::for_auth_test_with_drop_signal(TICKET);
        let mut file = VncPasswordFile::create(&runtime, ticket).unwrap();
        let path = file.path().to_owned();
        assert_eq!(fs::read(&path).unwrap(), OBFUSCATED);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!fs::read(&path)
            .unwrap()
            .windows(TICKET.len())
            .any(|window| window == TICKET.as_bytes()));
        assert!(dropped.try_recv().is_ok(), "ticket outlived password setup");

        let debug = format!("{file:?}");
        for secret in [TICKET, "e670fd73e26de5b6"] {
            assert!(!debug.contains(secret));
        }
        file.remove().unwrap();
        file.remove().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn drop_removes_password_file_and_remove_failure_is_typed_and_redacted() {
        let runtime = RuntimeDir::create().unwrap();
        let (ticket, _dropped) = ProxyTicket::for_auth_test_with_drop_signal(TICKET);
        let path: PathBuf;
        {
            let mut file = VncPasswordFile::create(&runtime, ticket).unwrap();
            path = file.path().to_owned();
            file.fail_next_remove();
            let error = file.remove().unwrap_err();
            assert_eq!(error.stage(), PasswordFileStage::Remove);
            let rendered = format!("{error:?} {error}");
            for secret in [TICKET, "e670fd73e26de5b6"] {
                assert!(!rendered.contains(secret));
            }
        }
        assert!(!path.exists(), "Drop did not retry owned file removal");
    }

    #[test]
    fn partial_and_exact_write_failures_remove_the_file_and_zeroize_both_blocks() {
        for written_before_failure in [3, 8] {
            let runtime = RuntimeDir::create().unwrap();
            let (ticket, mut ticket_dropped) = ProxyTicket::for_auth_test_with_drop_signal(TICKET);
            let (zeroized_tx, mut zeroized) = tokio::sync::oneshot::channel();
            let mut policy = PasswordFilePolicy::production();
            policy.fault = Some(PasswordFileFault::WriteAfter(written_before_failure));
            policy.zeroized = Some(zeroized_tx);

            let error = VncPasswordFile::create_with_policy(&runtime, ticket, policy).unwrap_err();

            assert_eq!(error.stage(), PasswordFileStage::Write);
            assert!(!error.has_cleanup_failure());
            assert!(password_files(&runtime).is_empty());
            assert!(ticket_dropped.try_recv().is_ok());
            let (clear, encrypted) = zeroized.try_recv().unwrap();
            assert_eq!(clear, [0; 8]);
            assert_eq!(encrypted, [0; 8]);
        }
    }

    #[test]
    fn sync_failure_retains_owner_after_first_remove_fault_and_drop_retries() {
        let runtime = RuntimeDir::create().unwrap();
        let (ticket, mut ticket_dropped) = ProxyTicket::for_auth_test_with_drop_signal(TICKET);
        let (zeroized_tx, mut zeroized) = tokio::sync::oneshot::channel();
        let mut policy = PasswordFilePolicy::production();
        policy.fault = Some(PasswordFileFault::Sync);
        policy.remove_failures = 1;
        policy.zeroized = Some(zeroized_tx);

        let error = VncPasswordFile::create_with_policy(&runtime, ticket, policy).unwrap_err();

        assert_eq!(error.stage(), PasswordFileStage::Sync);
        assert!(error.has_cleanup_failure());
        assert!(error.retains_owner());
        assert_eq!(password_files(&runtime).len(), 1);
        assert!(ticket_dropped.try_recv().is_ok());
        let (clear, encrypted) = zeroized.try_recv().unwrap();
        assert_eq!(clear, [0; 8]);
        assert_eq!(encrypted, [0; 8]);
        drop(error);
        assert!(password_files(&runtime).is_empty());
    }

    #[test]
    fn repeated_remove_fault_stays_typed_redacted_and_retains_cleanup_owner() {
        let runtime = RuntimeDir::create().unwrap();
        let (ticket, _ticket_dropped) = ProxyTicket::for_auth_test_with_drop_signal(TICKET);
        let mut policy = PasswordFilePolicy::production();
        policy.fault = Some(PasswordFileFault::WriteAfter(2));
        policy.remove_failures = 2;

        let mut error = VncPasswordFile::create_with_policy(&runtime, ticket, policy).unwrap_err();

        assert_eq!(error.stage(), PasswordFileStage::Write);
        assert!(error.has_cleanup_failure());
        assert!(error.retains_owner());
        assert!(error.retry_cleanup().is_err());
        assert!(error.retains_owner());
        let rendered = format!("{error:?} {error}");
        for forbidden in [
            TICKET,
            "e670fd73e26de5b6",
            runtime.path().to_string_lossy().as_ref(),
        ] {
            assert!(!rendered.contains(forbidden));
        }
        drop(error);
        assert!(password_files(&runtime).is_empty());
    }
}
