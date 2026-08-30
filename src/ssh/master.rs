use std::{io, process::Stdio, time::Duration};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
    task::JoinHandle,
    time::timeout,
};

use crate::model::PveProfile;

use super::{classify_stderr, CommandSpec, SshCommandFactory, SshFailure};

const MAX_CAPTURED_STDERR_BYTES: usize = 65_536;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(not(test))]
const CONTROL_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(test)]
const CONTROL_OPERATION_TIMEOUT: Duration = Duration::from_millis(250);
const REAP_TIMEOUT: Duration = Duration::from_secs(1);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Error)]
pub enum SshMasterError {
    #[error("could not run the owned SSH process: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Ssh(#[from] SshFailure),
    #[error("SSH control operation timed out")]
    ControlTimedOut,
    #[error("owned SSH child cleanup failed")]
    CleanupFailed,
    #[error(
        "SSH master close failed (exit request failed: {exit_request_failed}, cleanup failed: {cleanup_failed})"
    )]
    CloseFailed {
        exit_request_failed: bool,
        cleanup_failed: bool,
    },
}

pub struct SshMaster {
    factory: SshCommandFactory,
    profile: PveProfile,
    child: Option<Child>,
    stderr_task: Option<JoinHandle<io::Result<Vec<u8>>>>,
}

impl SshMaster {
    pub async fn start(
        factory: SshCommandFactory,
        profile: PveProfile,
    ) -> Result<Self, SshMasterError> {
        let spec = factory.master(&profile).unwrap();
        let mut command = tokio::process::Command::from(spec.to_command());
        command.stdout(Stdio::null());
        command.kill_on_drop(true);
        let mut child = command.spawn()?;
        let stderr_task = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(capture_bounded(stderr, MAX_CAPTURED_STDERR_BYTES)));

        Ok(Self {
            factory,
            profile,
            child: Some(child),
            stderr_task,
        })
    }

    pub async fn check(&mut self) -> Result<(), SshMasterError> {
        run_control(self.factory.check(&self.profile).unwrap()).await
    }

    pub async fn close(&mut self) -> Result<(), SshMasterError> {
        if self.child.is_none() {
            return Ok(());
        }

        let exit_result = run_control(self.factory.exit(&self.profile).unwrap()).await;
        let (terminated, cleanup_result) = stop_owned_child(
            self.child.as_mut().expect("child checked above"),
            CLOSE_TIMEOUT,
        )
        .await;
        let cleanup_result = self
            .apply_termination_outcome(terminated, cleanup_result)
            .await;

        compose_close_results(exit_result, cleanup_result)
    }

    async fn apply_termination_outcome(
        &mut self,
        terminated: bool,
        mut cleanup_result: Result<(), SshMasterError>,
    ) -> Result<(), SshMasterError> {
        if !terminated {
            return cleanup_result.and(Err(SshMasterError::CleanupFailed));
        }

        self.child.take();
        if finish_capture(self.stderr_task.take()).await.is_err() {
            cleanup_result = Err(SshMasterError::CleanupFailed);
        }
        cleanup_result
    }

    pub fn is_running(&mut self) -> bool {
        let exited = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(None) | Err(_) => false,
                Ok(Some(_)) => true,
            },
            None => return false,
        };
        if exited {
            self.child.take();
            false
        } else {
            true
        }
    }
}

impl Drop for SshMaster {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(stderr_task) = self.stderr_task.take() {
            stderr_task.abort();
        }
    }
}

async fn run_control(spec: CommandSpec) -> Result<(), SshMasterError> {
    let mut command = tokio::process::Command::from(spec.to_command());
    command.stdout(Stdio::null());
    command.kill_on_drop(true);
    let mut child = command.spawn()?;
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(capture_bounded(stderr, MAX_CAPTURED_STDERR_BYTES)));
    let status = match timeout(CONTROL_OPERATION_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            let (_, cleanup_result) = terminate_owned_child(&mut child).await;
            abort_capture(stderr_task);
            return cleanup_result.and(Err(SshMasterError::CleanupFailed));
        }
        Err(_) => {
            let (_, cleanup_result) = terminate_owned_child(&mut child).await;
            let capture_result = finish_capture(stderr_task).await;
            if cleanup_result.is_err() || capture_result.is_err() {
                return Err(SshMasterError::CleanupFailed);
            }
            return Err(SshMasterError::ControlTimedOut);
        }
    };
    let stderr = finish_capture(stderr_task).await?;
    if status.success() {
        Ok(())
    } else {
        Err(classify_stderr(&stderr).into())
    }
}

async fn finish_capture(
    mut task: Option<JoinHandle<io::Result<Vec<u8>>>>,
) -> Result<Vec<u8>, SshMasterError> {
    match task {
        Some(ref mut task) => match timeout(PIPE_DRAIN_TIMEOUT, &mut *task).await {
            Ok(Ok(Ok(captured))) => Ok(captured),
            Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => {
                task.abort();
                Err(SshMasterError::CleanupFailed)
            }
        },
        None => Ok(Vec::new()),
    }
}

fn abort_capture(task: Option<JoinHandle<io::Result<Vec<u8>>>>) {
    if let Some(task) = task {
        task.abort();
    }
}

async fn stop_owned_child(
    child: &mut Child,
    graceful_timeout: Duration,
) -> (bool, Result<(), SshMasterError>) {
    match timeout(graceful_timeout, child.wait()).await {
        Ok(Ok(_)) => (true, Ok(())),
        Ok(Err(_)) | Err(_) => terminate_owned_child(child).await,
    }
}

async fn terminate_owned_child(child: &mut Child) -> (bool, Result<(), SshMasterError>) {
    let kill_failed = child.start_kill().is_err();
    match timeout(REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) if !kill_failed => (true, Ok(())),
        Ok(Ok(_)) => (true, Err(SshMasterError::CleanupFailed)),
        Ok(Err(_)) | Err(_) => (false, Err(SshMasterError::CleanupFailed)),
    }
}

fn compose_close_results(
    exit_result: Result<(), SshMasterError>,
    cleanup_result: Result<(), SshMasterError>,
) -> Result<(), SshMasterError> {
    match (exit_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(exit), Ok(())) => Err(exit),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(_), Err(_)) => Err(SshMasterError::CloseFailed {
            exit_request_failed: true,
            cleanup_failed: true,
        }),
    }
}

pub(super) async fn capture_bounded<R>(mut reader: R, limit: usize) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut captured = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(captured)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use tempfile::{tempdir, TempDir};
    use tokio::time::{sleep, timeout};

    use super::{compose_close_results, SshMaster, SshMasterError};
    use crate::{
        model::{NodeName, PveProfile, SshTarget},
        runtime::RuntimeDir,
        ssh::SshCommandFactory,
    };

    fn fixture_profile() -> PveProfile {
        PveProfile {
            name: "Synthetic Proxmox".to_owned(),
            ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
            node: NodeName::parse("pve2").unwrap(),
        }
    }

    #[cfg(unix)]
    fn fake_ssh() -> (TempDir, PathBuf) {
        let directory = tempdir().unwrap();
        let executable = directory.path().join("fake_ssh.sh");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_ssh.sh"),
            &executable,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs::metadata(&executable).unwrap().mode() & 0o777, 0o700);
        (directory, executable)
    }

    async fn wait_for(path: &Path) {
        timeout(Duration::from_secs(2), async {
            while !path.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake SSH did not create its state file");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn master_uses_private_short_runtime_path_and_exits_owned_child_once() {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        master.check().await.unwrap();
        master.close().await.unwrap();
        master.close().await.unwrap();

        assert!(!master.is_running());
        assert!(!runtime.control_socket().with_extension("state").exists());
        assert!(!runtime.control_socket().with_extension("pid").exists());

        let argv = fs::read_to_string(runtime.control_socket().with_extension("argv")).unwrap();
        assert_eq!(argv.lines().filter(|line| *line == "<invoke>").count(), 3);
        assert!(argv.contains("root@pve.example.invalid"));
        assert!(argv.contains("check"));
        assert!(argv.contains("exit"));
        assert!(!runtime.path().join("environment").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn close_waits_three_seconds_then_kills_only_its_owned_child() {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        fs::write(
            runtime.control_socket().with_extension("ignore_exit"),
            b"synthetic fixture control\n",
        )
        .unwrap();

        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let started = Instant::now();
        master.close().await.unwrap();
        let elapsed = started.elapsed();

        assert!(elapsed >= Duration::from_secs(3), "elapsed: {elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(4_500),
            "elapsed: {elapsed:?}"
        );
        assert!(!master.is_running());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_check_is_bounded_and_its_child_is_reaped() {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        fs::write(
            runtime.control_socket().with_extension("hang_check"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;

        let result = timeout(Duration::from_secs(2), master.check()).await;

        assert!(result.is_ok(), "check exceeded its owned-child deadline");
        assert!(result.unwrap().is_err());
        fs::remove_file(runtime.control_socket().with_extension("hang_check")).unwrap();
        master.close().await.unwrap();
        assert!(!master.is_running());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_exit_helper_is_bounded_before_master_cleanup_continues() {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        fs::write(
            runtime.control_socket().with_extension("hang_exit"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;

        let result = timeout(Duration::from_millis(5_500), master.close()).await;

        assert!(
            result.is_ok(),
            "close awaited the exit helper without a bound"
        );
        assert!(result.unwrap().is_err());
        assert!(!master.is_running());
    }

    #[test]
    fn close_error_composition_preserves_exit_and_cleanup_failures() {
        let result = compose_close_results(
            Err(SshMasterError::ControlTimedOut),
            Err(SshMasterError::CleanupFailed),
        );

        assert!(matches!(
            result,
            Err(SshMasterError::CloseFailed {
                exit_request_failed: true,
                cleanup_failed: true,
            })
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unconfirmed_cleanup_outcome_retains_master_for_retry() {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;

        let result = master
            .apply_termination_outcome(false, Err(SshMasterError::CleanupFailed))
            .await;

        assert!(matches!(result, Err(SshMasterError::CleanupFailed)));
        assert!(master.is_running());
        master.close().await.unwrap();
        assert!(!master.is_running());
    }
}
