use std::{io, net::Ipv4Addr, process::ExitStatus, time::Duration};

use tokio::{
    io::{copy_bidirectional, AsyncRead, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::{timeout, timeout_at, Instant},
};

use super::{password_file::VncPasswordFile, FallbackError, FallbackErrorKind, OwnedViewer};

const PRODUCTION_ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) struct RelayPolicy {
    pub(super) accept_timeout: Duration,
    pub(super) close_timeout: Duration,
    #[cfg(test)]
    pub(super) accepted: Option<oneshot::Sender<()>>,
}

impl RelayPolicy {
    pub(super) fn production(close_timeout: Duration) -> Self {
        Self {
            accept_timeout: PRODUCTION_ACCEPT_TIMEOUT,
            close_timeout,
            #[cfg(test)]
            accepted: None,
        }
    }
}

pub(super) async fn bind_loopback() -> io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await
}

enum AcceptOutcome {
    Accepted(io::Result<(TcpStream, std::net::SocketAddr)>),
    Viewer(io::Result<ExitStatus>),
    Cancelled(Instant),
    TimedOut,
}

enum RelayOutcome {
    Relay(io::Result<(u64, u64)>),
    Viewer(io::Result<ExitStatus>),
    Cancelled(Instant),
}

pub(super) async fn run<S>(
    listener: TcpListener,
    mut viewer: OwnedViewer,
    mut proxy: S,
    mut password_file: VncPasswordFile,
    mut cancelled: oneshot::Receiver<Instant>,
    policy: RelayPolicy,
) -> Result<(), FallbackError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let close_timeout = policy.close_timeout;
    #[cfg(test)]
    let mut accepted_signal = policy.accepted;
    let accept_outcome = {
        let accept = timeout(policy.accept_timeout, listener.accept());
        tokio::pin!(accept);
        tokio::select! {
            result = &mut accept => match result {
                Ok(accepted) => AcceptOutcome::Accepted(accepted),
                Err(_) => AcceptOutcome::TimedOut,
            },
            status = viewer.child.wait() => AcceptOutcome::Viewer(status),
            deadline = &mut cancelled => AcceptOutcome::Cancelled(
                deadline.unwrap_or_else(|_| Instant::now() + close_timeout)
            ),
        }
    };
    drop(listener);

    let (primary, deadline, viewer_reaped, client) = match accept_outcome {
        AcceptOutcome::Accepted(Ok((client, peer))) => {
            if !peer.ip().is_ipv4() || !peer.ip().is_loopback() {
                (
                    Err(FallbackError::new(FallbackErrorKind::PeerRejected)),
                    Instant::now() + close_timeout,
                    false,
                    Some(client),
                )
            } else if password_file.remove().is_err() {
                (
                    Err(FallbackError::new(FallbackErrorKind::PasswordFile).with_cleanup_failure()),
                    Instant::now() + close_timeout,
                    false,
                    Some(client),
                )
            } else {
                #[cfg(test)]
                if let Some(accepted) = accepted_signal.take() {
                    let _ = accepted.send(());
                }
                let mut client = client;
                let outcome = {
                    let relay = copy_bidirectional(&mut client, &mut proxy);
                    tokio::pin!(relay);
                    tokio::select! {
                        result = &mut relay => RelayOutcome::Relay(result),
                        status = viewer.child.wait() => RelayOutcome::Viewer(status),
                        deadline = &mut cancelled => RelayOutcome::Cancelled(
                            deadline.unwrap_or_else(|_| Instant::now() + close_timeout)
                        ),
                    }
                };
                match outcome {
                    RelayOutcome::Relay(Ok(_)) => {
                        (Ok(()), Instant::now() + close_timeout, false, Some(client))
                    }
                    RelayOutcome::Relay(Err(_)) => (
                        Err(FallbackError::new(FallbackErrorKind::Relay)),
                        Instant::now() + close_timeout,
                        false,
                        Some(client),
                    ),
                    RelayOutcome::Viewer(Ok(status)) if status.success() => {
                        (Ok(()), Instant::now() + close_timeout, true, Some(client))
                    }
                    RelayOutcome::Viewer(Ok(_)) => (
                        Err(FallbackError::new(FallbackErrorKind::ViewerExited)),
                        Instant::now() + close_timeout,
                        true,
                        Some(client),
                    ),
                    RelayOutcome::Viewer(Err(_)) => (
                        Err(FallbackError::new(FallbackErrorKind::ViewerExited)),
                        Instant::now() + close_timeout,
                        false,
                        Some(client),
                    ),
                    RelayOutcome::Cancelled(deadline) => (Ok(()), deadline, false, Some(client)),
                }
            }
        }
        AcceptOutcome::Accepted(Err(_)) => (
            Err(FallbackError::new(FallbackErrorKind::Accept)),
            Instant::now() + close_timeout,
            false,
            None,
        ),
        AcceptOutcome::Viewer(Ok(_)) => (
            Err(FallbackError::new(
                FallbackErrorKind::ViewerExitedBeforeConnect,
            )),
            Instant::now() + close_timeout,
            true,
            None,
        ),
        AcceptOutcome::Viewer(Err(_)) => (
            Err(FallbackError::new(
                FallbackErrorKind::ViewerExitedBeforeConnect,
            )),
            Instant::now() + close_timeout,
            false,
            None,
        ),
        AcceptOutcome::Cancelled(deadline) => (Ok(()), deadline, false, None),
        AcceptOutcome::TimedOut => (
            Err(FallbackError::new(FallbackErrorKind::AcceptTimedOut)),
            Instant::now() + close_timeout,
            false,
            None,
        ),
    };

    drop(client);
    finish_cleanup(
        primary,
        deadline,
        viewer_reaped,
        &mut viewer,
        &mut proxy,
        &mut password_file,
    )
    .await
}

async fn finish_cleanup<S>(
    primary: Result<(), FallbackError>,
    deadline: Instant,
    viewer_reaped: bool,
    viewer: &mut OwnedViewer,
    proxy: &mut S,
    password_file: &mut VncPasswordFile,
) -> Result<(), FallbackError>
where
    S: AsyncWrite + Unpin,
{
    let password_result = password_file.remove();
    let viewer_cleanup = async {
        if viewer_reaped {
            Ok(())
        } else {
            stop_viewer(&mut viewer.child, deadline).await
        }
    };
    let proxy_cleanup = close_proxy(proxy, deadline);
    let (viewer_result, proxy_result) = tokio::join!(viewer_cleanup, proxy_cleanup);
    let snapshot_result = if viewer_result.is_ok() {
        viewer.snapshot.remove()
    } else {
        Ok(())
    };
    let cleanup_failed = password_result.is_err()
        || viewer_result.is_err()
        || snapshot_result.is_err()
        || proxy_result.is_err();
    match (primary, cleanup_failed) {
        (Err(error), true) => Err(error.with_cleanup_failure()),
        (Err(error), false) => Err(error),
        (Ok(()), true) => Err(FallbackError::new(FallbackErrorKind::Cleanup)),
        (Ok(()), false) => Ok(()),
    }
}

async fn stop_viewer(viewer: &mut tokio::process::Child, deadline: Instant) -> Result<(), ()> {
    match viewer.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(_) => return Err(()),
    }
    if viewer.start_kill().is_err() {
        return Err(());
    }
    match timeout_at(deadline, viewer.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(()),
    }
}

pub(super) async fn close_proxy<S>(proxy: &mut S, deadline: Instant) -> Result<(), ()>
where
    S: AsyncWrite + Unpin,
{
    match timeout_at(deadline, proxy.shutdown()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(()),
    }
}
