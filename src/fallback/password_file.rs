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
    Remove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("temporary fallback password-file operation failed")]
pub(super) struct PasswordFileError {
    stage: PasswordFileStage,
}

impl PasswordFileError {
    fn new(stage: PasswordFileStage) -> Self {
        Self { stage }
    }

    #[cfg(test)]
    pub(super) fn stage(self) -> PasswordFileStage {
        self.stage
    }
}

pub(super) struct VncPasswordFile {
    path: Option<PathBuf>,
    #[cfg(test)]
    fail_next_remove: bool,
}

impl VncPasswordFile {
    pub(super) fn create(
        runtime: &RuntimeDir,
        ticket: ProxyTicket,
    ) -> Result<Self, PasswordFileError> {
        let mut clear = [0_u8; PASSWORD_BYTES];
        let secret = ticket.expose_for_auth().as_bytes();
        if secret.len() != PASSWORD_BYTES {
            clear.zeroize();
            drop(ticket);
            return Err(PasswordFileError::new(PasswordFileStage::Write));
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
                drop(ticket);
                return Err(PasswordFileError::new(PasswordFileStage::Create));
            }
        };
        drop(ticket);
        let write_result = file
            .write_all(encrypted.as_slice())
            .and_then(|()| file.sync_data());
        encrypted.as_mut_slice().zeroize();
        drop(file);

        if write_result.is_err() {
            let _ = fs::remove_file(&path);
            return Err(PasswordFileError::new(PasswordFileStage::Write));
        }

        Ok(Self {
            path: Some(path),
            #[cfg(test)]
            fail_next_remove: false,
        })
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
        if std::mem::take(&mut self.fail_next_remove) {
            return Err(PasswordFileError::new(PasswordFileStage::Remove));
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
            Err(_) => Err(PasswordFileError::new(PasswordFileStage::Remove)),
        }
    }

    #[cfg(test)]
    pub(super) fn fail_next_remove(&mut self) {
        self.fail_next_remove = true;
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
            Ok(file) => {
                #[cfg(unix)]
                if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                return Ok((path, file));
            }
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

    use super::{PasswordFileStage, VncPasswordFile};
    use crate::{runtime::RuntimeDir, ssh::ProxyTicket};

    const TICKET: &str = "Ab12Cd34";
    const OBFUSCATED: [u8; 8] = [0xE6, 0x70, 0xFD, 0x73, 0xE2, 0x6D, 0xE5, 0xB6];

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
}
