use std::{future::Future, io, net::Ipv4Addr, process::ExitStatus, time::Duration};

use tokio::{
    io::{copy_bidirectional, AsyncRead, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::{timeout_at, Instant},
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

#[derive(Debug, Eq, PartialEq)]
enum PreConnectRace<A, W> {
    Accepted(A),
    Viewer(W),
    Cancelled(Instant),
    TimedOut,
}

async fn race_pre_connect<A, W>(
    accept: impl Future<Output = A>,
    wait: impl Future<Output = W>,
    cancelled: &mut oneshot::Receiver<Instant>,
    accept_timeout: Duration,
    close_timeout: Duration,
) -> PreConnectRace<A, W> {
    tokio::select! {
        biased;
        result = accept => PreConnectRace::Accepted(result),
        status = wait => PreConnectRace::Viewer(status),
        deadline = cancelled => PreConnectRace::Cancelled(
            deadline.unwrap_or_else(|_| Instant::now() + close_timeout)
        ),
        () = tokio::time::sleep(accept_timeout) => PreConnectRace::TimedOut,
    }
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
    let accept_outcome = match race_pre_connect(
        listener.accept(),
        viewer.child.wait(),
        &mut cancelled,
        policy.accept_timeout,
        close_timeout,
    )
    .await
    {
        PreConnectRace::Accepted(accepted) => AcceptOutcome::Accepted(accepted),
        PreConnectRace::Viewer(status) => AcceptOutcome::Viewer(status),
        PreConnectRace::Cancelled(deadline) => AcceptOutcome::Cancelled(deadline),
        PreConnectRace::TimedOut => AcceptOutcome::TimedOut,
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
    let password_cleanup_failed = password_result.is_err();
    let cleanup_failed = password_cleanup_failed
        || viewer_result.is_err()
        || snapshot_result.is_err()
        || proxy_result.is_err();
    match (primary, cleanup_failed) {
        (Err(error), true) => Err(error.with_cleanup_failure()),
        (Err(error), false) => Err(error),
        (Ok(()), true) if password_cleanup_failed => {
            Err(FallbackError::new(FallbackErrorKind::PasswordFile).with_cleanup_failure())
        }
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

#[cfg(test)]
mod pre_connect_race_tests {
    use super::*;
    use std::time::Duration;

    async fn register_deadline<T>(race: &mut std::pin::Pin<&mut impl Future<Output = T>>) {
        tokio::select! {
            biased;
            _ = race.as_mut() => panic!("deadline must register while every arm is pending"),
            () = std::future::ready(()) => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ready_accept_wins_over_ready_viewer_after_elapsed_timeout() {
        let (accept_tx, accept_rx) = oneshot::channel::<u8>();
        let (wait_tx, wait_rx) = oneshot::channel::<u8>();
        let (_cancel_tx, mut cancel_rx) = oneshot::channel::<Instant>();
        let race = race_pre_connect(
            async { accept_rx.await.expect("accept sender dropped") },
            async { wait_rx.await.expect("wait sender dropped") },
            &mut cancel_rx,
            Duration::from_millis(5),
            Duration::from_secs(1),
        );
        tokio::pin!(race);
        register_deadline(&mut race).await;
        tokio::time::advance(Duration::from_millis(5)).await;
        accept_tx.send(1).expect("accept receiver live");
        wait_tx.send(2).expect("wait receiver live");
        assert_eq!(race.await, PreConnectRace::Accepted(1));
    }

    #[tokio::test(start_paused = true)]
    async fn ready_viewer_wins_over_elapsed_timeout_when_accept_stays_pending() {
        let (_accept_tx, accept_rx) = oneshot::channel::<u8>();
        let (wait_tx, wait_rx) = oneshot::channel::<u8>();
        let (_cancel_tx, mut cancel_rx) = oneshot::channel::<Instant>();
        let race = race_pre_connect(
            async { accept_rx.await.expect("accept sender dropped") },
            async { wait_rx.await.expect("wait sender dropped") },
            &mut cancel_rx,
            Duration::from_millis(5),
            Duration::from_secs(1),
        );
        tokio::pin!(race);
        register_deadline(&mut race).await;
        tokio::time::advance(Duration::from_millis(5)).await;
        wait_tx.send(2).expect("wait receiver live");
        assert_eq!(race.await, PreConnectRace::Viewer(2));
    }
}
