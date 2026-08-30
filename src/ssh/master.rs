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

#[derive(Debug, Error)]
pub enum SshMasterError {
    #[error("could not run the owned SSH process: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Ssh(#[from] SshFailure),
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
        let mut child = self.child.take().expect("child checked above");
        let wait_result = match timeout(CLOSE_TIMEOUT, child.wait()).await {
            Ok(result) => result.map(|_| ()),
            Err(_) => {
                let kill_result = child.start_kill();
                let wait_result = child.wait().await.map(|_| ());
                kill_result.and(wait_result)
            }
        };
        let stderr_result = join_capture(self.stderr_task.take()).await;

        exit_result?;
        wait_result?;
        stderr_result?;
        Ok(())
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
    let status = child.wait().await?;
    let stderr = join_capture(stderr_task).await?;
    if status.success() {
        Ok(())
    } else {
        Err(classify_stderr(&stderr).into())
    }
}

async fn join_capture(task: Option<JoinHandle<io::Result<Vec<u8>>>>) -> Result<Vec<u8>, io::Error> {
    match task {
        Some(task) => task
            .await
            .map_err(|error| io::Error::other(format!("SSH capture task failed: {error}")))?,
        None => Ok(Vec::new()),
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

    use super::SshMaster;
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
}
