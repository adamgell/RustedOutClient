use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};

use crate::{
    cache::{CacheError, InventoryCache},
    config::AppConfig,
    connection::{bounded_vnc_channels, VncConnection, VncEvent},
    model::{PveProfile, VmId},
    runtime::RuntimeDir,
    ssh::{
        InventoryError, InventorySnapshot, ProxyOpenError, SshCommandFactory, SshFailureKind,
        SshMaster, SshMasterError, TrustedSshProxy,
    },
    vnc::{RfbError, RfbErrorKind, RfbPhase, VncClient},
};

use super::{
    AppCommand, AppEvent, InputAction, OpenOptions, PublicError, PublicErrorKind, SessionId,
    SessionPhase, SessionSnapshot, SessionTransportEvent,
};

pub const APP_QUEUE_CAPACITY: usize = 256;
const MAX_ACTIVE_NATIVE_SESSIONS: usize = 2;
const SESSION_POLL_INTERVAL: Duration = Duration::from_millis(1);

pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait ManagedSession: Send + 'static {
    fn try_recv(&mut self) -> Result<Option<SessionTransportEvent>, PublicError>;
    fn send_input(&mut self, action: InputAction) -> Result<(), PublicError>;
    fn release_all_keys(&mut self) -> Result<(), PublicError>;
    fn close(&mut self) -> BackendFuture<'_, Result<(), PublicError>>;
}

pub trait SessionBackend: Send + 'static {
    type Session: ManagedSession;

    fn load_cache(&mut self) -> BackendFuture<'_, Result<Option<InventorySnapshot>, PublicError>>;
    fn start_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>>;
    fn fetch_inventory(&mut self) -> BackendFuture<'_, Result<InventorySnapshot, PublicError>>;
    fn save_cache(
        &mut self,
        snapshot: &InventorySnapshot,
    ) -> BackendFuture<'_, Result<(), PublicError>>;
    fn open_session(
        &mut self,
        vmid: VmId,
        options: OpenOptions,
    ) -> BackendFuture<'_, Result<Self::Session, PublicError>>;
    fn close_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>>;
}

pub struct SessionManager {
    command_tx: mpsc::Sender<AppCommand>,
    event_rx: mpsc::Receiver<AppEvent>,
    worker: Option<JoinHandle<Result<(), PublicError>>>,
}

impl SessionManager {
    pub fn spawn<B>(config: AppConfig, backend: B) -> Self
    where
        B: SessionBackend,
    {
        let (command_tx, command_rx) = mpsc::channel(APP_QUEUE_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(APP_QUEUE_CAPACITY);
        let worker = tokio::spawn(
            Worker {
                profile_name: config.profile.name,
                refresh_interval: Duration::from_secs(config.inventory_refresh_seconds),
                backend,
                command_rx,
                event_tx,
                sessions: Vec::new(),
            }
            .run(),
        );
        Self {
            command_tx,
            event_rx,
            worker: Some(worker),
        }
    }

    pub fn spawn_production(config: AppConfig, cache_path: PathBuf) -> Result<Self, PublicError> {
        let backend = ProductionBackend::new(config.profile.clone(), cache_path)?;
        Ok(Self::spawn(config, backend))
    }

    pub async fn send(
        &self,
        command: AppCommand,
    ) -> Result<(), mpsc::error::SendError<AppCommand>> {
        self.command_tx.send(command).await
    }

    pub async fn recv(&mut self) -> Option<AppEvent> {
        self.event_rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<AppEvent, mpsc::error::TryRecvError> {
        self.event_rx.try_recv()
    }

    pub fn queued_event_count(&self) -> usize {
        self.event_rx.len()
    }

    pub fn command_capacity(&self) -> usize {
        self.command_tx.max_capacity()
    }

    pub fn event_capacity(&self) -> usize {
        self.event_rx.max_capacity()
    }

    pub async fn shutdown(mut self) -> Result<(), PublicError> {
        let shutdown = self.command_tx.send(AppCommand::Shutdown);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                result = &mut shutdown => {
                    let _ = result;
                    break;
                }
                _ = self.event_rx.recv() => {}
            }
        }
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let mut worker = worker;
        loop {
            tokio::select! {
                result = &mut worker => {
                    return result
                        .map_err(|_| PublicError::new(PublicErrorKind::Cleanup))?;
                }
                _ = self.event_rx.recv() => {}
            }
        }
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        let _ = self.command_tx.try_send(AppCommand::Shutdown);
    }
}

struct SessionRecord<S> {
    snapshot: SessionSnapshot,
    options: OpenOptions,
    session: Option<S>,
    error_emitted: bool,
}

struct Worker<B>
where
    B: SessionBackend,
{
    profile_name: String,
    refresh_interval: Duration,
    backend: B,
    command_rx: mpsc::Receiver<AppCommand>,
    event_tx: mpsc::Sender<AppEvent>,
    sessions: Vec<SessionRecord<B::Session>>,
}

impl<B> Worker<B>
where
    B: SessionBackend,
{
    async fn run(mut self) -> Result<(), PublicError> {
        self.startup().await;
        let first_refresh = Instant::now() + self.refresh_interval;
        let mut refresh = tokio::time::interval_at(first_refresh, self.refresh_interval);
        refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut session_poll = tokio::time::interval(SESSION_POLL_INTERVAL);
        session_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    match command {
                        Some(AppCommand::Shutdown) | None => return self.shutdown().await,
                        Some(command) => self.handle_command(command).await,
                    }
                }
                _ = refresh.tick() => self.refresh_inventory().await,
                _ = session_poll.tick() => self.poll_sessions().await,
            }
        }
    }

    async fn startup(&mut self) {
        match self.backend.load_cache().await {
            Ok(Some(mut snapshot)) => {
                snapshot.stale = true;
                self.emit_critical(AppEvent::CachedInventory(snapshot))
                    .await;
            }
            Ok(None) => {}
            Err(error) => self.emit_critical(AppEvent::Error(error)).await,
        }

        match self.backend.start_master().await {
            Ok(()) => self.refresh_inventory().await,
            Err(error) => self.emit_critical(AppEvent::Error(error)).await,
        }
    }

    async fn refresh_inventory(&mut self) {
        match self.backend.fetch_inventory().await {
            Ok(mut snapshot) => {
                snapshot.stale = false;
                if let Err(error) = self.backend.save_cache(&snapshot).await {
                    self.emit_critical(AppEvent::Error(error)).await;
                }
                self.emit_critical(AppEvent::LiveInventory(snapshot)).await;
            }
            Err(error) => self.emit_critical(AppEvent::Error(error)).await,
        }
    }

    async fn handle_command(&mut self, command: AppCommand) {
        match command {
            AppCommand::RefreshInventory => self.refresh_inventory().await,
            AppCommand::Open { vmid, options } => self.open(vmid, options).await,
            AppCommand::Reconnect { session_id } => self.reconnect(session_id).await,
            AppCommand::Close { session_id } => {
                if let Some(index) = self.session_index(session_id) {
                    let _ = self.close_session(index, None).await;
                } else {
                    self.emit_critical(AppEvent::Error(PublicError::new(
                        PublicErrorKind::VmNotFound,
                    )))
                    .await;
                }
            }
            AppCommand::SendInput { session_id, action } => {
                self.send_input(session_id, action).await
            }
            AppCommand::Shutdown => {}
        }
    }

    async fn open(&mut self, vmid: VmId, options: OpenOptions) {
        let existing_session_id = self.sessions.iter().find_map(|record| {
            (record.snapshot.vmid == vmid && record.snapshot.phase != SessionPhase::Disconnected)
                .then_some(record.snapshot.session_id)
        });
        if let Some(session_id) = existing_session_id {
            self.emit_critical(AppEvent::FocusExisting { session_id })
                .await;
            return;
        }
        if self.active_session_count() >= MAX_ACTIVE_NATIVE_SESSIONS {
            self.emit_critical(AppEvent::Error(PublicError::new(PublicErrorKind::Capacity)))
                .await;
            return;
        }

        let session_id = SessionId::new();
        let snapshot = SessionSnapshot::opening(session_id, self.profile_name.clone(), vmid);
        self.sessions.push(SessionRecord {
            snapshot: snapshot.clone(),
            options,
            session: None,
            error_emitted: false,
        });
        let index = self.sessions.len() - 1;
        self.emit_critical(AppEvent::SessionChanged(snapshot)).await;
        self.transition(index, SessionPhase::StartingProxy).await;

        match self.backend.open_session(vmid, options).await {
            Ok(session) => {
                self.sessions[index].session = Some(session);
                self.transition(index, SessionPhase::NegotiatingRfb).await;
            }
            Err(error) => {
                let contextual = error.for_session(session_id, vmid);
                let _ = self.close_session(index, Some(contextual)).await;
            }
        }
    }

    async fn reconnect(&mut self, session_id: SessionId) {
        let Some(index) = self.session_index(session_id) else {
            self.emit_critical(AppEvent::Error(PublicError::new(
                PublicErrorKind::VmNotFound,
            )))
            .await;
            return;
        };
        let vmid = self.sessions[index].snapshot.vmid;
        let options = self.sessions[index].options;
        if self.close_session(index, None).await.is_ok() {
            self.open(vmid, options).await;
        }
    }

    async fn send_input(&mut self, session_id: SessionId, action: InputAction) {
        let Some(index) = self.session_index(session_id) else {
            self.emit_critical(AppEvent::Error(PublicError::new(
                PublicErrorKind::VmNotFound,
            )))
            .await;
            return;
        };
        let vmid = self.sessions[index].snapshot.vmid;
        let result = self.sessions[index]
            .session
            .as_mut()
            .ok_or_else(|| PublicError::new(PublicErrorKind::Queue))
            .and_then(|session| session.send_input(action));
        if let Err(error) = result {
            let contextual = error.for_session(session_id, vmid);
            let _ = self.close_session(index, Some(contextual)).await;
        }
    }

    async fn poll_sessions(&mut self) {
        let mut pending = Vec::new();
        for (index, record) in self.sessions.iter_mut().enumerate() {
            if let Some(session) = record.session.as_mut() {
                match session.try_recv() {
                    Ok(Some(event)) => pending.push((index, event)),
                    Ok(None) => {}
                    Err(error) => pending.push((index, SessionTransportEvent::Error(error))),
                }
            }
        }

        for (index, event) in pending {
            match event {
                SessionTransportEvent::Framebuffer(rects) => {
                    if rects.is_empty() {
                        continue;
                    }
                    if self.sessions[index].snapshot.phase == SessionPhase::NegotiatingRfb {
                        self.transition(index, SessionPhase::Ready).await;
                    }
                    let session_id = self.sessions[index].snapshot.session_id;
                    self.emit_framebuffer(AppEvent::Framebuffer { session_id, rects });
                }
                SessionTransportEvent::Error(error) => {
                    let snapshot = &self.sessions[index].snapshot;
                    let contextual = error.for_session(snapshot.session_id, snapshot.vmid);
                    let _ = self.close_session(index, Some(contextual)).await;
                }
                SessionTransportEvent::Disconnected => {
                    let _ = self.close_session(index, None).await;
                }
            }
        }
    }

    async fn close_session(
        &mut self,
        index: usize,
        primary_error: Option<PublicError>,
    ) -> Result<(), PublicError> {
        if self.sessions[index].snapshot.phase == SessionPhase::Disconnected {
            return Ok(());
        }
        if self.sessions[index].snapshot.phase != SessionPhase::Disconnecting {
            self.transition(index, SessionPhase::Disconnecting).await;
        }

        let close_result = if let Some(mut session) = self.sessions[index].session.take() {
            let release_result = session.release_all_keys();
            let close_result = session.close().await;
            release_result.and(close_result)
        } else {
            Ok(())
        };
        let error = primary_error.or_else(|| close_result.err());
        if let Some(error) = error {
            if !self.sessions[index].error_emitted {
                self.sessions[index].error_emitted = true;
                self.emit_critical(AppEvent::Error(error)).await;
            }
        }
        self.transition(index, SessionPhase::Disconnected).await;
        close_result
    }

    async fn transition(&mut self, index: usize, phase: SessionPhase) {
        if self.sessions[index].snapshot.transition_to(phase).is_err() {
            let snapshot = &self.sessions[index].snapshot;
            let error = PublicError::new(PublicErrorKind::Cleanup)
                .for_session(snapshot.session_id, snapshot.vmid);
            if !self.sessions[index].error_emitted {
                self.sessions[index].error_emitted = true;
                self.emit_critical(AppEvent::Error(error)).await;
            }
            return;
        }
        let snapshot = self.sessions[index].snapshot.clone();
        self.emit_critical(AppEvent::SessionChanged(snapshot)).await;
    }

    async fn shutdown(&mut self) -> Result<(), PublicError> {
        let mut first_error = None;
        for index in 0..self.sessions.len() {
            if let Err(error) = self.close_session(index, None).await {
                first_error.get_or_insert(error);
            }
        }
        if let Err(error) = self.backend.close_master().await {
            self.emit_critical(AppEvent::Error(error)).await;
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn active_session_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|record| record.snapshot.phase != SessionPhase::Disconnected)
            .count()
    }

    fn session_index(&self, session_id: SessionId) -> Option<usize> {
        self.sessions
            .iter()
            .position(|record| record.snapshot.session_id == session_id)
    }

    async fn emit_critical(&mut self, event: AppEvent) {
        let _ = self.event_tx.send(event).await;
    }

    fn emit_framebuffer(&self, event: AppEvent) {
        let _ = self.event_tx.try_send(event);
    }
}

pub struct ProductionBackend {
    profile: PveProfile,
    cache: InventoryCache,
    runtime: RuntimeDir,
    master: Option<SshMaster>,
}

impl ProductionBackend {
    pub fn new(profile: PveProfile, cache_path: PathBuf) -> Result<Self, PublicError> {
        let runtime =
            RuntimeDir::create().map_err(|_| PublicError::new(PublicErrorKind::SshUnavailable))?;
        Ok(Self {
            profile,
            cache: InventoryCache::new(cache_path),
            runtime,
            master: None,
        })
    }
}

impl SessionBackend for ProductionBackend {
    type Session = ProductionSession;

    fn load_cache(&mut self) -> BackendFuture<'_, Result<Option<InventorySnapshot>, PublicError>> {
        Box::pin(async move { self.cache.load_optional().map_err(public_cache_error) })
    }

    fn start_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
        Box::pin(async move {
            if let Some(master) = self.master.as_mut() {
                return master
                    .verify()
                    .await
                    .map(|_| ())
                    .map_err(public_master_error);
            }
            let factory = SshCommandFactory::new(self.runtime.control_socket().to_owned());
            let mut master = SshMaster::start(factory, self.profile.clone())
                .await
                .map_err(public_master_error)?;
            if let Err(error) = master.verify().await {
                let primary = public_master_error(error);
                if master.close().await.is_err() {
                    return Err(PublicError::new(PublicErrorKind::Cleanup));
                }
                return Err(primary);
            }
            self.master = Some(master);
            Ok(())
        })
    }

    fn fetch_inventory(&mut self) -> BackendFuture<'_, Result<InventorySnapshot, PublicError>> {
        Box::pin(async move {
            let master = self
                .master
                .as_mut()
                .ok_or_else(|| PublicError::new(PublicErrorKind::SshUnavailable))?;
            let mut verified = master.verify().await.map_err(public_master_error)?;
            verified
                .fetch_inventory()
                .await
                .map_err(public_inventory_error)
        })
    }

    fn save_cache(
        &mut self,
        snapshot: &InventorySnapshot,
    ) -> BackendFuture<'_, Result<(), PublicError>> {
        let result = self.cache.save(snapshot).map_err(public_cache_error);
        Box::pin(async move { result })
    }

    fn open_session(
        &mut self,
        vmid: VmId,
        options: OpenOptions,
    ) -> BackendFuture<'_, Result<Self::Session, PublicError>> {
        Box::pin(async move {
            let master = self
                .master
                .as_mut()
                .ok_or_else(|| PublicError::new(PublicErrorKind::SshUnavailable))?;
            let mut verified = master.verify().await.map_err(public_master_error)?;
            let proxy = TrustedSshProxy::connect(&mut verified, vmid)
                .await
                .map_err(public_proxy_error)?;
            Ok(ProductionSession::spawn(proxy, options))
        })
    }

    fn close_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
        Box::pin(async move {
            match self.master.as_mut() {
                Some(master) => master.close().await.map_err(public_master_error),
                None => Ok(()),
            }
        })
    }
}

pub struct ProductionSession {
    connection: VncConnection,
    task: Option<JoinHandle<()>>,
    terminal: Arc<Mutex<Option<Result<(), PublicError>>>>,
    terminal_reported: bool,
    cancel: Option<oneshot::Sender<()>>,
}

impl ProductionSession {
    fn spawn(proxy: TrustedSshProxy, options: OpenOptions) -> Self {
        let (connection, channels) = bounded_vnc_channels();
        let terminal = Arc::new(Mutex::new(None));
        let task_terminal = terminal.clone();
        let terminal_sender = channels.event_tx.clone();
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = VncClient::run_cancellable(
                proxy,
                options.vnc,
                channels.event_tx,
                channels.command_rx,
                cancelled,
            )
            .await
            .map_err(public_rfb_error);
            *task_terminal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
            drop(terminal_sender);
        });
        Self {
            connection,
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        }
    }

    fn take_terminal_event(&mut self) -> Option<SessionTransportEvent> {
        if self.terminal_reported {
            return None;
        }
        let terminal = *self
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        terminal.map(|result| {
            self.terminal_reported = true;
            match result {
                Ok(()) => SessionTransportEvent::Disconnected,
                Err(error) => SessionTransportEvent::Error(error),
            }
        })
    }
}

impl ManagedSession for ProductionSession {
    fn try_recv(&mut self) -> Result<Option<SessionTransportEvent>, PublicError> {
        loop {
            match self.connection.event_rx.try_recv() {
                Ok(VncEvent::FramebufferRects(rects)) => {
                    return Ok(Some(SessionTransportEvent::Framebuffer(rects)));
                }
                Ok(VncEvent::Error(error)) => {
                    self.terminal_reported = true;
                    return Ok(Some(SessionTransportEvent::Error(public_rfb_error(error))));
                }
                Ok(VncEvent::Disconnected) => {
                    self.terminal_reported = true;
                    return Ok(Some(SessionTransportEvent::Disconnected));
                }
                Ok(
                    VncEvent::DesktopSize(_, _)
                    | VncEvent::DesktopName(_)
                    | VncEvent::ClipboardText(_),
                ) => {}
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    return Ok(self.take_terminal_event());
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    if let Some(event) = self.take_terminal_event() {
                        return Ok(Some(event));
                    }
                    if self.task.as_ref().is_some_and(JoinHandle::is_finished) {
                        self.terminal_reported = true;
                        return Ok(Some(SessionTransportEvent::Error(PublicError::new(
                            PublicErrorKind::Cleanup,
                        ))));
                    }
                    return Ok(None);
                }
            }
        }
    }

    fn send_input(&mut self, action: InputAction) -> Result<(), PublicError> {
        let InputAction::Forward(command) = action;
        self.connection
            .command_tx
            .try_send(command)
            .map_err(|_| PublicError::new(PublicErrorKind::Queue))
    }

    fn release_all_keys(&mut self) -> Result<(), PublicError> {
        // Task 10 replaces this compile-safe lifecycle seam with tracked-key release.
        Ok(())
    }

    fn close(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
        Box::pin(async move {
            if let Some(cancel) = self.cancel.take() {
                let _ = cancel.send(());
            }
            if let Some(task) = self.task.take() {
                task.await
                    .map_err(|_| PublicError::new(PublicErrorKind::Cleanup))?;
            }
            let terminal_result = self
                .terminal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .unwrap_or(Ok(()));
            match terminal_result {
                Err(error) if error.kind() == PublicErrorKind::Cleanup => Err(error),
                Err(error) if !self.terminal_reported => {
                    self.terminal_reported = true;
                    Err(error)
                }
                Ok(()) | Err(_) => Ok(()),
            }
        })
    }
}

fn public_cache_error(error: CacheError) -> PublicError {
    match error {
        CacheError::CleanupFailed | CacheError::CommittedDurabilityUnknown => {
            PublicError::new(PublicErrorKind::Cleanup)
        }
        CacheError::InsecureDirectory
        | CacheError::InsecureFile
        | CacheError::Io(_)
        | CacheError::Json(_)
        | CacheError::InvalidInventory => PublicError::new(PublicErrorKind::Inventory),
    }
}

fn public_master_error(error: SshMasterError) -> PublicError {
    match error {
        SshMasterError::Ssh(failure) => public_ssh_failure(failure.kind()),
        SshMasterError::CleanupFailed | SshMasterError::CloseFailed { .. } => {
            PublicError::new(PublicErrorKind::Cleanup)
        }
        SshMasterError::Io(_) | SshMasterError::ControlTimedOut => {
            PublicError::new(PublicErrorKind::SshUnavailable)
        }
    }
}

fn public_inventory_error(error: InventoryError) -> PublicError {
    match error {
        InventoryError::Ssh(failure) => public_ssh_failure(failure.kind()),
        InventoryError::OwnedChildCleanupFailed { .. } => {
            PublicError::new(PublicErrorKind::Cleanup)
        }
        InventoryError::Io(_)
        | InventoryError::StdoutTooLarge
        | InventoryError::MalformedInventory
        | InventoryError::InvalidSystemTime
        | InventoryError::ProcessTimedOut => PublicError::new(PublicErrorKind::Inventory),
    }
}

fn public_proxy_error(error: ProxyOpenError) -> PublicError {
    match error {
        ProxyOpenError::Master(error) => public_master_error(error),
        ProxyOpenError::Inventory(error) => public_inventory_error(error),
        ProxyOpenError::Stream(error) if error.has_cleanup_failure() => {
            PublicError::new(PublicErrorKind::Cleanup)
        }
        ProxyOpenError::Stream(error) => match error.ssh_failure_kind() {
            Some(kind) => public_ssh_failure(kind),
            None => PublicError::new(PublicErrorKind::Proxy),
        },
        ProxyOpenError::VmNotFound => PublicError::new(PublicErrorKind::VmNotFound),
        ProxyOpenError::VmNotRunning => PublicError::new(PublicErrorKind::VmNotRunning),
    }
}

fn public_ssh_failure(kind: SshFailureKind) -> PublicError {
    let kind = match kind {
        SshFailureKind::HostKeyUnknown => PublicErrorKind::HostKeyUnknown,
        SshFailureKind::HostKeyChanged => PublicErrorKind::HostKeyChanged,
        SshFailureKind::Authentication => PublicErrorKind::SshAuthentication,
        SshFailureKind::Timeout | SshFailureKind::Ssh => PublicErrorKind::SshUnavailable,
    };
    PublicError::new(kind)
}

fn public_rfb_error(error: RfbError) -> PublicError {
    let has_cleanup_failure = error.has_cleanup_failure();
    let kind = match (error.kind(), error.phase()) {
        (_, RfbPhase::Cleanup) => PublicErrorKind::Cleanup,
        (RfbErrorKind::SecurityAllowlist | RfbErrorKind::SecurityFailure, _) => {
            PublicErrorKind::RfbSecurity
        }
        (RfbErrorKind::Limit | RfbErrorKind::Allocation, _) => PublicErrorKind::RfbLimit,
        (RfbErrorKind::Decoder, _) => PublicErrorKind::Decoder,
        (RfbErrorKind::Queue, RfbPhase::EventQueue) => PublicErrorKind::Queue,
        _ => PublicErrorKind::RfbProtocol,
    };
    let error = PublicError::new(kind);
    if has_cleanup_failure {
        error.with_cleanup_failure()
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{tempdir, TempDir};
    use tokio::time::{sleep, timeout};

    use super::{ManagedSession, OpenOptions, ProductionSession};
    use crate::{
        connection::{bounded_vnc_channels, FbRect, VncEvent, VNC_QUEUE_CAPACITY},
        model::{NodeName, PveProfile, SshTarget, VmId},
        runtime::RuntimeDir,
        session::{PublicError, PublicErrorKind, SessionTransportEvent},
        ssh::{SshCommandFactory, SshMaster, TrustedSshProxy},
        vnc::{RfbError, RfbErrorKind, RfbPhase},
    };

    fn production_session(
        terminal: Option<Result<(), PublicError>>,
        terminal_reported: bool,
    ) -> (ProductionSession, crate::connection::VncSessionChannels) {
        let (connection, channels) = bounded_vnc_channels();
        (
            ProductionSession {
                connection,
                task: None,
                terminal: Arc::new(Mutex::new(terminal)),
                terminal_reported,
                cancel: None,
            },
            channels,
        )
    }

    #[test]
    fn terminal_error_survives_a_full_vnc_event_queue() {
        let (connection, channels) = bounded_vnc_channels();
        for _ in 0..VNC_QUEUE_CAPACITY {
            channels
                .event_tx
                .try_send(VncEvent::FramebufferRects(vec![FbRect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                    rgba: vec![0, 0, 0, 255],
                }]))
                .unwrap();
        }
        let mut session = ProductionSession {
            connection,
            task: None,
            terminal: Arc::new(Mutex::new(Some(Err(PublicError::new(
                PublicErrorKind::RfbProtocol,
            ))))),
            terminal_reported: false,
            cancel: None,
        };

        for _ in 0..VNC_QUEUE_CAPACITY {
            assert!(matches!(
                session.try_recv().unwrap(),
                Some(SessionTransportEvent::Framebuffer(_))
            ));
        }
        assert!(matches!(
            session.try_recv().unwrap(),
            Some(SessionTransportEvent::Error(error))
                if error.kind() == PublicErrorKind::RfbProtocol
        ));
        assert!(session.try_recv().unwrap().is_none());
    }

    #[test]
    fn disconnected_vnc_channel_waits_for_terminal_publication_before_reporting() {
        let (mut session, channels) = production_session(None, false);
        drop(channels);

        assert!(session.try_recv().unwrap().is_none());
        *session.terminal.lock().unwrap() =
            Some(Err(PublicError::new(PublicErrorKind::RfbSecurity)));
        assert!(matches!(
            session.try_recv().unwrap(),
            Some(SessionTransportEvent::Error(error))
                if error.kind() == PublicErrorKind::RfbSecurity
        ));
        assert!(session.try_recv().unwrap().is_none());
    }

    #[tokio::test]
    async fn close_surfaces_an_unreported_terminal_error_exactly_once() {
        let (mut session, channels) =
            production_session(Some(Err(PublicError::new(PublicErrorKind::Decoder))), false);
        drop(channels);

        let error = session.close().await.unwrap_err();
        assert_eq!(error.kind(), PublicErrorKind::Decoder);
        assert!(session.close().await.is_ok());
    }

    #[test]
    fn public_terminal_error_preserves_primary_kind_and_cleanup_failure() {
        let error = RfbError::new(
            RfbPhase::Authentication,
            RfbErrorKind::SecurityFailure,
            "synthetic authentication",
        )
        .with_cleanup_failure(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "synthetic cleanup failure",
        ));

        let public = super::public_rfb_error(error);
        assert_eq!(public.kind(), PublicErrorKind::RfbSecurity);
        assert!(public.has_cleanup_failure());
    }

    #[tokio::test]
    async fn explicit_cleanup_does_not_treat_an_earlier_protocol_error_as_cleanup_failure() {
        let (mut session, _channels) = production_session(
            Some(Err(PublicError::new(PublicErrorKind::RfbProtocol))),
            true,
        );

        session.close().await.unwrap();
    }

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

    async fn wait_for(path: &Path) {
        timeout(Duration::from_secs(30), async {
            while !path.exists() {
                sleep(Duration::from_millis(10)).await;
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
        timeout(Duration::from_secs(30), async {
            while exact_pid_is_alive(pid) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned synthetic proxy child was not reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn close_cancels_pre_session_negotiation_and_reaps_the_owned_proxy() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime
                .control_socket()
                .with_extension("proxy_wait_for_eof"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let mut verified = master.verify().await.unwrap();
        let proxy = TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap())
            .await
            .unwrap();
        let mut session = ProductionSession::spawn(proxy, OpenOptions::default());
        let proxy_pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&proxy_pid_path).await;
        let proxy_pid = helper_pid(&proxy_pid_path);

        timeout(Duration::from_secs(2), session.close())
            .await
            .expect("pre-session cancellation did not complete")
            .unwrap();
        assert_exact_pid_is_gone(proxy_pid).await;
        master.close().await.unwrap();
    }
}
