use std::{
    future::Future,
    io,
    pin::Pin,
    process::{ExitStatus, Stdio},
    task::{Context, Poll},
    time::Duration,
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

#[derive(Debug, Error)]
pub enum ProxyStreamError {
    #[error("could not run the owned SSH proxy process: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Ssh(#[from] SshFailure),
    #[error("owned SSH proxy child cleanup failed")]
    CleanupFailed,
}

/// Direct asynchronous byte I/O over one owned OpenSSH child's pipes.
///
/// The child handle lives in the single owner task. Dropping or cancelling the
/// stream closes stdin and signals that task; explicit `close` additionally
/// awaits the same bounded kill/reap/drain path.
pub struct ProxyStream {
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    cleanup: Option<oneshot::Sender<()>>,
    owner: Option<JoinHandle<Result<(), ProxyStreamError>>>,
    shutdown_started: bool,
    closed: bool,
}

impl ProxyStream {
    pub(super) fn spawn(spec: CommandSpec) -> Result<Self, ProxyStreamError> {
        let mut command = tokio::process::Command::from(spec.to_command());
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("SSH proxy stdin pipe was not available"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("SSH proxy stdout pipe was not available"))?;
        let stderr_task = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(capture_bounded(stderr, MAX_CAPTURED_STDERR_BYTES)));
        let (cleanup, cleanup_requested) = oneshot::channel();
        let owner = tokio::spawn(own_child(child, stderr_task, cleanup_requested));

        Ok(Self {
            stdin: Some(stdin),
            stdout,
            cleanup: Some(cleanup),
            owner: Some(owner),
            shutdown_started: false,
            closed: false,
        })
    }

    pub async fn close(&mut self) -> Result<(), ProxyStreamError> {
        tokio::io::AsyncWriteExt::shutdown(self)
            .await
            .map_err(ProxyStreamError::Io)
    }

    fn signal_cleanup(&mut self) {
        self.stdin.take();
        if let Some(cleanup) = self.cleanup.take() {
            let _ = cleanup.send(());
        }
        self.shutdown_started = true;
    }
}

impl AsyncRead for ProxyStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.stdout).poll_read(cx, buffer);
        if matches!(result, Poll::Ready(Err(_))) {
            self.signal_cleanup();
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
        let result = match self.stdin.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_write(cx, buffer),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SSH proxy stdin is unavailable",
            ))),
        };
        if matches!(result, Poll::Ready(Err(_))) {
            self.signal_cleanup();
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.shutdown_started {
            return Poll::Ready(Ok(()));
        }
        match self.stdin.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.closed {
            return Poll::Ready(Ok(()));
        }

        if !self.shutdown_started {
            if let Some(stdin) = self.stdin.as_mut() {
                match Pin::new(stdin).poll_shutdown(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => {
                        self.signal_cleanup();
                        return Poll::Ready(Err(error));
                    }
                    Poll::Ready(Ok(())) => {}
                }
            }
            self.signal_cleanup();
        }

        let Some(owner) = self.owner.as_mut() else {
            self.closed = true;
            return Poll::Ready(Ok(()));
        };
        match Pin::new(owner).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.owner.take();
                self.closed = true;
                match result {
                    Ok(Ok(())) => Poll::Ready(Ok(())),
                    Ok(Err(error)) => Poll::Ready(Err(io::Error::other(error))),
                    Err(_) => Poll::Ready(Err(io::Error::other(
                        "owned SSH proxy cleanup task stopped",
                    ))),
                }
            }
        }
    }
}

impl Drop for ProxyStream {
    fn drop(&mut self) {
        self.signal_cleanup();
        // Dropping a Tokio JoinHandle detaches rather than cancels it. The
        // owner task therefore keeps the Child until bounded cleanup finishes.
        self.owner.take();
    }
}

async fn own_child(
    mut child: Child,
    stderr_task: Option<JoinHandle<io::Result<Vec<u8>>>>,
    mut cleanup_requested: oneshot::Receiver<()>,
) -> Result<(), ProxyStreamError> {
    tokio::select! {
        status = child.wait() => {
            let status = status?;
            let stderr = finish_stderr(stderr_task).await?;
            classify_status(status, &stderr)
        }
        _ = &mut cleanup_requested => {
            stop_owned_child(&mut child).await?;
            let _bounded_stderr = finish_stderr(stderr_task).await?;
            Ok(())
        }
    }
}

async fn stop_owned_child(child: &mut Child) -> Result<(), ProxyStreamError> {
    match timeout(GRACEFUL_CLOSE_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) | Err(_) => {
            child
                .start_kill()
                .map_err(|_| ProxyStreamError::CleanupFailed)?;
            match timeout(REAP_TIMEOUT, child.wait()).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(_)) | Err(_) => Err(ProxyStreamError::CleanupFailed),
            }
        }
    }
}

async fn finish_stderr(
    mut task: Option<JoinHandle<io::Result<Vec<u8>>>>,
) -> Result<Vec<u8>, ProxyStreamError> {
    match task.as_mut() {
        Some(task) => match timeout(PIPE_DRAIN_TIMEOUT, &mut *task).await {
            Ok(Ok(Ok(stderr))) => Ok(stderr),
            Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => {
                task.abort();
                Err(ProxyStreamError::CleanupFailed)
            }
        },
        None => Ok(Vec::new()),
    }
}

fn classify_status(status: ExitStatus, stderr: &[u8]) -> Result<(), ProxyStreamError> {
    if status.success() {
        Ok(())
    } else {
        Err(classify_stderr(stderr).into())
    }
}
