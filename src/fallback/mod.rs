mod password_file;
mod relay;
mod viewer;

use std::{
    ffi::OsString,
    fmt,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    process::Child,
    sync::oneshot,
    task::JoinHandle,
    time::{timeout_at, Instant},
};

use crate::{
    runtime::RuntimeDir,
    ssh::{ProxyTicket, TrustedSshProxy},
};

#[cfg(test)]
use password_file::PasswordFilePolicy;
use password_file::VncPasswordFile;
use relay::RelayPolicy;
use viewer::{ViewerSnapshot, ViewerSnapshotError, ViewerSnapshotPolicy};

const FALLBACK_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(test)]
static ACTIVE_OWNER_TASKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FallbackPreferences {
    pub fullscreen: bool,
    pub view_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackErrorKind {
    ViewerPath,
    ViewerSnapshot,
    PasswordFile,
    Listener,
    ViewerSpawn,
    ViewerExitedBeforeConnect,
    ViewerExited,
    Accept,
    AcceptTimedOut,
    PeerRejected,
    Relay,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("fallback viewer operation failed")]
pub struct FallbackError {
    kind: FallbackErrorKind,
    cleanup_failed: bool,
}

impl FallbackError {
    pub fn kind(self) -> FallbackErrorKind {
        self.kind
    }

    pub fn has_cleanup_failure(self) -> bool {
        self.cleanup_failed
    }

    pub(crate) fn new(kind: FallbackErrorKind) -> Self {
        Self {
            kind,
            cleanup_failed: false,
        }
    }

    pub(crate) fn with_cleanup_failure(mut self) -> Self {
        self.cleanup_failed = true;
        self
    }
}

pub struct TigerVncFallback;

impl TigerVncFallback {
    pub async fn open(
        proxy: TrustedSshProxy,
        runtime: &RuntimeDir,
        viewer_path: &Path,
        preferences: FallbackPreferences,
    ) -> Result<FallbackSession, FallbackError> {
        let (stream, ticket) = proxy.into_parts();
        open_with_parts(
            stream,
            ticket,
            runtime,
            viewer_path,
            preferences,
            OpenPolicy::production(),
        )
        .await
    }
}

pub struct FallbackSession {
    cancel: Option<oneshot::Sender<Instant>>,
    task: Option<JoinHandle<()>>,
    terminal: Arc<Mutex<Option<Result<(), FallbackError>>>>,
    result: Option<Result<(), FallbackError>>,
    close_timeout: Duration,
}

impl FallbackSession {
    pub fn try_complete(&mut self) -> Option<Result<(), FallbackError>> {
        if let Some(result) = self.result {
            return Some(result);
        }
        if !self.task.as_ref().is_some_and(JoinHandle::is_finished) {
            return None;
        }
        self.task.take();
        let result = self
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unwrap_or_else(|| Err(FallbackError::new(FallbackErrorKind::Cleanup)));
        self.cancel.take();
        self.result = Some(result);
        Some(result)
    }

    pub async fn close(&mut self) -> Result<(), FallbackError> {
        if let Some(result) = self.result {
            return result;
        }
        let deadline = Instant::now() + self.close_timeout;
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(deadline);
        }
        let join_failed = if let Some(mut task) = self.task.take() {
            match timeout_at(deadline, &mut task).await {
                Ok(result) => result.is_err(),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    true
                }
            }
        } else {
            false
        };
        let mut result = self
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unwrap_or_else(|| Err(FallbackError::new(FallbackErrorKind::Cleanup)));
        if join_failed {
            result = match result {
                Ok(()) => Err(FallbackError::new(FallbackErrorKind::Cleanup)),
                Err(error) => Err(error.with_cleanup_failure()),
            };
        }
        self.result = Some(result);
        result
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(
        closed: Arc<std::sync::atomic::AtomicBool>,
    ) -> (Self, TestFallbackCompletion) {
        let (complete, completed) = oneshot::channel();
        let (cancel, cancelled) = oneshot::channel();
        let terminal = Arc::new(Mutex::new(None));
        let task_terminal = Arc::clone(&terminal);
        let task = tokio::spawn(async move {
            let result = tokio::select! {
                result = completed => result.unwrap_or_else(|_| {
                    Err(FallbackError::new(FallbackErrorKind::Cleanup))
                }),
                _ = cancelled => Ok(()),
            };
            closed.store(true, std::sync::atomic::Ordering::SeqCst);
            *task_terminal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
        });
        (
            Self {
                cancel: Some(cancel),
                task: Some(task),
                terminal,
                result: None,
                close_timeout: Duration::from_secs(1),
            },
            TestFallbackCompletion(Some(complete)),
        )
    }
}

#[cfg(test)]
pub(crate) struct TestFallbackCompletion(Option<oneshot::Sender<Result<(), FallbackError>>>);

#[cfg(test)]
impl TestFallbackCompletion {
    pub(crate) fn finish(mut self, result: Result<(), FallbackError>) {
        if let Some(complete) = self.0.take() {
            let _ = complete.send(result);
        }
    }
}

impl fmt::Debug for FallbackSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FallbackSession")
            .field("finished", &self.result.is_some())
            .finish()
    }
}

impl Drop for FallbackSession {
    fn drop(&mut self) {
        if self.result.is_none() {
            if let Some(cancel) = self.cancel.take() {
                let _ = cancel.send(Instant::now() + self.close_timeout);
            }
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }
}

struct ViewerEnvironment {
    home: Option<OsString>,
    tmpdir: Option<OsString>,
    lang: Option<OsString>,
    lc_all: Option<OsString>,
    lc_ctype: Option<OsString>,
    #[cfg(test)]
    inherited_seed: Vec<(OsString, OsString)>,
}

impl ViewerEnvironment {
    fn from_parent() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            tmpdir: std::env::var_os("TMPDIR"),
            lang: std::env::var_os("LANG"),
            lc_all: std::env::var_os("LC_ALL"),
            lc_ctype: std::env::var_os("LC_CTYPE"),
            #[cfg(test)]
            inherited_seed: Vec::new(),
        }
    }

    #[cfg(test)]
    fn for_test(allowed: [Option<&str>; 5], inherited_seed: &[(&str, &str)]) -> Self {
        Self {
            home: allowed[0].map(Into::into),
            tmpdir: allowed[1].map(Into::into),
            lang: allowed[2].map(Into::into),
            lc_all: allowed[3].map(Into::into),
            lc_ctype: allowed[4].map(Into::into),
            inherited_seed: inherited_seed
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect(),
        }
    }

    fn apply(self, command: &mut tokio::process::Command) {
        #[cfg(test)]
        for (name, value) in self.inherited_seed {
            command.env(name, value);
        }
        command.env_clear().env("PATH", "/usr/bin:/bin");
        for (name, value) in [
            ("HOME", self.home),
            ("TMPDIR", self.tmpdir),
            ("LANG", self.lang),
            ("LC_ALL", self.lc_all),
            ("LC_CTYPE", self.lc_ctype),
        ] {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
    }
}

struct OpenPolicy {
    relay: RelayPolicy,
    viewer_snapshot: ViewerSnapshotPolicy,
    viewer_environment: ViewerEnvironment,
    #[cfg(test)]
    password_file: PasswordFilePolicy,
    close_timeout: Duration,
    #[cfg(test)]
    bound: Option<oneshot::Sender<std::net::SocketAddr>>,
    #[cfg(test)]
    viewer_pid: Option<oneshot::Sender<u32>>,
    #[cfg(test)]
    fail_password_remove: bool,
}

impl OpenPolicy {
    fn production() -> Self {
        Self {
            relay: RelayPolicy::production(FALLBACK_CLOSE_TIMEOUT),
            viewer_snapshot: ViewerSnapshotPolicy::production(),
            viewer_environment: ViewerEnvironment::from_parent(),
            #[cfg(test)]
            password_file: PasswordFilePolicy::production(),
            close_timeout: FALLBACK_CLOSE_TIMEOUT,
            #[cfg(test)]
            bound: None,
            #[cfg(test)]
            viewer_pid: None,
            #[cfg(test)]
            fail_password_remove: false,
        }
    }
}

async fn open_with_parts<S>(
    mut proxy: S,
    ticket: ProxyTicket,
    runtime: &RuntimeDir,
    viewer_path: &Path,
    preferences: FallbackPreferences,
    #[allow(unused_mut)] mut policy: OpenPolicy,
) -> Result<FallbackSession, FallbackError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut viewer_snapshot =
        match ViewerSnapshot::create(runtime, viewer_path, policy.viewer_snapshot) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                drop(ticket);
                let primary = fallback_snapshot_error(&error);
                drop(error);
                let deadline = Instant::now() + policy.close_timeout;
                return Err(with_open_cleanup(
                    primary,
                    relay::close_proxy(&mut proxy, deadline).await,
                ));
            }
        };

    #[cfg(test)]
    let password_file_result =
        VncPasswordFile::create_with_policy(runtime, ticket, policy.password_file);
    #[cfg(not(test))]
    let password_file_result = VncPasswordFile::create(runtime, ticket);
    let mut password_file = match password_file_result {
        Ok(file) => file,
        Err(mut password_error) => {
            let deadline = Instant::now() + policy.close_timeout;
            let cleanup_failed_before_retry = password_error.has_cleanup_failure();
            let retry_cleanup = password_error.retry_cleanup();
            let password_cleanup = if cleanup_failed_before_retry || retry_cleanup.is_err() {
                Err(())
            } else {
                Ok(())
            };
            drop(password_error);
            let snapshot_cleanup = viewer_snapshot.remove();
            let proxy_cleanup = relay::close_proxy(&mut proxy, deadline).await;
            return Err(with_three_open_cleanups(
                FallbackError::new(FallbackErrorKind::PasswordFile),
                password_cleanup,
                snapshot_cleanup,
                proxy_cleanup,
            ));
        }
    };
    #[cfg(test)]
    if policy.fail_password_remove {
        password_file.fail_next_remove();
    }

    let listener = match relay::bind_loopback().await {
        Ok(listener) => listener,
        Err(_) => {
            let deadline = Instant::now() + policy.close_timeout;
            let password_cleanup = password_file.remove().map_err(|_| ());
            let snapshot_cleanup = viewer_snapshot.remove();
            let proxy_cleanup = relay::close_proxy(&mut proxy, deadline).await;
            return Err(with_three_open_cleanups(
                FallbackError::new(FallbackErrorKind::Listener),
                password_cleanup,
                snapshot_cleanup,
                proxy_cleanup,
            ));
        }
    };
    let address = match listener.local_addr() {
        Ok(address) => address,
        Err(_) => {
            drop(listener);
            let deadline = Instant::now() + policy.close_timeout;
            let password_cleanup = password_file.remove().map_err(|_| ());
            let snapshot_cleanup = viewer_snapshot.remove();
            let proxy_cleanup = relay::close_proxy(&mut proxy, deadline).await;
            return Err(with_three_open_cleanups(
                FallbackError::new(FallbackErrorKind::Listener),
                password_cleanup,
                snapshot_cleanup,
                proxy_cleanup,
            ));
        }
    };
    let endpoint = format!("127.0.0.1::{}", address.port());
    let viewer = spawn_viewer(
        viewer_snapshot,
        password_file.path(),
        &endpoint,
        preferences,
        policy.viewer_environment,
    );
    let viewer = match viewer {
        Ok(viewer) => viewer,
        Err(mut viewer_snapshot) => {
            drop(listener);
            let deadline = Instant::now() + policy.close_timeout;
            let password_cleanup = password_file.remove().map_err(|_| ());
            let snapshot_cleanup = viewer_snapshot.remove();
            let proxy_cleanup = relay::close_proxy(&mut proxy, deadline).await;
            return Err(with_three_open_cleanups(
                FallbackError::new(FallbackErrorKind::ViewerSpawn),
                password_cleanup,
                snapshot_cleanup,
                proxy_cleanup,
            ));
        }
    };

    #[cfg(test)]
    if let Some(bound) = policy.bound.take() {
        let _ = bound.send(address);
    }
    #[cfg(test)]
    if let Some(viewer_pid) = policy.viewer_pid.take() {
        if let Some(pid) = viewer.child.id() {
            let _ = viewer_pid.send(pid);
        }
    }

    let (cancel, cancelled) = oneshot::channel();
    let close_timeout = policy.close_timeout;
    let terminal = Arc::new(Mutex::new(None));
    let task_terminal = Arc::clone(&terminal);
    let task = tokio::spawn(async move {
        #[cfg(test)]
        let _owner_guard = OwnerTaskGuard::enter();
        let result = relay::run(
            listener,
            viewer,
            proxy,
            password_file,
            cancelled,
            policy.relay,
        )
        .await;
        *task_terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
    });
    Ok(FallbackSession {
        cancel: Some(cancel),
        task: Some(task),
        terminal,
        result: None,
        close_timeout,
    })
}

#[cfg(test)]
fn validate_viewer_path(viewer_path: &Path) -> Result<(), FallbackError> {
    viewer::validate_viewer_path(viewer_path).map_err(|error| fallback_snapshot_error(&error))
}

struct OwnedViewer {
    // Field order is intentional: kill-on-drop is invoked before the snapshot
    // cleanup guard removes the interpreter-visible executable path.
    child: Child,
    snapshot: ViewerSnapshot,
}

fn spawn_viewer(
    snapshot: ViewerSnapshot,
    password_path: &Path,
    endpoint: &str,
    preferences: FallbackPreferences,
    environment: ViewerEnvironment,
) -> Result<OwnedViewer, ViewerSnapshot> {
    let mut command = tokio::process::Command::new(snapshot.path());
    command
        .args(viewer_arguments(password_path, endpoint, preferences))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    environment.apply(&mut command);
    match command.spawn() {
        Ok(child) => Ok(OwnedViewer { child, snapshot }),
        Err(_) => Err(snapshot),
    }
}

fn viewer_arguments(
    password_path: &Path,
    endpoint: &str,
    preferences: FallbackPreferences,
) -> Vec<std::ffi::OsString> {
    let mut arguments = vec![
        "-Shared=1".into(),
        "-RemoteResize=1".into(),
        "-SecurityTypes=VncAuth".into(),
        "-PasswordFile".into(),
        password_path.as_os_str().to_owned(),
    ];
    if preferences.fullscreen {
        arguments.push("-FullScreen=1".into());
    }
    if preferences.view_only {
        arguments.push("-ViewOnly=1".into());
    }
    arguments.push(endpoint.into());
    arguments
}

fn with_open_cleanup(error: FallbackError, cleanup: Result<(), ()>) -> FallbackError {
    if cleanup.is_err() {
        error.with_cleanup_failure()
    } else {
        error
    }
}

fn fallback_snapshot_error(snapshot_error: &ViewerSnapshotError) -> FallbackError {
    let kind = if snapshot_error.is_source() {
        FallbackErrorKind::ViewerPath
    } else {
        FallbackErrorKind::ViewerSnapshot
    };
    let error = FallbackError::new(kind);
    if snapshot_error.has_cleanup_failure() {
        error.with_cleanup_failure()
    } else {
        error
    }
}

fn with_three_open_cleanups(
    error: FallbackError,
    first: Result<(), ()>,
    second: Result<(), ()>,
    third: Result<(), ()>,
) -> FallbackError {
    if first.is_err() || second.is_err() || third.is_err() {
        error.with_cleanup_failure()
    } else {
        error
    }
}

#[cfg(test)]
struct OwnerTaskGuard;

#[cfg(test)]
impl OwnerTaskGuard {
    fn enter() -> Self {
        ACTIVE_OWNER_TASKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for OwnerTaskGuard {
    fn drop(&mut self) {
        ACTIVE_OWNER_TASKS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
fn active_owner_tasks() -> usize {
    ACTIVE_OWNER_TASKS.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        pin::Pin,
        process::{Command, Stdio},
        task::{Context, Poll},
        time::Duration,
    };

    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};

    use tempfile::{tempdir, TempDir};
    use tokio::{
        io::{duplex, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
        net::TcpStream,
        process::Command as TokioCommand,
        sync::oneshot,
        time::{sleep, timeout},
    };

    use super::{
        active_owner_tasks, open_with_parts, password_file::PasswordFileFault,
        validate_viewer_path, viewer::ViewerSnapshotFault, viewer_arguments, FallbackErrorKind,
        FallbackPreferences, FallbackSession, OpenPolicy, RelayPolicy,
    };
    use crate::{
        model::{NodeName, PveProfile, SshTarget, VmId},
        runtime::RuntimeDir,
        ssh::{ProxyTicket, SshCommandFactory, SshMaster, TrustedSshProxy},
    };

    const TICKET: &str = "Ab12Cd34";
    const CIPHERTEXT_HEX: &str = "e670fd73e26de5b6";

    enum ViewerBehavior {
        Hang,
        ExitImmediately,
        ExitOnMarker,
        SpawnFailure,
    }

    struct ViewerFixture {
        _directory: TempDir,
        path: PathBuf,
    }

    impl ViewerFixture {
        #[cfg(unix)]
        fn new(behavior: ViewerBehavior) -> Self {
            let directory = tempdir().unwrap();
            let path = directory.path().join("synthetic-viewer");
            let template = match behavior {
                ViewerBehavior::Hang => {
                    r#"#!/bin/sh
printf '%s\n' "$@" > @ARGV@
if [ "${LC_PVE_TICKET+x}" = x ]; then printf 'present\n'; else printf 'absent\n'; fi > @TICKET_ENV@
printf '%s\n' "$$" > @PID@
while :; do sleep 1; done
"#
                }
                ViewerBehavior::ExitImmediately => {
                    r#"#!/bin/sh
printf '%s\n' "$@" > @ARGV@
if [ "${LC_PVE_TICKET+x}" = x ]; then printf 'present\n'; else printf 'absent\n'; fi > @TICKET_ENV@
printf '%s\n' "$$" > @PID@
exit 0
"#
                }
                ViewerBehavior::ExitOnMarker => {
                    r#"#!/bin/sh
printf '%s\n' "$@" > @ARGV@
if [ "${LC_PVE_TICKET+x}" = x ]; then printf 'present\n'; else printf 'absent\n'; fi > @TICKET_ENV@
printf '%s\n' "$$" > @PID@
while [ ! -f @EXIT@ ]; do sleep 1; done
exit 0
"#
                }
                ViewerBehavior::SpawnFailure => "#!/synthetic/missing-interpreter\n",
            };
            let body = template
                .replace("@ARGV@", &shell_quote(&Self::artifact_path(&path, ".argv")))
                .replace(
                    "@TICKET_ENV@",
                    &shell_quote(&Self::artifact_path(&path, ".ticket-env")),
                )
                .replace("@PID@", &shell_quote(&Self::artifact_path(&path, ".pid")))
                .replace("@EXIT@", &shell_quote(&Self::artifact_path(&path, ".exit")));
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                _directory: directory,
                path,
            }
        }

        fn artifact(&self, suffix: &str) -> PathBuf {
            Self::artifact_path(&self.path, suffix)
        }

        fn artifact_path(path: &Path, suffix: &str) -> PathBuf {
            PathBuf::from(format!("{}{suffix}", path.display()))
        }
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
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
        (directory, executable)
    }

    fn fixture_profile() -> PveProfile {
        PveProfile {
            name: "Synthetic Proxmox".to_owned(),
            ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
            node: NodeName::parse("pve2").unwrap(),
        }
    }

    struct ParentTicketEnvironment(Option<std::ffi::OsString>);

    impl ParentTicketEnvironment {
        fn install() -> Self {
            let previous = std::env::var_os("LC_PVE_TICKET");
            std::env::set_var("LC_PVE_TICKET", "SYNTHETIC_PARENT_SENTINEL");
            Self(previous)
        }
    }

    impl Drop for ParentTicketEnvironment {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                std::env::set_var("LC_PVE_TICKET", previous);
            } else {
                std::env::remove_var("LC_PVE_TICKET");
            }
        }
    }

    struct TestChannels {
        bound: oneshot::Receiver<std::net::SocketAddr>,
        accepted: oneshot::Receiver<()>,
        pid: oneshot::Receiver<u32>,
    }

    fn test_policy(
        accept_timeout: Duration,
        close_timeout: Duration,
    ) -> (OpenPolicy, TestChannels) {
        let (bound_tx, bound) = oneshot::channel();
        let (accepted_tx, accepted) = oneshot::channel();
        let (pid_tx, pid) = oneshot::channel();
        (
            OpenPolicy {
                relay: RelayPolicy {
                    accept_timeout,
                    close_timeout,
                    accepted: Some(accepted_tx),
                },
                viewer_snapshot: super::viewer::ViewerSnapshotPolicy::production(),
                viewer_environment: super::ViewerEnvironment::from_parent(),
                password_file: super::password_file::PasswordFilePolicy::production(),
                close_timeout,
                bound: Some(bound_tx),
                viewer_pid: Some(pid_tx),
                fail_password_remove: false,
            },
            TestChannels {
                bound,
                accepted,
                pid,
            },
        )
    }

    async fn open_test<S>(
        runtime: &RuntimeDir,
        viewer: &Path,
        proxy: S,
        preferences: FallbackPreferences,
        policy: OpenPolicy,
    ) -> Result<FallbackSession, super::FallbackError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (ticket, _dropped) = ProxyTicket::for_auth_test_with_drop_signal(TICKET);
        open_with_parts(proxy, ticket, runtime, viewer, preferences, policy).await
    }

    async fn wait_for(path: &Path) {
        timeout(Duration::from_secs(5), async {
            while !path.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synthetic viewer artifact was not created");
    }

    async fn wait_for_nonempty(path: &Path) {
        timeout(Duration::from_secs(5), async {
            while !fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synthetic viewer artifact remained empty");
    }

    async fn completed(session: &mut FallbackSession) -> Result<(), super::FallbackError> {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(result) = session.try_complete() {
                    break result;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fallback owner did not complete")
    }

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

    fn executable_snapshots(runtime: &RuntimeDir) -> Vec<PathBuf> {
        fs::read_dir(runtime.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".vnc-viewer-"))
            })
            .collect()
    }

    async fn assert_password_files_are_gone(runtime: &RuntimeDir) {
        timeout(Duration::from_secs(5), async {
            while !password_files(runtime).is_empty() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fallback password artifact remained present");
    }

    async fn assert_executable_snapshots_are_gone(runtime: &RuntimeDir) {
        timeout(Duration::from_secs(5), async {
            while !executable_snapshots(runtime).is_empty() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fallback executable snapshot remained present");
    }

    fn exact_pid_is_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    async fn assert_exact_pid_is_gone(pid: u32) {
        timeout(Duration::from_secs(5), async {
            while exact_pid_is_alive(pid) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("exact synthetic viewer PID remained alive");
    }

    async fn assert_owner_is_gone() {
        timeout(Duration::from_secs(5), async {
            while active_owner_tasks() != 0 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fallback owner task remained alive");
    }

    async fn assert_owner_started() {
        timeout(Duration::from_secs(5), async {
            while active_owner_tasks() == 0 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fallback owner task never started");
    }

    async fn assert_port_is_closed(address: std::net::SocketAddr) {
        timeout(Duration::from_secs(5), async {
            while TcpStream::connect(address).await.is_ok() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("exact fallback listener port remained open");
    }

    fn helper_pid(path: &Path) -> u32 {
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn viewer_validation_follows_absolute_symlink_and_rejects_unsafe_targets() {
        let directory = tempdir().unwrap();
        let executable = directory.path().join("viewer-real");
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let link = directory.path().join("viewer-link");
        symlink(&executable, &link).unwrap();
        assert!(validate_viewer_path(&link).is_ok());

        let relative = Path::new("relative-viewer");
        let missing = directory.path().join("missing");
        let broken = directory.path().join("broken");
        symlink(&missing, &broken).unwrap();
        let non_executable = directory.path().join("not-executable");
        fs::write(&non_executable, b"fixture").unwrap();
        fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o600)).unwrap();
        let oversized = directory.path().join("oversized-executable");
        let oversized_file = fs::File::create(&oversized).unwrap();
        oversized_file.set_len(64 * 1024 * 1024 + 1).unwrap();
        drop(oversized_file);
        fs::set_permissions(&oversized, fs::Permissions::from_mode(0o700)).unwrap();
        for rejected in [
            relative,
            missing.as_path(),
            broken.as_path(),
            directory.path(),
            non_executable.as_path(),
            oversized.as_path(),
        ] {
            assert_eq!(
                validate_viewer_path(rejected).unwrap_err().kind(),
                FallbackErrorKind::ViewerPath
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opened_symlink_bytes_are_pinned_in_a_private_executable_snapshot() {
        let _guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let directory = tempdir().unwrap();
        let original = directory.path().join("viewer-original");
        let replacement = directory.path().join("viewer-replacement");
        let configured = directory.path().join("viewer-link");
        let marker = directory.path().join("executed-marker");
        let marker_path = shell_quote(&marker);
        write_executable(
            &original,
            &format!(
                "#!/bin/sh\nprintf 'opened-descriptor\\n' > {marker_path}\nwhile :; do sleep 1; done\n"
            ),
        );
        write_executable(
            &replacement,
            &format!(
                "#!/bin/sh\nprintf 'replacement-path\\n' > {marker_path}\nwhile :; do sleep 1; done\n"
            ),
        );
        symlink(&original, &configured).unwrap();

        let (proxy, _peer) = duplex(64);
        let (mut policy, channels) = test_policy(Duration::from_secs(5), Duration::from_secs(1));
        let hook_path = configured.clone();
        let hook_replacement = replacement.clone();
        policy.viewer_snapshot.after_open = Some(Box::new(move || {
            fs::remove_file(&hook_path).unwrap();
            symlink(&hook_replacement, &hook_path).unwrap();
        }));
        let mut session = open_test(
            &runtime,
            &configured,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let _address = channels.bound.await.unwrap();
        let pid = channels.pid.await.unwrap();
        wait_for_nonempty(&marker).await;

        assert_eq!(fs::read_to_string(&marker).unwrap(), "opened-descriptor\n");
        assert_eq!(fs::read_link(&configured).unwrap(), replacement);
        let snapshots = executable_snapshots(&runtime);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            fs::metadata(&snapshots[0]).unwrap().permissions().mode() & 0o777,
            0o500
        );

        session.close().await.unwrap();
        assert_exact_pid_is_gone(pid).await;
        assert_executable_snapshots_are_gone(&runtime).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn viewer_child_receives_only_the_fixed_minimal_environment() {
        let _guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let directory = tempdir().unwrap();
        let viewer = directory.path().join("environment-viewer");
        let capture = directory.path().join("environment-capture");
        let template = r#"#!/bin/sh
printf '%s\n' "$PATH" "$HOME" "$TMPDIR" "$LANG" "$LC_ALL" "$LC_CTYPE" "${ROC_SYNTHETIC_SECRET-absent}" "${DYLD_INSERT_LIBRARIES-absent}" "${LD_PRELOAD-absent}" "${SSH_AUTH_SOCK-absent}" "${SSH_AGENT_PID-absent}" "${LC_PVE_TICKET-absent}" > @CAPTURE@
while :; do sleep 1; done
"#;
        write_executable(
            &viewer,
            &template.replace("@CAPTURE@", &shell_quote(&capture)),
        );
        let (proxy, _peer) = duplex(64);
        let (mut policy, channels) = test_policy(Duration::from_secs(5), Duration::from_secs(1));
        policy.viewer_environment = super::ViewerEnvironment::for_test(
            [
                Some("/synthetic/home"),
                Some("/synthetic/tmp"),
                Some("C"),
                Some("C"),
                Some("C"),
            ],
            &[
                ("PATH", "/synthetic/parent-path"),
                ("HOME", "/blocked-home"),
                ("ROC_SYNTHETIC_SECRET", "blocked-arbitrary"),
                ("DYLD_INSERT_LIBRARIES", "blocked-loader"),
                ("LD_PRELOAD", "blocked-loader"),
                ("SSH_AUTH_SOCK", "blocked-ssh"),
                ("SSH_AGENT_PID", "blocked-ssh"),
                ("LC_PVE_TICKET", "blocked-ticket"),
            ],
        );
        let mut session = open_test(
            &runtime,
            &viewer,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let _address = channels.bound.await.unwrap();
        let pid = channels.pid.await.unwrap();
        wait_for_nonempty(&capture).await;
        assert_eq!(
            fs::read_to_string(&capture)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            [
                "/usr/bin:/bin",
                "/synthetic/home",
                "/synthetic/tmp",
                "C",
                "C",
                "C",
                "absent",
                "absent",
                "absent",
                "absent",
                "absent",
                "absent",
            ]
        );

        session.close().await.unwrap();
        assert_exact_pid_is_gone(pid).await;
        assert_executable_snapshots_are_gone(&runtime).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_setup_faults_are_typed_redacted_and_remove_the_executable() {
        let _guard = crate::ssh::process_test_guard().await;
        for fault in [
            ViewerSnapshotFault::Create,
            ViewerSnapshotFault::Write,
            ViewerSnapshotFault::Sync,
        ] {
            let runtime = RuntimeDir::create().unwrap();
            let fixture = ViewerFixture::new(ViewerBehavior::Hang);
            let (proxy, _peer) = duplex(64);
            let (mut policy, _channels) =
                test_policy(Duration::from_secs(1), Duration::from_secs(1));
            policy.viewer_snapshot.fault = Some(fault);
            let error = open_test(
                &runtime,
                &fixture.path,
                proxy,
                FallbackPreferences::default(),
                policy,
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), FallbackErrorKind::ViewerSnapshot);
            assert!(!error.has_cleanup_failure());
            assert_executable_snapshots_are_gone(&runtime).await;
            assert!(password_files(&runtime).is_empty());
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains(TICKET));
            assert!(!rendered.contains(CIPHERTEXT_HEX));
            assert!(!rendered.contains(&runtime.path().to_string_lossy().into_owned()));
        }

        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::Hang);
        let (proxy, _peer) = duplex(64);
        let (mut policy, _channels) = test_policy(Duration::from_secs(1), Duration::from_secs(1));
        policy.viewer_snapshot.fault = Some(ViewerSnapshotFault::Write);
        policy.viewer_snapshot.remove_failures = 1;
        let error = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), FallbackErrorKind::ViewerSnapshot);
        assert!(error.has_cleanup_failure());
        assert_executable_snapshots_are_gone(&runtime).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn password_setup_cleanup_failure_propagates_and_raii_retry_removes_artifact() {
        let _guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::Hang);
        let (proxy, _peer) = duplex(64);
        let (mut policy, _channels) = test_policy(Duration::from_secs(1), Duration::from_secs(1));
        policy.password_file.fault = Some(PasswordFileFault::WriteAfter(3));
        policy.password_file.remove_failures = 1;

        let error = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), FallbackErrorKind::PasswordFile);
        assert!(error.has_cleanup_failure());
        assert_password_files_are_gone(&runtime).await;
        assert_executable_snapshots_are_gone(&runtime).await;
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(TICKET));
        assert!(!rendered.contains(CIPHERTEXT_HEX));
        assert!(!rendered.contains(&runtime.path().to_string_lossy().into_owned()));
    }

    #[test]
    fn optional_viewer_flags_are_added_exactly_once_only_when_requested() {
        let password = Path::new("/synthetic/private/password-file");
        let endpoint = "127.0.0.1::59000";
        for (preferences, expected_optional) in [
            (FallbackPreferences::default(), Vec::<&str>::new()),
            (
                FallbackPreferences {
                    fullscreen: true,
                    view_only: false,
                },
                vec!["-FullScreen=1"],
            ),
            (
                FallbackPreferences {
                    fullscreen: false,
                    view_only: true,
                },
                vec!["-ViewOnly=1"],
            ),
            (
                FallbackPreferences {
                    fullscreen: true,
                    view_only: true,
                },
                vec!["-FullScreen=1", "-ViewOnly=1"],
            ),
        ] {
            let arguments = viewer_arguments(password, endpoint, preferences);
            let rendered = arguments
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let mut expected = vec![
                "-Shared=1",
                "-RemoteResize=1",
                "-SecurityTypes=VncAuth",
                "-PasswordFile",
                "/synthetic/private/password-file",
            ];
            expected.extend(expected_optional);
            expected.push(endpoint);
            assert_eq!(rendered, expected);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exact_direct_argv_loopback_single_client_and_bounded_bidirectional_relay() {
        let _guard = crate::ssh::process_test_guard().await;
        let _environment = ParentTicketEnvironment::install();
        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::Hang);
        let (proxy, mut proxy_peer) = duplex(64);
        let (policy, channels) = test_policy(Duration::from_secs(5), Duration::from_secs(2));
        let mut session = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences {
                fullscreen: true,
                view_only: true,
            },
            policy,
        )
        .await
        .unwrap();
        let address = channels.bound.await.unwrap();
        assert_eq!(
            address.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        let pid = channels.pid.await.unwrap();
        let mut viewer = TcpStream::connect(address).await.unwrap();
        channels.accepted.await.unwrap();
        assert_eq!(
            password_files(&runtime).len(),
            1,
            "TigerVNC must be able to read its password file after connecting"
        );
        assert!(TcpStream::connect(address).await.is_err());

        viewer.write_all(b"viewer-bytes").await.unwrap();
        let mut from_viewer = [0_u8; 12];
        proxy_peer.read_exact(&mut from_viewer).await.unwrap();
        assert_eq!(&from_viewer, b"viewer-bytes");
        proxy_peer.write_all(b"proxy-bytes").await.unwrap();
        let mut from_proxy = [0_u8; 11];
        viewer.read_exact(&mut from_proxy).await.unwrap();
        assert_eq!(&from_proxy, b"proxy-bytes");

        wait_for_nonempty(&fixture.artifact(".argv")).await;
        let arguments = fs::read_to_string(fixture.artifact(".argv"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            &arguments[0..4],
            [
                "-Shared=1",
                "-RemoteResize=1",
                "-SecurityTypes=VncAuth",
                "-PasswordFile",
            ]
        );
        assert!(Path::new(&arguments[4]).starts_with(runtime.path()));
        assert_eq!(arguments[5], "-FullScreen=1");
        assert_eq!(arguments[6], "-ViewOnly=1");
        assert_eq!(arguments[7], format!("127.0.0.1::{}", address.port()));
        wait_for_nonempty(&fixture.artifact(".ticket-env")).await;
        assert_eq!(
            fs::read_to_string(fixture.artifact(".ticket-env")).unwrap(),
            "absent\n"
        );
        let rendered = arguments.join(" ");
        assert!(!rendered.contains(TICKET));
        assert!(!rendered.contains(CIPHERTEXT_HEX));

        drop(viewer);
        drop(proxy_peer);
        completed(&mut session).await.unwrap();
        assert_password_files_are_gone(&runtime).await;
        assert_exact_pid_is_gone(pid).await;
        assert_owner_is_gone().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verified_fake_ssh_proxy_and_exact_viewer_child_are_reaped_after_relay() {
        let _guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_ssh_directory, ssh_executable) = fake_ssh();
        let viewer = ViewerFixture::new(ViewerBehavior::Hang);
        let factory =
            SshCommandFactory::new_for_test(ssh_executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let mut verified = master.verify().await.unwrap();
        let proxy = TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap())
            .await
            .unwrap();
        let proxy_pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for_nonempty(&proxy_pid_path).await;
        let proxy_pid = helper_pid(&proxy_pid_path);

        let mut session = super::TigerVncFallback::open(
            proxy,
            &runtime,
            &viewer.path,
            FallbackPreferences::default(),
        )
        .await
        .unwrap();
        wait_for_nonempty(&viewer.artifact(".argv")).await;
        wait_for_nonempty(&viewer.artifact(".pid")).await;
        let viewer_pid = helper_pid(&viewer.artifact(".pid"));
        let arguments = fs::read_to_string(viewer.artifact(".argv")).unwrap();
        let endpoint = arguments.lines().last().unwrap();
        let port = endpoint
            .strip_prefix("127.0.0.1::")
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let address = (std::net::Ipv4Addr::LOCALHOST, port);
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut banner = [0_u8; 12];
        client.read_exact(&mut banner).await.unwrap();
        assert_eq!(&banner, b"RFB 003.008\n");
        assert_eq!(
            password_files(&runtime).len(),
            1,
            "the password file must remain available during viewer authentication"
        );
        client.write_all(b"x").await.unwrap();
        assert!(TcpStream::connect(address).await.is_err());
        drop(client);

        completed(&mut session).await.unwrap();
        assert_password_files_are_gone(&runtime).await;
        assert_exact_pid_is_gone(viewer_pid).await;
        assert_exact_pid_is_gone(proxy_pid).await;
        assert_owner_is_gone().await;
        master.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_preaccept_timeout_and_remove_faults_all_clean_owned_resources() {
        let _guard = crate::ssh::process_test_guard().await;

        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::SpawnFailure);
        let (proxy, _peer) = duplex(64);
        let (policy, _channels) = test_policy(Duration::from_millis(50), Duration::from_secs(1));
        let error = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), FallbackErrorKind::ViewerSpawn);
        assert_password_files_are_gone(&runtime).await;
        assert_executable_snapshots_are_gone(&runtime).await;
        assert_eq!(active_owner_tasks(), 0);

        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::ExitImmediately);
        let (proxy, _peer) = duplex(64);
        let (policy, channels) = test_policy(Duration::from_secs(5), Duration::from_secs(1));
        let mut session = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let pid = channels.pid.await.unwrap();
        let error = completed(&mut session).await.unwrap_err();
        assert_eq!(error.kind(), FallbackErrorKind::ViewerExitedBeforeConnect);
        assert_password_files_are_gone(&runtime).await;
        assert_executable_snapshots_are_gone(&runtime).await;
        assert_exact_pid_is_gone(pid).await;

        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::Hang);
        let (proxy, _peer) = duplex(64);
        let (policy, channels) = test_policy(Duration::from_millis(40), Duration::from_secs(1));
        let mut session = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let pid = channels.pid.await.unwrap();
        let error = completed(&mut session).await.unwrap_err();
        assert_eq!(error.kind(), FallbackErrorKind::AcceptTimedOut);
        assert!(password_files(&runtime).is_empty());
        assert_executable_snapshots_are_gone(&runtime).await;
        assert_exact_pid_is_gone(pid).await;

        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::Hang);
        let (proxy, peer) = duplex(64);
        let (mut policy, channels) = test_policy(Duration::from_secs(1), Duration::from_secs(1));
        policy.fail_password_remove = true;
        let mut session = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let address = channels.bound.await.unwrap();
        let pid = channels.pid.await.unwrap();
        let viewer = TcpStream::connect(address).await.unwrap();
        channels.accepted.await.unwrap();
        assert_eq!(password_files(&runtime).len(), 1);
        drop(viewer);
        drop(peer);
        let error = completed(&mut session).await.unwrap_err();
        assert_eq!(error.kind(), FallbackErrorKind::PasswordFile);
        assert!(error.has_cleanup_failure());
        assert!(password_files(&runtime).is_empty());
        assert_executable_snapshots_are_gone(&runtime).await;
        assert_exact_pid_is_gone(pid).await;
        assert_owner_is_gone().await;
    }

    struct RelayFault;

    impl AsyncRead for RelayFault {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("synthetic relay read failure")))
        }
    }

    impl AsyncWrite for RelayFault {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::other("synthetic relay write failure")))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct StalledOwnedProxy {
        _child: tokio::process::Child,
    }

    impl AsyncRead for StalledOwnedProxy {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for StalledOwnedProxy {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_aborts_stalled_owner_and_only_its_exact_resources() {
        let _guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::Hang);

        let mut proxy_child = TokioCommand::new("/bin/sleep");
        proxy_child.arg("60").kill_on_drop(true);
        let proxy_child = proxy_child.spawn().unwrap();
        let proxy_pid = proxy_child.id().unwrap();
        let proxy = StalledOwnedProxy {
            _child: proxy_child,
        };

        let mut unrelated = TokioCommand::new("/bin/sleep");
        unrelated.arg("60").kill_on_drop(true);
        let mut unrelated = unrelated.spawn().unwrap();
        let unrelated_pid = unrelated.id().unwrap();

        let (policy, channels) = test_policy(Duration::from_secs(60), Duration::from_secs(60));
        let session = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let address = channels.bound.await.unwrap();
        let viewer_pid = channels.pid.await.unwrap();
        assert_owner_started().await;

        drop(session);

        assert_owner_is_gone().await;
        assert_port_is_closed(address).await;
        assert_exact_pid_is_gone(viewer_pid).await;
        assert_exact_pid_is_gone(proxy_pid).await;
        assert_password_files_are_gone(&runtime).await;
        assert_executable_snapshots_are_gone(&runtime).await;
        assert!(
            exact_pid_is_alive(unrelated_pid),
            "Drop touched an unrelated exact PID"
        );

        unrelated.start_kill().unwrap();
        unrelated.wait().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn relay_error_cancellation_drop_natural_exit_and_repeated_close_are_bounded() {
        let _guard = crate::ssh::process_test_guard().await;

        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::Hang);
        let (policy, channels) = test_policy(Duration::from_secs(1), Duration::from_secs(1));
        let mut session = open_test(
            &runtime,
            &fixture.path,
            RelayFault,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let address = channels.bound.await.unwrap();
        let pid = channels.pid.await.unwrap();
        let _viewer = TcpStream::connect(address).await.unwrap();
        let error = completed(&mut session).await.unwrap_err();
        assert_eq!(error.kind(), FallbackErrorKind::Relay);
        assert!(password_files(&runtime).is_empty());
        assert_executable_snapshots_are_gone(&runtime).await;
        assert_exact_pid_is_gone(pid).await;

        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::Hang);
        let (proxy, _peer) = duplex(64);
        let (policy, channels) = test_policy(Duration::from_secs(5), Duration::from_secs(1));
        let mut session = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let address = channels.bound.await.unwrap();
        let pid = channels.pid.await.unwrap();
        session.close().await.unwrap();
        session.close().await.unwrap();
        assert!(TcpStream::connect(address).await.is_err());
        assert!(password_files(&runtime).is_empty());
        assert_executable_snapshots_are_gone(&runtime).await;
        assert_exact_pid_is_gone(pid).await;

        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::Hang);
        let (proxy, _peer) = duplex(64);
        let (policy, channels) = test_policy(Duration::from_secs(5), Duration::from_secs(1));
        let session = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let pid = channels.pid.await.unwrap();
        drop(session);
        assert_owner_is_gone().await;
        assert_password_files_are_gone(&runtime).await;
        assert_executable_snapshots_are_gone(&runtime).await;
        assert_exact_pid_is_gone(pid).await;

        let runtime = RuntimeDir::create().unwrap();
        let fixture = ViewerFixture::new(ViewerBehavior::ExitOnMarker);
        let (proxy, _peer) = duplex(64);
        let (policy, channels) = test_policy(Duration::from_secs(2), Duration::from_secs(1));
        let mut session = open_test(
            &runtime,
            &fixture.path,
            proxy,
            FallbackPreferences::default(),
            policy,
        )
        .await
        .unwrap();
        let address = channels.bound.await.unwrap();
        let pid = channels.pid.await.unwrap();
        let _viewer = TcpStream::connect(address).await.unwrap();
        channels.accepted.await.unwrap();
        fs::write(fixture.artifact(".exit"), b"synthetic control\n").unwrap();
        completed(&mut session).await.unwrap();
        assert!(password_files(&runtime).is_empty());
        assert_executable_snapshots_are_gone(&runtime).await;
        assert_exact_pid_is_gone(pid).await;
        assert_owner_is_gone().await;
    }

    #[test]
    fn fallback_errors_and_debug_surfaces_never_render_ticket_or_ciphertext() {
        let rendered = format!(
            "{:?} {}",
            super::FallbackError::new(FallbackErrorKind::ViewerSpawn),
            super::FallbackError::new(FallbackErrorKind::ViewerSpawn)
        );
        assert!(!rendered.contains(TICKET));
        assert!(!rendered.contains(CIPHERTEXT_HEX));
    }
}
