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
const CONTROL_OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
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

#[derive(Clone, Copy, Default)]
struct CleanupFaults {
    #[cfg(test)]
    fault: Option<CleanupFault>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupFault {
    Kill,
    Wait,
    Drain,
}

impl CleanupFaults {
    fn kill_fails(self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(CleanupFault::Kill)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn wait_fails(self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(CleanupFault::Wait)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn drain_fails(self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(CleanupFault::Drain)
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

#[cfg(test)]
impl From<CleanupFault> for CleanupFaults {
    fn from(fault: CleanupFault) -> Self {
        Self { fault: Some(fault) }
    }
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
        self.close_inner(CleanupFaults::default()).await
    }

    async fn close_inner(&mut self, cleanup_faults: CleanupFaults) -> Result<(), SshMasterError> {
        if self.child.is_none() {
            return Ok(());
        }

        let exit_result = run_control(self.factory.exit(&self.profile).unwrap()).await;
        let (terminated, cleanup_result) = stop_owned_child(
            self.child.as_mut().expect("child checked above"),
            CLOSE_TIMEOUT,
            cleanup_faults,
        )
        .await;
        let cleanup_result = self
            .apply_termination_outcome(terminated, cleanup_result, cleanup_faults)
            .await;

        compose_close_results(exit_result, cleanup_result)
    }

    async fn apply_termination_outcome(
        &mut self,
        terminated: bool,
        mut cleanup_result: Result<(), SshMasterError>,
        cleanup_faults: CleanupFaults,
    ) -> Result<(), SshMasterError> {
        if !terminated {
            return cleanup_result.and(Err(SshMasterError::CleanupFailed));
        }

        self.child.take();
        if finish_capture(self.stderr_task.take(), cleanup_faults)
            .await
            .is_err()
        {
            cleanup_result = Err(SshMasterError::CleanupFailed);
        }
        cleanup_result
    }

    #[cfg(test)]
    async fn close_with_cleanup_fault(
        &mut self,
        fault: CleanupFault,
    ) -> Result<(), SshMasterError> {
        self.close_inner(fault.into()).await
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
    run_control_inner(spec, CleanupFaults::default()).await
}

#[cfg(test)]
async fn run_control_with_cleanup_fault(
    spec: CommandSpec,
    fault: CleanupFault,
) -> Result<(), SshMasterError> {
    run_control_inner(spec, fault.into()).await
}

async fn run_control_inner(
    spec: CommandSpec,
    cleanup_faults: CleanupFaults,
) -> Result<(), SshMasterError> {
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
            let (_, cleanup_result) = terminate_owned_child(&mut child, cleanup_faults).await;
            abort_capture(stderr_task);
            return cleanup_result.and(Err(SshMasterError::CleanupFailed));
        }
        Err(_) => {
            let (_, cleanup_result) = terminate_owned_child(&mut child, cleanup_faults).await;
            let capture_result = finish_capture(stderr_task, cleanup_faults).await;
            if cleanup_result.is_err() || capture_result.is_err() {
                return Err(SshMasterError::CleanupFailed);
            }
            return Err(SshMasterError::ControlTimedOut);
        }
    };
    let stderr = finish_capture(stderr_task, cleanup_faults).await?;
    if status.success() {
        Ok(())
    } else {
        Err(classify_stderr(&stderr).into())
    }
}

async fn finish_capture(
    mut task: Option<JoinHandle<io::Result<Vec<u8>>>>,
    cleanup_faults: CleanupFaults,
) -> Result<Vec<u8>, SshMasterError> {
    let result = match task {
        Some(ref mut task) => match timeout(PIPE_DRAIN_TIMEOUT, &mut *task).await {
            Ok(Ok(Ok(captured))) => Ok(captured),
            Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => {
                task.abort();
                Err(SshMasterError::CleanupFailed)
            }
        },
        None => Ok(Vec::new()),
    };
    if cleanup_faults.drain_fails() {
        Err(SshMasterError::CleanupFailed)
    } else {
        result
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
    cleanup_faults: CleanupFaults,
) -> (bool, Result<(), SshMasterError>) {
    match timeout(graceful_timeout, child.wait()).await {
        Ok(Ok(_)) => (true, Ok(())),
        Ok(Err(_)) | Err(_) => terminate_owned_child(child, cleanup_faults).await,
    }
}

async fn terminate_owned_child(
    child: &mut Child,
    cleanup_faults: CleanupFaults,
) -> (bool, Result<(), SshMasterError>) {
    let kill_failed = child.start_kill().is_err() || cleanup_faults.kill_fails();
    let reaped = matches!(timeout(REAP_TIMEOUT, child.wait()).await, Ok(Ok(_)));
    let confirmed = reaped && !cleanup_faults.wait_fails();
    if confirmed && !kill_failed {
        (true, Ok(()))
    } else {
        (confirmed, Err(SshMasterError::CleanupFailed))
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
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use tempfile::{tempdir, TempDir};
    use tokio::time::{sleep, timeout};

    use super::{run_control_with_cleanup_fault, CleanupFault, SshMaster, SshMasterError};
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

    fn helper_pid(path: &Path) -> u32 {
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    fn assert_exact_pid_is_gone(pid: u32) {
        let status = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "synthetic helper PID {pid} is still alive"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn master_uses_private_short_runtime_path_and_exits_owned_child_once() {
        let _process_guard = crate::ssh::process_test_guard().await;
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
        let _process_guard = crate::ssh::process_test_guard().await;
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
        let _process_guard = crate::ssh::process_test_guard().await;
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

        let result = timeout(Duration::from_secs(5), master.check()).await;

        assert!(result.is_ok(), "check exceeded its owned-child deadline");
        assert!(result.unwrap().is_err());
        let check_pid = helper_pid(&runtime.control_socket().with_extension("check.pid"));
        assert_exact_pid_is_gone(check_pid);
        fs::remove_file(runtime.control_socket().with_extension("hang_check")).unwrap();
        master.close().await.unwrap();
        assert!(!master.is_running());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_exit_helper_is_bounded_before_master_cleanup_continues() {
        let _process_guard = crate::ssh::process_test_guard().await;
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

        let result = timeout(Duration::from_secs(8), master.close()).await;

        assert!(
            result.is_ok(),
            "close awaited the exit helper without a bound"
        );
        assert!(result.unwrap().is_err());
        let exit_pid = helper_pid(&runtime.control_socket().with_extension("exit.pid"));
        assert_exact_pid_is_gone(exit_pid);
        assert!(!master.is_running());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn control_cleanup_faults_run_owned_kill_wait_and_drain_orchestration() {
        let _process_guard = crate::ssh::process_test_guard().await;
        for fault in [CleanupFault::Kill, CleanupFault::Wait, CleanupFault::Drain] {
            let runtime = RuntimeDir::create().unwrap();
            let (_fixture_directory, executable) = fake_ssh();
            fs::write(
                runtime.control_socket().with_extension("hang_check"),
                b"synthetic fixture control\n",
            )
            .unwrap();
            let factory =
                SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

            let result =
                run_control_with_cleanup_fault(factory.check(&fixture_profile()).unwrap(), fault)
                    .await;

            assert!(matches!(result, Err(SshMasterError::CleanupFailed)));
            let pid = helper_pid(&runtime.control_socket().with_extension("check.pid"));
            assert_exact_pid_is_gone(pid);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn injected_unconfirmed_reap_retains_master_ownership_for_retry() {
        let _process_guard = crate::ssh::process_test_guard().await;
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
        let master_pid = helper_pid(&runtime.control_socket().with_extension("pid"));

        let result = master.close_with_cleanup_fault(CleanupFault::Wait).await;

        assert!(matches!(result, Err(SshMasterError::CleanupFailed)));
        assert_exact_pid_is_gone(master_pid);
        assert!(
            master.child.is_some(),
            "unconfirmed ownership was discarded"
        );
        master.close().await.unwrap();
        assert!(master.child.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_exit_and_master_cleanup_failure_are_composed_end_to_end() {
        let _process_guard = crate::ssh::process_test_guard().await;
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
        let master_pid = helper_pid(&runtime.control_socket().with_extension("pid"));

        let result = master.close_with_cleanup_fault(CleanupFault::Drain).await;

        assert!(matches!(
            result,
            Err(SshMasterError::CloseFailed {
                exit_request_failed: true,
                cleanup_failed: true,
            })
        ));
        let exit_pid = helper_pid(&runtime.control_socket().with_extension("exit.pid"));
        assert_exact_pid_is_gone(exit_pid);
        assert_exact_pid_is_gone(master_pid);
        assert!(master.child.is_none());
    }
}
