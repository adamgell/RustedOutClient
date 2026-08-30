use std::{
    future::{poll_fn, Future},
    io::{self, ErrorKind},
    pin::Pin,
    process::{ExitStatus, Stdio},
    task::{Context, Poll},
    time::{Duration, Instant},
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
static ACTIVE_OWNER_TASKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static ACTIVE_EXCEPTIONAL_REAPERS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static EXCEPTIONAL_REAPER_STARTS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyIoStage {
    Spawn,
    SetupStdin,
    SetupStdout,
    Read,
    Write,
    Flush,
    Shutdown,
    GracefulWait,
    Kill,
    FinalReap,
    StderrDrain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProxyIoFailure {
    stage: ProxyIoStage,
    kind: ErrorKind,
    raw_os_error: Option<i32>,
}

impl ProxyIoFailure {
    fn from_error(stage: ProxyIoStage, error: &io::Error) -> Self {
        Self {
            stage,
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
        }
    }

    pub fn stage(self) -> ProxyIoStage {
        self.stage
    }

    pub fn kind(self) -> ErrorKind {
        self.kind
    }

    pub fn raw_os_error(self) -> Option<i32> {
        self.raw_os_error
    }
}

impl std::fmt::Display for ProxyIoFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "owned SSH proxy {:?} I/O failed ({:?}, OS code {:?})",
            self.stage, self.kind, self.raw_os_error
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyCleanupStage {
    OwnerJoin,
    GracefulWait,
    Kill,
    FinalReap,
    StderrDrain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProxyCleanupFailure {
    stage: ProxyCleanupStage,
    io: Option<ProxyIoFailure>,
}

impl ProxyCleanupFailure {
    fn new(stage: ProxyCleanupStage, io: Option<ProxyIoFailure>) -> Self {
        Self { stage, io }
    }

    pub fn stage(self) -> ProxyCleanupStage {
        self.stage
    }

    pub fn io_failure(self) -> Option<ProxyIoFailure> {
        self.io
    }
}

impl std::fmt::Display for ProxyCleanupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "owned SSH proxy cleanup failed at {:?}",
            self.stage
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ProxyStreamError {
    #[error("{0}")]
    Io(ProxyIoFailure),
    #[error(transparent)]
    Ssh(SshFailure),
    #[error("{0}")]
    CleanupFailed(ProxyCleanupFailure),
    #[error("owned SSH proxy I/O and SSH terminal status both failed")]
    IoAndSsh { io: ProxyIoFailure, ssh: SshFailure },
    #[error("owned SSH proxy I/O and child cleanup both failed")]
    IoAndCleanup {
        io: ProxyIoFailure,
        cleanup: ProxyCleanupFailure,
    },
    #[error("owned SSH proxy SSH terminal status and child cleanup both failed")]
    SshAndCleanup {
        ssh: SshFailure,
        cleanup: ProxyCleanupFailure,
    },
    #[error("owned SSH proxy I/O, SSH terminal status, and child cleanup all failed")]
    IoSshAndCleanup {
        io: ProxyIoFailure,
        ssh: SshFailure,
        cleanup: ProxyCleanupFailure,
    },
}

impl ProxyStreamError {
    pub fn has_io_failure(self) -> bool {
        self.io_failure().is_some()
    }

    pub fn has_cleanup_failure(self) -> bool {
        matches!(
            self,
            Self::CleanupFailed(_)
                | Self::IoAndCleanup { .. }
                | Self::SshAndCleanup { .. }
                | Self::IoSshAndCleanup { .. }
        )
    }

    pub fn ssh_failure_kind(self) -> Option<super::SshFailureKind> {
        match self {
            Self::Ssh(ssh)
            | Self::IoAndSsh { ssh, .. }
            | Self::SshAndCleanup { ssh, .. }
            | Self::IoSshAndCleanup { ssh, .. } => Some(ssh.kind()),
            Self::Io(_) | Self::CleanupFailed(_) | Self::IoAndCleanup { .. } => None,
        }
    }

    pub fn io_failure(self) -> Option<ProxyIoFailure> {
        match self {
            Self::Io(io)
            | Self::IoAndSsh { io, .. }
            | Self::IoAndCleanup { io, .. }
            | Self::IoSshAndCleanup { io, .. } => Some(io),
            Self::CleanupFailed(cleanup) | Self::SshAndCleanup { cleanup, .. } => {
                cleanup.io_failure()
            }
            Self::Ssh(_) => None,
        }
    }

    pub fn cleanup_failure(self) -> Option<ProxyCleanupFailure> {
        match self {
            Self::CleanupFailed(cleanup)
            | Self::IoAndCleanup { cleanup, .. }
            | Self::SshAndCleanup { cleanup, .. }
            | Self::IoSshAndCleanup { cleanup, .. } => Some(cleanup),
            Self::Io(_) | Self::Ssh(_) | Self::IoAndSsh { .. } => None,
        }
    }

    fn with_io(self, io: ProxyIoFailure) -> Self {
        match self {
            Self::Ssh(ssh) => Self::IoAndSsh { io, ssh },
            Self::CleanupFailed(cleanup) => Self::IoAndCleanup { io, cleanup },
            Self::SshAndCleanup { ssh, cleanup } => Self::IoSshAndCleanup { io, ssh, cleanup },
            error @ (Self::Io(_)
            | Self::IoAndSsh { .. }
            | Self::IoAndCleanup { .. }
            | Self::IoSshAndCleanup { .. }) => error,
        }
    }

    fn with_cleanup(self, cleanup: ProxyCleanupFailure) -> Self {
        match self {
            Self::Io(io) => Self::IoAndCleanup { io, cleanup },
            Self::Ssh(ssh) => Self::SshAndCleanup { ssh, cleanup },
            Self::IoAndSsh { io, ssh } => Self::IoSshAndCleanup { io, ssh, cleanup },
            error @ (Self::CleanupFailed(_)
            | Self::IoAndCleanup { .. }
            | Self::SshAndCleanup { .. }
            | Self::IoSshAndCleanup { .. }) => error,
        }
    }

    fn as_io_error(self) -> io::Error {
        let kind = match self {
            Self::Io(failure)
            | Self::IoAndSsh { io: failure, .. }
            | Self::IoAndCleanup { io: failure, .. }
            | Self::IoSshAndCleanup { io: failure, .. } => failure.kind(),
            Self::Ssh(_) | Self::CleanupFailed(_) | Self::SshAndCleanup { .. } => ErrorKind::Other,
        };
        io::Error::new(kind, self)
    }
}

impl From<SshFailure> for ProxyStreamError {
    fn from(failure: SshFailure) -> Self {
        Self::Ssh(failure)
    }
}

type TerminalResult = Result<(), ProxyStreamError>;

#[derive(Clone)]
struct StreamPolicy {
    graceful_close_timeout: Duration,
    reap_timeout: Duration,
    pipe_drain_timeout: Duration,
    #[cfg(test)]
    readiness: Option<PathBuf>,
    #[cfg(test)]
    startup_gate: Option<PathBuf>,
}

impl StreamPolicy {
    fn production() -> Self {
        Self {
            graceful_close_timeout: GRACEFUL_CLOSE_TIMEOUT,
            reap_timeout: REAP_TIMEOUT,
            pipe_drain_timeout: PIPE_DRAIN_TIMEOUT,
            #[cfg(test)]
            readiness: None,
            #[cfg(test)]
            startup_gate: None,
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
            startup_gate: None,
        }
    }

    fn short_with_startup_gate(startup_gate: PathBuf) -> Self {
        Self {
            graceful_close_timeout: Duration::from_millis(150),
            reap_timeout: Duration::from_secs(5),
            pipe_drain_timeout: Duration::from_secs(5),
            readiness: None,
            startup_gate: Some(startup_gate),
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
    GracefulWait,
    Kill,
    Wait,
    Drain,
    OwnerPanic,
    OwnerCancel,
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
    fn injected_cleanup(self, stage: ProxyIoStage) -> Option<ProxyCleanupFailure> {
        #[cfg(test)]
        {
            let matches = matches!(
                (self.cleanup, stage),
                (
                    Some(TestCleanupFault::GracefulWait),
                    ProxyIoStage::GracefulWait
                ) | (Some(TestCleanupFault::Kill), ProxyIoStage::Kill)
                    | (Some(TestCleanupFault::Wait), ProxyIoStage::FinalReap)
                    | (Some(TestCleanupFault::Drain), ProxyIoStage::StderrDrain)
            );
            if matches {
                let error = io::Error::from_raw_os_error(32);
                Some(ProxyCleanupFailure::new(
                    cleanup_stage_for_io(stage),
                    Some(ProxyIoFailure::from_error(stage, &error)),
                ))
            } else {
                None
            }
        }
        #[cfg(not(test))]
        {
            let _ = stage;
            None
        }
    }

    fn owner_panics(self) -> bool {
        #[cfg(test)]
        {
            self.cleanup == Some(TestCleanupFault::OwnerPanic)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    #[cfg(test)]
    fn owner_cancels(self) -> bool {
        self.cleanup == Some(TestCleanupFault::OwnerCancel)
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

#[cfg(test)]
fn cleanup_stage_for_io(stage: ProxyIoStage) -> ProxyCleanupStage {
    match stage {
        ProxyIoStage::GracefulWait => ProxyCleanupStage::GracefulWait,
        ProxyIoStage::Kill => ProxyCleanupStage::Kill,
        ProxyIoStage::FinalReap => ProxyCleanupStage::FinalReap,
        ProxyIoStage::StderrDrain => ProxyCleanupStage::StderrDrain,
        ProxyIoStage::Spawn
        | ProxyIoStage::SetupStdin
        | ProxyIoStage::SetupStdout
        | ProxyIoStage::Read
        | ProxyIoStage::Write
        | ProxyIoStage::Flush
        | ProxyIoStage::Shutdown => ProxyCleanupStage::OwnerJoin,
    }
}

/// Direct asynchronous byte I/O over one owned OpenSSH child's pipes.
///
/// The child handle lives in one caller-runtime owner task. Dropping or
/// cancelling the stream closes stdin and signals that owner; explicit
/// `close` additionally awaits its bounded kill/reap/drain result.
pub struct ProxyStream {
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    cleanup: Option<oneshot::Sender<()>>,
    owner: Option<JoinHandle<TerminalResult>>,
    first_io_error: Option<ProxyIoFailure>,
    terminal_result: Option<TerminalResult>,
    shutdown_started: bool,
    #[cfg(test)]
    faults: StreamFaults,
}

impl ProxyStream {
    pub(super) async fn spawn(spec: CommandSpec) -> Result<Self, ProxyStreamError> {
        Self::spawn_inner(spec, StreamFaults::default(), StreamPolicy::production()).await
    }

    #[cfg(test)]
    async fn spawn_with_test_seams(
        spec: CommandSpec,
        faults: TestStreamFaults,
        policy: TestStreamPolicy,
    ) -> Result<Self, ProxyStreamError> {
        Self::spawn_inner(spec, faults, policy).await
    }

    async fn spawn_inner(
        spec: CommandSpec,
        faults: StreamFaults,
        policy: StreamPolicy,
    ) -> Result<Self, ProxyStreamError> {
        let mut command = tokio::process::Command::from(spec.to_command());
        drop(spec);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        let spawn_result = command.spawn();
        drop(command);
        let child = spawn_result.map_err(|error| {
            ProxyStreamError::Io(ProxyIoFailure::from_error(ProxyIoStage::Spawn, &error))
        })?;
        let mut child = OwnedChildGuard::new(child, policy.reap_timeout);
        let stdin = child.child_mut().stdin.take();
        let stdout = child.child_mut().stdout.take();
        let stderr = child.child_mut().stderr.take();
        let stderr_task =
            stderr.map(|stderr| tokio::spawn(capture_bounded(stderr, MAX_CAPTURED_STDERR_BYTES)));
        let mut stderr = StderrTaskGuard::new(stderr_task);

        wait_for_startup_gate(&policy).await;

        let setup_error = if stdin.is_none() {
            Some(ProxyIoFailure::from_error(
                ProxyIoStage::SetupStdin,
                &io::Error::other("SSH proxy stdin pipe was not available"),
            ))
        } else if stdout.is_none() || faults.missing_stdout() {
            Some(ProxyIoFailure::from_error(
                ProxyIoStage::SetupStdout,
                &io::Error::other("SSH proxy stdout pipe was not available"),
            ))
        } else {
            None
        };
        if let Some(setup_error) = setup_error {
            drop(stdin);
            drop(stdout);
            let outcome = terminate_owned_child(child.child_mut(), faults, &policy).await;
            if outcome.reaped {
                child.mark_reaped();
            }
            let stderr_result = finish_stderr(&mut stderr, faults, &policy).await;
            let cleanup = outcome.cleanup.or_else(|| stderr_result.err());
            return Err(match cleanup {
                Some(cleanup) => ProxyStreamError::Io(setup_error).with_cleanup(cleanup),
                None => ProxyStreamError::Io(setup_error),
            });
        }

        let (cleanup, cleanup_requested) = oneshot::channel();
        #[cfg(test)]
        let owner_cancel_readiness = policy.readiness.clone();
        let owner = tokio::spawn(own_child(child, stderr, cleanup_requested, faults, policy));
        #[cfg(test)]
        if faults.owner_cancels() {
            let abort = owner.abort_handle();
            tokio::spawn(async move {
                if let Some(path) = owner_cancel_readiness {
                    while !path.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                abort.abort();
            });
        }
        Ok(Self {
            stdin,
            stdout: stdout.expect("stdout pipe checked above"),
            cleanup: Some(cleanup),
            owner: Some(owner),
            first_io_error: None,
            terminal_result: None,
            shutdown_started: false,
            #[cfg(test)]
            faults,
        })
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

    fn record_io_error(&mut self, failure: ProxyIoFailure) {
        if self.first_io_error.is_none() {
            self.first_io_error = Some(failure);
        }
        self.signal_cleanup();
    }

    #[cfg(test)]
    fn inject_io_fault(&mut self, expected: TestIoFault, stage: ProxyIoStage) -> Option<io::Error> {
        if self.faults.io == Some(expected) {
            self.faults.io = None;
            let error = io::Error::from_raw_os_error(32);
            self.record_io_error(ProxyIoFailure::from_error(stage, &error));
            Some(error)
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
            if self
                .inject_io_fault(TestIoFault::Shutdown, ProxyIoStage::Shutdown)
                .is_some()
            {
                // The first pipe failure is retained while cleanup continues.
            } else if let Some(stdin) = self.stdin.as_mut() {
                match Pin::new(stdin).poll_shutdown(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => self.record_io_error(ProxyIoFailure::from_error(
                        ProxyIoStage::Shutdown,
                        &error,
                    )),
                    Poll::Ready(Ok(())) => self.signal_cleanup(),
                }
            } else {
                self.signal_cleanup();
            }
            #[cfg(not(test))]
            if let Some(stdin) = self.stdin.as_mut() {
                match Pin::new(stdin).poll_shutdown(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => self.record_io_error(ProxyIoFailure::from_error(
                        ProxyIoStage::Shutdown,
                        &error,
                    )),
                    Poll::Ready(Ok(())) => self.signal_cleanup(),
                }
            } else {
                self.signal_cleanup();
            }
        }

        let owner_result = match self.owner.as_mut() {
            Some(owner) => match Pin::new(owner).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(result)) => result,
                Poll::Ready(Err(_)) => Err(ProxyStreamError::CleanupFailed(
                    ProxyCleanupFailure::new(ProxyCleanupStage::OwnerJoin, None),
                )),
            },
            None => Err(ProxyStreamError::CleanupFailed(ProxyCleanupFailure::new(
                ProxyCleanupStage::OwnerJoin,
                None,
            ))),
        };
        self.owner.take();
        let result = match (self.first_io_error, owner_result) {
            (Some(io), Ok(())) => Err(ProxyStreamError::Io(io)),
            (Some(io), Err(error)) => Err(error.with_io(io)),
            (None, result) => result,
        };
        self.terminal_result = Some(result);
        Poll::Ready(result)
    }
}

impl AsyncRead for ProxyStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        #[cfg(test)]
        if let Some(error) = self.inject_io_fault(TestIoFault::Read, ProxyIoStage::Read) {
            return Poll::Ready(Err(error));
        }
        let filled_before = buffer.filled().len();
        let had_capacity = buffer.remaining() != 0;
        let result = Pin::new(&mut self.stdout).poll_read(cx, buffer);
        match &result {
            Poll::Ready(Err(error)) => {
                self.record_io_error(ProxyIoFailure::from_error(ProxyIoStage::Read, error))
            }
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
        if let Some(error) = self.inject_io_fault(TestIoFault::Write, ProxyIoStage::Write) {
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
            self.record_io_error(ProxyIoFailure::from_error(ProxyIoStage::Write, error));
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
        if let Some(error) = self.inject_io_fault(TestIoFault::Flush, ProxyIoStage::Flush) {
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
            self.record_io_error(ProxyIoFailure::from_error(ProxyIoStage::Flush, error));
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
        // Dropping a Tokio JoinHandle detaches the normal owner. If the
        // application runtime later cancels that owner, its child guard runs
        // the exceptional bounded reaper.
        self.owner.take();
    }
}

struct OwnedChildGuard {
    child: Option<Child>,
    fallback_reap_timeout: Duration,
}

impl OwnedChildGuard {
    fn new(child: Child, fallback_reap_timeout: Duration) -> Self {
        Self {
            child: Some(child),
            fallback_reap_timeout,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("owned child not yet reaped")
    }

    fn mark_reaped(&mut self) {
        self.child.take();
    }
}

impl Drop for OwnedChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            exceptional_reap(child, self.fallback_reap_timeout);
        }
    }
}

fn exceptional_reap(mut child: Child, reap_timeout: Duration) {
    #[cfg(test)]
    {
        EXCEPTIONAL_REAPER_STARTS.fetch_add(1, Ordering::SeqCst);
        ACTIVE_EXCEPTIONAL_REAPERS.fetch_add(1, Ordering::SeqCst);
    }
    struct ActiveReaper;
    impl Drop for ActiveReaper {
        fn drop(&mut self) {
            #[cfg(test)]
            ACTIVE_EXCEPTIONAL_REAPERS.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _active = ActiveReaper;

    let _ = child.start_kill();
    let deadline = Instant::now() + reap_timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) | Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => break,
        }
    }
}

struct StderrTaskGuard {
    task: Option<JoinHandle<io::Result<Vec<u8>>>>,
}

impl StderrTaskGuard {
    fn new(task: Option<JoinHandle<io::Result<Vec<u8>>>>) -> Self {
        Self { task }
    }
}

impl Drop for StderrTaskGuard {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn own_child(
    mut child: OwnedChildGuard,
    mut stderr: StderrTaskGuard,
    mut cleanup_requested: oneshot::Receiver<()>,
    faults: StreamFaults,
    policy: StreamPolicy,
) -> TerminalResult {
    #[cfg(test)]
    ACTIVE_OWNER_TASKS.fetch_add(1, Ordering::SeqCst);
    struct ActiveOwner;
    impl Drop for ActiveOwner {
        fn drop(&mut self) {
            #[cfg(test)]
            ACTIVE_OWNER_TASKS.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let _active = ActiveOwner;

    wait_for_test_readiness(&policy).await;
    assert!(!faults.owner_panics(), "synthetic proxy owner panic");

    enum Trigger {
        Natural(io::Result<ExitStatus>),
        Cleanup,
    }
    let trigger = tokio::select! {
        status = child.child_mut().wait() => Trigger::Natural(status),
        _ = &mut cleanup_requested => Trigger::Cleanup,
    };
    let outcome = match trigger {
        Trigger::Natural(status) => match status {
            Ok(status) => StopOutcome {
                status: Some(status),
                forced: false,
                reaped: true,
                cleanup: None,
            },
            Err(error) => {
                let initial = ProxyCleanupFailure::new(
                    ProxyCleanupStage::GracefulWait,
                    Some(ProxyIoFailure::from_error(
                        ProxyIoStage::GracefulWait,
                        &error,
                    )),
                );
                let mut outcome = terminate_owned_child(child.child_mut(), faults, &policy).await;
                outcome.cleanup = Some(initial);
                outcome
            }
        },
        Trigger::Cleanup => terminate_owned_child(child.child_mut(), faults, &policy).await,
    };
    if outcome.reaped {
        child.mark_reaped();
    }
    let stderr = finish_stderr(&mut stderr, faults, &policy).await;
    compose_terminal(outcome, stderr)
}

#[derive(Clone, Copy)]
struct StopOutcome {
    status: Option<ExitStatus>,
    forced: bool,
    reaped: bool,
    cleanup: Option<ProxyCleanupFailure>,
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
            reaped: true,
            cleanup: None,
        },
        first_wait => {
            let mut cleanup = match first_wait {
                Ok(Err(error)) => Some(ProxyCleanupFailure::new(
                    ProxyCleanupStage::GracefulWait,
                    Some(ProxyIoFailure::from_error(
                        ProxyIoStage::GracefulWait,
                        &error,
                    )),
                )),
                Ok(Ok(_)) | Err(_) => None,
            };
            if let Some(injected) = faults.injected_cleanup(ProxyIoStage::GracefulWait) {
                cleanup.get_or_insert(injected);
            }
            let kill_result = child.start_kill();
            if let Err(error) = kill_result {
                cleanup.get_or_insert_with(|| {
                    ProxyCleanupFailure::new(
                        ProxyCleanupStage::Kill,
                        Some(ProxyIoFailure::from_error(ProxyIoStage::Kill, &error)),
                    )
                });
            }
            if let Some(injected) = faults.injected_cleanup(ProxyIoStage::Kill) {
                cleanup.get_or_insert(injected);
            }
            let final_wait = timeout(policy.reap_timeout, child.wait()).await;
            let (status, reaped) = match final_wait {
                Ok(Ok(status)) => (Some(status), true),
                Ok(Err(error)) => {
                    cleanup.get_or_insert_with(|| {
                        ProxyCleanupFailure::new(
                            ProxyCleanupStage::FinalReap,
                            Some(ProxyIoFailure::from_error(ProxyIoStage::FinalReap, &error)),
                        )
                    });
                    (None, false)
                }
                Err(_) => {
                    cleanup.get_or_insert_with(|| {
                        ProxyCleanupFailure::new(ProxyCleanupStage::FinalReap, None)
                    });
                    (None, false)
                }
            };
            if let Some(injected) = faults.injected_cleanup(ProxyIoStage::FinalReap) {
                cleanup.get_or_insert(injected);
            }
            StopOutcome {
                status,
                forced: true,
                reaped,
                cleanup,
            }
        }
    }
}

async fn finish_stderr(
    stderr: &mut StderrTaskGuard,
    faults: StreamFaults,
    policy: &StreamPolicy,
) -> Result<Vec<u8>, ProxyCleanupFailure> {
    let result = match stderr.task.as_mut() {
        Some(task) => match timeout(policy.pipe_drain_timeout, &mut *task).await {
            Ok(Ok(Ok(bytes))) => Ok(bytes),
            Ok(Ok(Err(error))) => Err(ProxyCleanupFailure::new(
                ProxyCleanupStage::StderrDrain,
                Some(ProxyIoFailure::from_error(
                    ProxyIoStage::StderrDrain,
                    &error,
                )),
            )),
            Ok(Err(_)) | Err(_) => {
                task.abort();
                Err(ProxyCleanupFailure::new(
                    ProxyCleanupStage::StderrDrain,
                    None,
                ))
            }
        },
        None => Ok(Vec::new()),
    };
    stderr.task.take();
    if let Some(injected) = faults.injected_cleanup(ProxyIoStage::StderrDrain) {
        Err(injected)
    } else {
        result
    }
}

fn compose_terminal(
    outcome: StopOutcome,
    stderr: Result<Vec<u8>, ProxyCleanupFailure>,
) -> TerminalResult {
    let cleanup = outcome.cleanup.or_else(|| stderr.as_ref().err().copied());
    let process_result = match (outcome.status, outcome.forced, stderr.as_ref()) {
        (Some(_), true, _) => Ok(()),
        (Some(status), false, Ok(stderr)) => classify_status(status, stderr),
        (Some(status), false, Err(_)) => classify_status(status, &[]),
        (None, _, _) => Ok(()),
    };
    match (process_result, cleanup) {
        (Ok(()), None) => Ok(()),
        (Ok(()), Some(cleanup)) => Err(ProxyStreamError::CleanupFailed(cleanup)),
        (Err(error), None) => Err(error),
        (Err(error), Some(cleanup)) => Err(error.with_cleanup(cleanup)),
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
async fn wait_for_startup_gate(policy: &StreamPolicy) {
    if let Some(path) = &policy.startup_gate {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(not(test))]
async fn wait_for_startup_gate(_policy: &StreamPolicy) {}

#[cfg(test)]
fn active_owner_tasks() -> usize {
    ACTIVE_OWNER_TASKS.load(Ordering::SeqCst)
}

#[cfg(test)]
fn active_exceptional_reapers() -> usize {
    ACTIVE_EXCEPTIONAL_REAPERS.load(Ordering::SeqCst)
}

#[cfg(test)]
fn exceptional_reaper_starts() -> usize {
    EXCEPTIONAL_REAPER_STARTS.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{tempdir, TempDir};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        active_exceptional_reapers, active_owner_tasks, exceptional_reaper_starts,
        ProxyCleanupStage, ProxyIoStage, ProxyStream, ProxyStreamError, TestCleanupFault,
        TestIoFault, TestSetupFault, TestStreamFaults, TestStreamPolicy, MAX_CAPTURED_STDERR_BYTES,
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

    async fn spawn_test_stream(
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
        .await
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

    async fn assert_no_owner_or_exceptional_reaper() {
        tokio::time::timeout(Duration::from_secs(30), async {
            while active_owner_tasks() != 0 || active_exceptional_reapers() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("proxy owner or exceptional reaper remained active");
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
    async fn spawn_failure_retains_spawn_stage_and_raw_os_error() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let factory = SshCommandFactory::new_for_test(
            runtime.path().join("missing-ssh"),
            runtime.control_socket().to_owned(),
        );
        let ticket = ProxyTicket::generate();
        let spec = factory
            .proxy(&fixture_profile(), VmId::new(107).unwrap(), &ticket)
            .unwrap();
        let reaper_starts = exceptional_reaper_starts();

        let error = match ProxyStream::spawn(spec).await {
            Err(error) => error,
            Ok(_) => panic!("missing executable unexpectedly spawned"),
        };
        let io = error.io_failure().unwrap();
        assert_eq!(io.stage(), ProxyIoStage::Spawn);
        assert_eq!(io.kind(), io::ErrorKind::NotFound);
        assert!(io.raw_os_error().is_some());
        assert_eq!(exceptional_reaper_starts(), reaper_starts);
        assert_eq!(active_owner_tasks(), 0);
        assert_eq!(active_exceptional_reapers(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_spec_drops_before_startup_await_and_cancelled_startup_reaps_exact_child() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("hang_proxy"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let ticket = ProxyTicket::generate();
        let mut spec = factory
            .proxy(&fixture_profile(), VmId::new(107).unwrap(), &ticket)
            .unwrap();
        let spec_dropped = Arc::new(AtomicBool::new(false));
        spec.set_test_drop_probe(Arc::clone(&spec_dropped));
        let gate = runtime
            .control_socket()
            .with_extension("allow_proxy_startup");
        let reaper_starts = exceptional_reaper_starts();
        let startup = tokio::spawn(ProxyStream::spawn_with_test_seams(
            spec,
            TestStreamFaults::default(),
            TestStreamPolicy::short_with_startup_gate(gate),
        ));
        let pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&pid_path).await;
        let pid = helper_pid(&pid_path);

        assert!(spec_dropped.load(Ordering::SeqCst));
        assert!(!startup.is_finished());
        startup.abort();
        match startup.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("cancelled startup unexpectedly completed"),
        }
        assert_exact_pid_is_gone(pid).await;
        assert_eq!(exceptional_reaper_starts(), reaper_starts + 1);
        assert_eq!(active_owner_tasks(), 0);
        assert_eq!(active_exceptional_reapers(), 0);
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
        let reaper_starts = exceptional_reaper_starts();
        let mut stream = spawn_test_stream(&runtime, executable, TestStreamFaults::default())
            .await
            .unwrap();
        let pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&pid_path).await;
        let pid = helper_pid(&pid_path);

        let mut output = Vec::new();
        stream.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"RFB 003.008\n");
        stream.close().await.unwrap();
        assert_exact_pid_is_gone(pid).await;
        assert_eq!(exceptional_reaper_starts(), reaper_starts);
        assert_eq!(active_exceptional_reapers(), 0);
        assert_eq!(active_owner_tasks(), 0);
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
            let mut stream = spawn_test_stream(&runtime, executable, TestStreamFaults::io(fault))
                .await
                .unwrap();
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
            let io = first.io_failure().unwrap();
            let expected_stage = match fault {
                TestIoFault::Read => ProxyIoStage::Read,
                TestIoFault::Write => ProxyIoStage::Write,
                TestIoFault::Flush => ProxyIoStage::Flush,
                TestIoFault::Shutdown => ProxyIoStage::Shutdown,
            };
            assert_eq!(io.stage(), expected_stage);
            assert_eq!(io.kind(), io::ErrorKind::BrokenPipe);
            assert_eq!(io.raw_os_error(), Some(32));
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
        let mut stream = spawn_test_stream(&runtime, executable, TestStreamFaults::default())
            .await
            .unwrap();
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
    async fn graceful_wait_kill_final_wait_and_drain_faults_reap_with_typed_failure() {
        let _process_guard = crate::ssh::process_test_guard().await;
        for fault in [
            TestCleanupFault::GracefulWait,
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
                spawn_test_stream(&runtime, executable, TestStreamFaults::cleanup(fault))
                    .await
                    .unwrap();
            let pid_path = runtime.control_socket().with_extension("proxy.pid");
            wait_for(&pid_path).await;
            let pid = helper_pid(&pid_path);

            let first = stream.close().await.unwrap_err();
            let second = stream.close().await.unwrap_err();
            assert_eq!(first, second);
            assert!(first.has_cleanup_failure());
            assert!(first.has_io_failure());
            let expected_stage = match fault {
                TestCleanupFault::GracefulWait => ProxyIoStage::GracefulWait,
                TestCleanupFault::Kill => ProxyIoStage::Kill,
                TestCleanupFault::Wait => ProxyIoStage::FinalReap,
                TestCleanupFault::Drain => ProxyIoStage::StderrDrain,
                TestCleanupFault::OwnerPanic | TestCleanupFault::OwnerCancel => unreachable!(),
            };
            assert_eq!(first.io_failure().unwrap().stage(), expected_stage);
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
        .await
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
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("missing stdout setup unexpectedly succeeded"),
        };
        assert_eq!(
            error.io_failure().unwrap().stage(),
            ProxyIoStage::SetupStdout
        );
        let pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&pid_path).await;
        let pid = helper_pid(&pid_path);
        assert_exact_pid_is_gone(pid).await;
        assert_eq!(active_owner_tasks(), 0);
        assert_eq!(active_exceptional_reapers(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owner_panic_and_cancellation_return_finite_typed_cleanup_and_exceptionally_reap() {
        let _process_guard = crate::ssh::process_test_guard().await;
        for fault in [TestCleanupFault::OwnerPanic, TestCleanupFault::OwnerCancel] {
            let runtime = RuntimeDir::create().unwrap();
            let (_fixture_directory, executable) = fake_ssh();
            fs::write(
                runtime.control_socket().with_extension("hang_proxy"),
                b"synthetic fixture control\n",
            )
            .unwrap();
            let reaper_starts = exceptional_reaper_starts();
            let mut stream =
                spawn_test_stream(&runtime, executable, TestStreamFaults::cleanup(fault))
                    .await
                    .unwrap();
            let pid_path = runtime.control_socket().with_extension("proxy.pid");
            wait_for(&pid_path).await;
            let pid = helper_pid(&pid_path);

            let first = tokio::time::timeout(Duration::from_secs(30), stream.close())
                .await
                .unwrap()
                .unwrap_err();
            let second = stream.close().await.unwrap_err();
            assert_eq!(first, second);
            assert_eq!(
                first.cleanup_failure().unwrap().stage(),
                ProxyCleanupStage::OwnerJoin
            );
            assert_exact_pid_is_gone(pid).await;
            assert_eq!(exceptional_reaper_starts(), reaper_starts + 1);
            assert_eq!(active_owner_tasks(), 0);
            assert_eq!(active_exceptional_reapers(), 0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn active_runtime_stream_drop_uses_normal_owner_without_exceptional_reaper() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("hang_proxy"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let reaper_starts = exceptional_reaper_starts();
        let stream = spawn_test_stream(&runtime, executable, TestStreamFaults::default())
            .await
            .unwrap();
        let pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&pid_path).await;
        let pid = helper_pid(&pid_path);

        drop(stream);
        assert_exact_pid_is_gone(pid).await;
        assert_no_owner_or_exceptional_reaper().await;
        assert_eq!(exceptional_reaper_starts(), reaper_starts);
    }

    #[cfg(unix)]
    #[test]
    fn exceptional_reaper_handles_stream_dropped_after_runtime_shutdown() {
        let application_runtime = tokio::runtime::Runtime::new().unwrap();
        let _process_guard = application_runtime.block_on(crate::ssh::process_test_guard());
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("hang_proxy"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let reaper_starts = exceptional_reaper_starts();
        let stream = application_runtime.block_on(async {
            let stream = spawn_test_stream(&runtime, executable, TestStreamFaults::default())
                .await
                .unwrap();
            wait_for(&runtime.control_socket().with_extension("proxy.pid")).await;
            stream
        });
        let pid = helper_pid(&runtime.control_socket().with_extension("proxy.pid"));
        drop(application_runtime);
        drop(stream);

        assert_exact_pid_is_gone_blocking(pid);
        let started = Instant::now();
        while active_exceptional_reapers() != 0 || active_owner_tasks() != 0 {
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(exceptional_reaper_starts(), reaper_starts + 1);
    }

    #[cfg(unix)]
    #[test]
    fn exceptional_reaper_handles_stream_cancelled_by_runtime_teardown() {
        let application_runtime = tokio::runtime::Runtime::new().unwrap();
        let _process_guard = application_runtime.block_on(crate::ssh::process_test_guard());
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("hang_proxy"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let reaper_starts = exceptional_reaper_starts();
        application_runtime.block_on(async {
            let stream = spawn_test_stream(&runtime, executable, TestStreamFaults::default())
                .await
                .unwrap();
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
        while active_exceptional_reapers() != 0 || active_owner_tasks() != 0 {
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(exceptional_reaper_starts(), reaper_starts + 1);
    }
}
