use std::{
    future::poll_fn,
    io,
    pin::Pin,
    process::{ExitStatus, Stdio},
    sync::{mpsc, Arc, Mutex},
    task::{Context, Poll, Waker},
    thread,
    time::Duration,
};

#[cfg(test)]
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    process::{Child, ChildStdin, ChildStdout},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

use super::{classify_stderr, master::capture_bounded, CommandSpec, SshFailure};

const MAX_CAPTURED_STDERR_BYTES: usize = 65_536;
const GRACEFUL_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
const REAP_TIMEOUT: Duration = Duration::from_secs(1);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(test)]
static ACTIVE_SUPERVISORS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ProxyStreamError {
    #[error("owned SSH proxy I/O failed ({0:?})")]
    Io(io::ErrorKind),
    #[error(transparent)]
    Ssh(SshFailure),
    #[error("owned SSH proxy child cleanup failed")]
    CleanupFailed,
    #[error("owned SSH proxy I/O and SSH terminal status both failed")]
    IoAndSsh { io: io::ErrorKind, ssh: SshFailure },
    #[error("owned SSH proxy I/O and child cleanup both failed")]
    IoAndCleanup { io: io::ErrorKind },
    #[error("owned SSH proxy SSH terminal status and child cleanup both failed")]
    SshAndCleanup { ssh: SshFailure },
    #[error("owned SSH proxy I/O, SSH terminal status, and child cleanup all failed")]
    IoSshAndCleanup { io: io::ErrorKind, ssh: SshFailure },
}

impl ProxyStreamError {
    pub fn has_io_failure(self) -> bool {
        matches!(
            self,
            Self::Io(_)
                | Self::IoAndSsh { .. }
                | Self::IoAndCleanup { .. }
                | Self::IoSshAndCleanup { .. }
        )
    }

    pub fn has_cleanup_failure(self) -> bool {
        matches!(
            self,
            Self::CleanupFailed
                | Self::IoAndCleanup { .. }
                | Self::SshAndCleanup { .. }
                | Self::IoSshAndCleanup { .. }
        )
    }

    pub fn ssh_failure_kind(self) -> Option<super::SshFailureKind> {
        match self {
            Self::Ssh(ssh)
            | Self::IoAndSsh { ssh, .. }
            | Self::SshAndCleanup { ssh }
            | Self::IoSshAndCleanup { ssh, .. } => Some(ssh.kind()),
            Self::Io(_) | Self::CleanupFailed | Self::IoAndCleanup { .. } => None,
        }
    }

    fn with_io(self, io: io::ErrorKind) -> Self {
        match self {
            Self::Ssh(ssh) => Self::IoAndSsh { io, ssh },
            Self::CleanupFailed => Self::IoAndCleanup { io },
            Self::SshAndCleanup { ssh } => Self::IoSshAndCleanup { io, ssh },
            error @ (Self::Io(_)
            | Self::IoAndSsh { .. }
            | Self::IoAndCleanup { .. }
            | Self::IoSshAndCleanup { .. }) => error,
        }
    }

    fn with_cleanup(self) -> Self {
        match self {
            Self::Io(io) => Self::IoAndCleanup { io },
            Self::Ssh(ssh) => Self::SshAndCleanup { ssh },
            Self::IoAndSsh { io, ssh } => Self::IoSshAndCleanup { io, ssh },
            error @ (Self::CleanupFailed
            | Self::IoAndCleanup { .. }
            | Self::SshAndCleanup { .. }
            | Self::IoSshAndCleanup { .. }) => error,
        }
    }

    fn as_io_error(self) -> io::Error {
        let kind = match self {
            Self::Io(kind)
            | Self::IoAndSsh { io: kind, .. }
            | Self::IoAndCleanup { io: kind }
            | Self::IoSshAndCleanup { io: kind, .. } => kind,
            Self::Ssh(_) | Self::CleanupFailed | Self::SshAndCleanup { .. } => io::ErrorKind::Other,
        };
        io::Error::new(kind, self)
    }
}

impl From<io::Error> for ProxyStreamError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

impl From<SshFailure> for ProxyStreamError {
    fn from(failure: SshFailure) -> Self {
        Self::Ssh(failure)
    }
}

type TerminalResult = Result<(), ProxyStreamError>;

#[derive(Default)]
struct CompletionState {
    result: Option<TerminalResult>,
    waker: Option<Waker>,
}

#[derive(Default)]
struct SharedCompletion(Mutex<CompletionState>);

impl SharedCompletion {
    fn complete(&self, result: TerminalResult) {
        let mut state = self.0.lock().expect("proxy completion mutex poisoned");
        if state.result.is_none() {
            state.result = Some(result);
        }
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<TerminalResult> {
        let mut state = self.0.lock().expect("proxy completion mutex poisoned");
        if let Some(result) = state.result {
            Poll::Ready(result)
        } else {
            if state
                .waker
                .as_ref()
                .is_none_or(|waker| !waker.will_wake(cx.waker()))
            {
                state.waker = Some(cx.waker().clone());
            }
            Poll::Pending
        }
    }
}

#[derive(Clone)]
struct StreamPolicy {
    graceful_close_timeout: Duration,
    reap_timeout: Duration,
    pipe_drain_timeout: Duration,
    #[cfg(test)]
    readiness: Option<PathBuf>,
}

impl StreamPolicy {
    fn production() -> Self {
        Self {
            graceful_close_timeout: GRACEFUL_CLOSE_TIMEOUT,
            reap_timeout: REAP_TIMEOUT,
            pipe_drain_timeout: PIPE_DRAIN_TIMEOUT,
            #[cfg(test)]
            readiness: None,
        }
    }
}

#[cfg(test)]
type TestStreamPolicy = StreamPolicy;

#[cfg(test)]
impl TestStreamPolicy {
    fn short_after_ready(readiness: PathBuf) -> Self {
        Self {
            graceful_close_timeout: Duration::from_millis(150),
            reap_timeout: Duration::from_secs(5),
            pipe_drain_timeout: Duration::from_secs(5),
            readiness: Some(readiness),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestIoFault {
    Read,
    Write,
    Flush,
    Shutdown,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestCleanupFault {
    Kill,
    Wait,
    Drain,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestSetupFault {
    MissingStdout,
}

#[derive(Clone, Copy, Default)]
struct StreamFaults {
    #[cfg(test)]
    io: Option<TestIoFault>,
    #[cfg(test)]
    cleanup: Option<TestCleanupFault>,
    #[cfg(test)]
    setup: Option<TestSetupFault>,
}

#[cfg(test)]
type TestStreamFaults = StreamFaults;

#[cfg(test)]
impl TestStreamFaults {
    fn io(io: TestIoFault) -> Self {
        Self {
            io: Some(io),
            ..Self::default()
        }
    }

    fn cleanup(cleanup: TestCleanupFault) -> Self {
        Self {
            cleanup: Some(cleanup),
            ..Self::default()
        }
    }

    fn setup(setup: TestSetupFault) -> Self {
        Self {
            setup: Some(setup),
            ..Self::default()
        }
    }
}

impl StreamFaults {
    fn kill_fails(self) -> bool {
        #[cfg(test)]
        {
            self.cleanup == Some(TestCleanupFault::Kill)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn wait_fails(self) -> bool {
        #[cfg(test)]
        {
            self.cleanup == Some(TestCleanupFault::Wait)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn drain_fails(self) -> bool {
        #[cfg(test)]
        {
            self.cleanup == Some(TestCleanupFault::Drain)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn missing_stdout(self) -> bool {
        #[cfg(test)]
        {
            self.setup == Some(TestSetupFault::MissingStdout)
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

/// Direct asynchronous byte I/O over one owned OpenSSH child's pipes.
///
/// The child handle lives in one private supervisor. Dropping or cancelling
/// the stream closes stdin and signals that supervisor; explicit `close`
/// additionally awaits the same bounded kill/reap/drain path.
pub struct ProxyStream {
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    cleanup: Option<oneshot::Sender<()>>,
    completion: Arc<SharedCompletion>,
    supervisor: Option<thread::JoinHandle<()>>,
    first_io_error: Option<io::ErrorKind>,
    terminal_result: Option<TerminalResult>,
    shutdown_started: bool,
    #[cfg(test)]
    faults: StreamFaults,
}

impl ProxyStream {
    pub(super) fn spawn(spec: CommandSpec) -> Result<Self, ProxyStreamError> {
        Self::spawn_inner(spec, StreamFaults::default(), StreamPolicy::production())
    }

    #[cfg(test)]
    fn spawn_with_test_seams(
        spec: CommandSpec,
        faults: TestStreamFaults,
        policy: TestStreamPolicy,
    ) -> Result<Self, ProxyStreamError> {
        Self::spawn_inner(spec, faults, policy)
    }

    fn spawn_inner(
        spec: CommandSpec,
        faults: StreamFaults,
        policy: StreamPolicy,
    ) -> Result<Self, ProxyStreamError> {
        let completion = Arc::new(SharedCompletion::default());
        let supervisor_completion = Arc::clone(&completion);
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let supervisor = thread::Builder::new()
            .name("rustedoutclient-ssh-proxy-reaper".to_owned())
            .spawn(move || {
                supervise_process(spec, faults, policy, supervisor_completion, startup_sender);
            })
            .map_err(ProxyStreamError::from)?;

        match startup_receiver.recv() {
            Ok(Ok(startup)) => Ok(Self {
                stdin: Some(startup.stdin),
                stdout: startup.stdout,
                cleanup: Some(startup.cleanup),
                completion,
                supervisor: Some(supervisor),
                first_io_error: None,
                terminal_result: None,
                shutdown_started: false,
                #[cfg(test)]
                faults,
            }),
            Ok(Err(error)) => {
                let _ = supervisor.join();
                Err(error)
            }
            Err(_) => {
                let _ = supervisor.join();
                Err(ProxyStreamError::CleanupFailed)
            }
        }
    }

    pub async fn close(&mut self) -> Result<(), ProxyStreamError> {
        poll_fn(|cx| self.poll_close_typed(cx)).await
    }

    fn signal_cleanup(&mut self) {
        self.stdin.take();
        if let Some(cleanup) = self.cleanup.take() {
            let _ = cleanup.send(());
        }
        self.shutdown_started = true;
    }

    fn record_io_error(&mut self, kind: io::ErrorKind) {
        if self.first_io_error.is_none() {
            self.first_io_error = Some(kind);
        }
        self.signal_cleanup();
    }

    #[cfg(test)]
    fn inject_io_fault(&mut self, expected: TestIoFault) -> Option<io::Error> {
        if self.faults.io == Some(expected) {
            self.faults.io = None;
            let kind = io::ErrorKind::BrokenPipe;
            self.record_io_error(kind);
            Some(io::Error::new(kind, "synthetic proxy I/O fault"))
        } else {
            None
        }
    }

    fn poll_close_typed(&mut self, cx: &mut Context<'_>) -> Poll<TerminalResult> {
        if let Some(result) = self.terminal_result {
            return Poll::Ready(result);
        }

        if !self.shutdown_started {
            #[cfg(test)]
            if self.inject_io_fault(TestIoFault::Shutdown).is_some() {
                // The first pipe failure is retained while cleanup continues.
            } else if let Some(stdin) = self.stdin.as_mut() {
                match Pin::new(stdin).poll_shutdown(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => self.record_io_error(error.kind()),
                    Poll::Ready(Ok(())) => self.signal_cleanup(),
                }
            } else {
                self.signal_cleanup();
            }
            #[cfg(not(test))]
            if let Some(stdin) = self.stdin.as_mut() {
                match Pin::new(stdin).poll_shutdown(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => self.record_io_error(error.kind()),
                    Poll::Ready(Ok(())) => self.signal_cleanup(),
                }
            } else {
                self.signal_cleanup();
            }
        }

        match self.completion.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                let result = match (self.first_io_error, result) {
                    (Some(kind), Ok(())) => Err(ProxyStreamError::Io(kind)),
                    (Some(kind), Err(error)) => Err(error.with_io(kind)),
                    (None, result) => result,
                };
                self.terminal_result = Some(result);
                if let Some(supervisor) = self.supervisor.take() {
                    let _ = supervisor.join();
                }
                Poll::Ready(result)
            }
        }
    }
}

impl AsyncRead for ProxyStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        #[cfg(test)]
        if let Some(error) = self.inject_io_fault(TestIoFault::Read) {
            return Poll::Ready(Err(error));
        }
        let filled_before = buffer.filled().len();
        let had_capacity = buffer.remaining() != 0;
        let result = Pin::new(&mut self.stdout).poll_read(cx, buffer);
        match &result {
            Poll::Ready(Err(error)) => self.record_io_error(error.kind()),
            Poll::Ready(Ok(())) if had_capacity && buffer.filled().len() == filled_before => {
                self.signal_cleanup();
            }
            Poll::Pending | Poll::Ready(Ok(())) => {}
        }
        result
    }
}

impl AsyncWrite for ProxyStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.shutdown_started {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SSH proxy stream is closed",
            )));
        }
        #[cfg(test)]
        if let Some(error) = self.inject_io_fault(TestIoFault::Write) {
            return Poll::Ready(Err(error));
        }
        let result = match self.stdin.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_write(cx, buffer),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SSH proxy stdin is unavailable",
            ))),
        };
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error.kind());
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.shutdown_started {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SSH proxy stream is closed",
            )));
        }
        #[cfg(test)]
        if let Some(error) = self.inject_io_fault(TestIoFault::Flush) {
            return Poll::Ready(Err(error));
        }
        let result = match self.stdin.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_flush(cx),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SSH proxy stdin is unavailable",
            ))),
        };
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error.kind());
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.poll_close_typed(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error.as_io_error())),
        }
    }
}

impl Drop for ProxyStream {
    fn drop(&mut self) {
        self.signal_cleanup();
        // The private supervisor owns the child and its independent runtime.
        // Detaching its thread here lets bounded kill/wait/drain finish even
        // when the application runtime itself is being torn down.
        self.supervisor.take();
    }
}

struct Startup {
    stdin: ChildStdin,
    stdout: ChildStdout,
    cleanup: oneshot::Sender<()>,
}

fn supervise_process(
    spec: CommandSpec,
    faults: StreamFaults,
    policy: StreamPolicy,
    completion: Arc<SharedCompletion>,
    startup: mpsc::SyncSender<Result<Startup, ProxyStreamError>>,
) {
    #[cfg(test)]
    ACTIVE_SUPERVISORS.fetch_add(1, Ordering::SeqCst);
    struct ActiveGuard;
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            #[cfg(test)]
            ACTIVE_SUPERVISORS.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _active = ActiveGuard;

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = startup.send(Err(error.into()));
            return;
        }
    };
    runtime.block_on(async move {
        let mut command = tokio::process::Command::from(spec.to_command());
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = startup.send(Err(error.into()));
                return;
            }
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr_task = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(capture_bounded(stderr, MAX_CAPTURED_STDERR_BYTES)));

        wait_for_test_readiness(&policy).await;
        let setup_error = if stdin.is_none() {
            Some(io::Error::other("SSH proxy stdin pipe was not available"))
        } else if stdout.is_none() || faults.missing_stdout() {
            Some(io::Error::other("SSH proxy stdout pipe was not available"))
        } else {
            None
        };
        if let Some(error) = setup_error {
            let outcome = terminate_owned_child(&mut child, faults, &policy).await;
            let stderr_result = finish_stderr(stderr_task, faults, &policy).await;
            let mut result = ProxyStreamError::from(error);
            if outcome.cleanup_failed || stderr_result.is_err() {
                result = result.with_cleanup();
            }
            let _ = startup.send(Err(result));
            return;
        }

        let (cleanup, cleanup_requested) = oneshot::channel();
        if startup
            .send(Ok(Startup {
                stdin: stdin.expect("pipe checked above"),
                stdout: stdout.expect("pipe checked above"),
                cleanup,
            }))
            .is_err()
        {
            let outcome = terminate_owned_child(&mut child, faults, &policy).await;
            let stderr_result = finish_stderr(stderr_task, faults, &policy).await;
            let result = compose_terminal(outcome, stderr_result);
            completion.complete(result);
            return;
        }

        own_child(
            child,
            stderr_task,
            cleanup_requested,
            faults,
            &policy,
            completion,
        )
        .await;
    });
}

async fn own_child(
    mut child: Child,
    stderr_task: Option<JoinHandle<io::Result<Vec<u8>>>>,
    mut cleanup_requested: oneshot::Receiver<()>,
    faults: StreamFaults,
    policy: &StreamPolicy,
    completion: Arc<SharedCompletion>,
) {
    enum Trigger {
        Natural(io::Result<ExitStatus>),
        Cleanup,
    }
    let trigger = tokio::select! {
        status = child.wait() => Trigger::Natural(status),
        _ = &mut cleanup_requested => Trigger::Cleanup,
    };
    match trigger {
        Trigger::Natural(status) => {
            let outcome = match status {
                Ok(status) => StopOutcome {
                    status: Some(status),
                    forced: false,
                    cleanup_failed: false,
                },
                Err(_) => {
                    let mut outcome = terminate_owned_child(&mut child, faults, policy).await;
                    outcome.cleanup_failed = true;
                    outcome
                }
            };
            let stderr = finish_stderr(stderr_task, faults, policy).await;
            completion.complete(compose_terminal(outcome, stderr));
            let _ = cleanup_requested.await;
        }
        Trigger::Cleanup => {
            let outcome = terminate_owned_child(&mut child, faults, policy).await;
            let stderr = finish_stderr(stderr_task, faults, policy).await;
            completion.complete(compose_terminal(outcome, stderr));
        }
    }
}

#[derive(Clone, Copy)]
struct StopOutcome {
    status: Option<ExitStatus>,
    forced: bool,
    cleanup_failed: bool,
}

async fn terminate_owned_child(
    child: &mut Child,
    faults: StreamFaults,
    policy: &StreamPolicy,
) -> StopOutcome {
    match timeout(policy.graceful_close_timeout, child.wait()).await {
        Ok(Ok(status)) => StopOutcome {
            status: Some(status),
            forced: false,
            cleanup_failed: false,
        },
        first_wait => {
            let mut cleanup_failed = matches!(first_wait, Ok(Err(_)));
            let kill_result = child.start_kill();
            if kill_result.is_err() || faults.kill_fails() {
                cleanup_failed = true;
            }
            let final_wait = timeout(policy.reap_timeout, child.wait()).await;
            let status = match final_wait {
                Ok(Ok(status)) => Some(status),
                Ok(Err(_)) | Err(_) => {
                    cleanup_failed = true;
                    None
                }
            };
            if faults.wait_fails() {
                cleanup_failed = true;
            }
            StopOutcome {
                status,
                forced: true,
                cleanup_failed,
            }
        }
    }
}

async fn finish_stderr(
    mut task: Option<JoinHandle<io::Result<Vec<u8>>>>,
    faults: StreamFaults,
    policy: &StreamPolicy,
) -> Result<Vec<u8>, ProxyStreamError> {
    let result = match task.as_mut() {
        Some(task) => match timeout(policy.pipe_drain_timeout, &mut *task).await {
            Ok(Ok(Ok(stderr))) => Ok(stderr),
            Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => {
                task.abort();
                Err(ProxyStreamError::CleanupFailed)
            }
        },
        None => Ok(Vec::new()),
    };
    if faults.drain_fails() {
        Err(ProxyStreamError::CleanupFailed)
    } else {
        result
    }
}

fn compose_terminal(
    outcome: StopOutcome,
    stderr: Result<Vec<u8>, ProxyStreamError>,
) -> TerminalResult {
    let mut cleanup_failed = outcome.cleanup_failed || stderr.is_err() || outcome.status.is_none();
    let process_result = match (outcome.status, outcome.forced, stderr.as_ref()) {
        (Some(_), true, _) => Ok(()),
        (Some(status), false, Ok(stderr)) => classify_status(status, stderr),
        (Some(_), false, Err(_)) => Ok(()),
        (None, _, _) => {
            cleanup_failed = true;
            Ok(())
        }
    };
    match (process_result, cleanup_failed) {
        (Ok(()), false) => Ok(()),
        (Ok(()), true) => Err(ProxyStreamError::CleanupFailed),
        (Err(error), false) => Err(error),
        (Err(error), true) => Err(error.with_cleanup()),
    }
}

fn classify_status(status: ExitStatus, stderr: &[u8]) -> TerminalResult {
    if status.success() {
        Ok(())
    } else {
        Err(classify_stderr(stderr).into())
    }
}

async fn wait_for_test_readiness(policy: &StreamPolicy) {
    #[cfg(test)]
    if let Some(path) = &policy.readiness {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !path.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    #[cfg(not(test))]
    let _ = policy;
}

#[cfg(test)]
fn active_supervisors() -> usize {
    ACTIVE_SUPERVISORS.load(Ordering::SeqCst)
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
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{tempdir, TempDir};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        active_supervisors, ProxyStream, ProxyStreamError, TestCleanupFault, TestIoFault,
        TestSetupFault, TestStreamFaults, TestStreamPolicy, MAX_CAPTURED_STDERR_BYTES,
    };
    use crate::{
        model::{NodeName, PveProfile, SshTarget, VmId},
        runtime::RuntimeDir,
        ssh::{ProxyTicket, SshCommandFactory, SshFailureKind},
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
        (directory, executable)
    }

    fn spawn_test_stream(
        runtime: &RuntimeDir,
        executable: PathBuf,
        faults: TestStreamFaults,
    ) -> Result<ProxyStream, ProxyStreamError> {
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let ticket = ProxyTicket::generate();
        let spec = factory
            .proxy(&fixture_profile(), VmId::new(107).unwrap(), &ticket)
            .unwrap();
        ProxyStream::spawn_with_test_seams(
            spec,
            faults,
            TestStreamPolicy::short_after_ready(
                runtime.control_socket().with_extension("proxy.pid"),
            ),
        )
    }

    async fn wait_for(path: &Path) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synthetic helper did not become ready");
    }

    fn helper_pid(path: &Path) -> u32 {
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
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
        tokio::time::timeout(Duration::from_secs(30), async {
            while exact_pid_is_alive(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned synthetic proxy child was not reaped");
    }

    fn assert_exact_pid_is_gone_blocking(pid: u32) {
        let started = Instant::now();
        while exact_pid_is_alive(pid) {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "owned synthetic proxy child was not reaped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn stderr_capture_limit_is_exactly_sixty_four_kib() {
        assert_eq!(MAX_CAPTURED_STDERR_BYTES, 65_536);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn eof_while_process_is_live_initiates_owned_cleanup() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("proxy_eof_live"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let mut stream =
            spawn_test_stream(&runtime, executable, TestStreamFaults::default()).unwrap();
        let pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&pid_path).await;
        let pid = helper_pid(&pid_path);

        let mut output = Vec::new();
        stream.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"RFB 003.008\n");
        stream.close().await.unwrap();
        assert_exact_pid_is_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn every_terminal_io_fault_starts_cleanup_and_close_repeats_the_same_error() {
        let _process_guard = crate::ssh::process_test_guard().await;
        for fault in [
            TestIoFault::Read,
            TestIoFault::Write,
            TestIoFault::Flush,
            TestIoFault::Shutdown,
        ] {
            let runtime = RuntimeDir::create().unwrap();
            let (_fixture_directory, executable) = fake_ssh();
            fs::write(
                runtime.control_socket().with_extension("hang_proxy"),
                b"synthetic fixture control\n",
            )
            .unwrap();
            let mut stream =
                spawn_test_stream(&runtime, executable, TestStreamFaults::io(fault)).unwrap();
            let pid_path = runtime.control_socket().with_extension("proxy.pid");
            wait_for(&pid_path).await;
            let pid = helper_pid(&pid_path);

            let operation = match fault {
                TestIoFault::Read => stream.read_u8().await.map(|_| ()),
                TestIoFault::Write => stream.write_all(b"x").await,
                TestIoFault::Flush => stream.flush().await,
                TestIoFault::Shutdown => Ok(()),
            };
            if fault != TestIoFault::Shutdown {
                assert!(operation.is_err());
            }
            let first = stream.close().await.unwrap_err();
            let second = stream.close().await.unwrap_err();
            assert_eq!(first, second);
            assert!(first.has_io_failure());
            assert_exact_pid_is_gone(pid).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn graceful_nonzero_close_is_classified_and_idempotent() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime
                .control_socket()
                .with_extension("proxy_auth_failure"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let mut stream =
            spawn_test_stream(&runtime, executable, TestStreamFaults::default()).unwrap();
        let pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&pid_path).await;
        let pid = helper_pid(&pid_path);
        stream.write_all(b"x").await.unwrap();

        let first = stream.close().await.unwrap_err();
        let second = stream.close().await.unwrap_err();
        assert_eq!(first, second);
        assert_eq!(
            first.ssh_failure_kind(),
            Some(SshFailureKind::Authentication)
        );
        assert_exact_pid_is_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_wait_and_drain_cleanup_faults_still_reap_and_repeat_typed_failure() {
        let _process_guard = crate::ssh::process_test_guard().await;
        for fault in [
            TestCleanupFault::Kill,
            TestCleanupFault::Wait,
            TestCleanupFault::Drain,
        ] {
            let runtime = RuntimeDir::create().unwrap();
            let (_fixture_directory, executable) = fake_ssh();
            fs::write(
                runtime.control_socket().with_extension("hang_proxy"),
                b"synthetic fixture control\n",
            )
            .unwrap();
            let mut stream =
                spawn_test_stream(&runtime, executable, TestStreamFaults::cleanup(fault)).unwrap();
            let pid_path = runtime.control_socket().with_extension("proxy.pid");
            wait_for(&pid_path).await;
            let pid = helper_pid(&pid_path);

            let first = stream.close().await.unwrap_err();
            let second = stream.close().await.unwrap_err();
            assert_eq!(first, second);
            assert!(first.has_cleanup_failure());
            assert_exact_pid_is_gone(pid).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn first_pipe_error_is_combined_with_typed_terminal_failure() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime
                .control_socket()
                .with_extension("proxy_auth_failure"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let mut stream = spawn_test_stream(
            &runtime,
            executable,
            TestStreamFaults::io(TestIoFault::Flush),
        )
        .unwrap();
        let pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&pid_path).await;
        let pid = helper_pid(&pid_path);
        assert!(stream.flush().await.is_err());

        let first = stream.close().await.unwrap_err();
        let second = stream.close().await.unwrap_err();
        assert_eq!(first, second);
        assert!(first.has_io_failure());
        assert_eq!(
            first.ssh_failure_kind(),
            Some(SshFailureKind::Authentication)
        );
        assert_exact_pid_is_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn setup_failure_after_spawn_confirms_exact_child_reap() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("hang_proxy"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let result = spawn_test_stream(
            &runtime,
            executable,
            TestStreamFaults::setup(TestSetupFault::MissingStdout),
        );
        assert!(result.is_err());
        let pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&pid_path).await;
        let pid = helper_pid(&pid_path);
        assert_exact_pid_is_gone(pid).await;
        assert_eq!(active_supervisors(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn independent_supervisor_reaps_when_stream_drops_after_runtime_shutdown() {
        let application_runtime = tokio::runtime::Runtime::new().unwrap();
        let _process_guard = application_runtime.block_on(crate::ssh::process_test_guard());
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("hang_proxy"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let stream = application_runtime.block_on(async {
            let stream =
                spawn_test_stream(&runtime, executable, TestStreamFaults::default()).unwrap();
            wait_for(&runtime.control_socket().with_extension("proxy.pid")).await;
            stream
        });
        let pid = helper_pid(&runtime.control_socket().with_extension("proxy.pid"));
        drop(application_runtime);
        drop(stream);

        assert_exact_pid_is_gone_blocking(pid);
        let started = Instant::now();
        while active_supervisors() != 0 {
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn independent_supervisor_reaps_stream_cancelled_by_runtime_teardown() {
        let application_runtime = tokio::runtime::Runtime::new().unwrap();
        let _process_guard = application_runtime.block_on(crate::ssh::process_test_guard());
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("hang_proxy"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        application_runtime.block_on(async {
            let stream =
                spawn_test_stream(&runtime, executable, TestStreamFaults::default()).unwrap();
            wait_for(&runtime.control_socket().with_extension("proxy.pid")).await;
            tokio::spawn(async move {
                let _stream = stream;
                std::future::pending::<()>().await;
            });
        });
        let pid = helper_pid(&runtime.control_socket().with_extension("proxy.pid"));
        drop(application_runtime);

        assert_exact_pid_is_gone_blocking(pid);
        let started = Instant::now();
        while active_supervisors() != 0 {
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
