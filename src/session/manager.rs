use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use crossbeam_channel::TrySendError;
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
    vnc::{
        normalize_resize_request, ClipboardText, InputController, InputError, RfbError,
        RfbErrorKind, RfbPhase, VncClient,
    },
};

use super::{
    AppCommand, AppEvent, DesktopSize, InputAction, OpenOptions, PublicError, PublicErrorKind,
    ResizeProtocolOutcome, ResizeStatus, SessionId, SessionPhase, SessionSnapshot,
    SessionTransportEvent,
};

pub const APP_QUEUE_CAPACITY: usize = 256;
const MAX_ACTIVE_NATIVE_SESSIONS: usize = 2;
const SESSION_POLL_INTERVAL: Duration = Duration::from_millis(1);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(250);
const RESIZE_OUTCOME_DEADLINE: Duration = Duration::from_secs(2);

pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait ManagedSession: Send + 'static {
    fn try_recv(&mut self) -> Result<Option<SessionTransportEvent>, PublicError>;
    fn mark_ready(&mut self);
    fn send_input(&mut self, action: InputAction) -> Result<Option<ClipboardText>, InputError>;
    fn request_resize(&mut self, _requested: DesktopSize) -> Result<(), PublicError> {
        Err(PublicError::new(PublicErrorKind::Queue))
    }
    fn release_all_keys(&mut self) -> Result<(), PublicError>;
    fn close(&mut self, deadline: Instant) -> BackendFuture<'_, Result<(), PublicError>>;
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

    pub fn try_send(
        &self,
        command: AppCommand,
    ) -> Result<(), mpsc::error::TrySendError<AppCommand>> {
        self.command_tx.try_send(command)
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
    resize: ResizePolicy,
}

#[derive(Clone, Copy)]
struct InFlightResize {
    requested: DesktopSize,
    deadline: Instant,
}

struct ResizePolicy {
    desired: Option<DesktopSize>,
    debounce_deadline: Option<Instant>,
    in_flight: Option<InFlightResize>,
    pending_replacement: Option<DesktopSize>,
    automatic_allowed: bool,
}

impl ResizePolicy {
    fn new(enabled: bool) -> Self {
        Self {
            desired: None,
            debounce_deadline: None,
            in_flight: None,
            pending_replacement: None,
            automatic_allowed: enabled,
        }
    }

    fn cancel_automatic_work(&mut self) {
        self.debounce_deadline = None;
        self.pending_replacement = None;
    }
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
            AppCommand::ViewportChanged {
                session_id,
                backing_width,
                backing_height,
            } => {
                self.viewport_changed(session_id, backing_width, backing_height)
                    .await
            }
            AppCommand::SetDynamicResolution {
                session_id,
                enabled,
            } => self.set_dynamic_resolution(session_id, enabled).await,
            AppCommand::RetryDynamicResolution { session_id } => {
                self.retry_dynamic_resolution(session_id).await
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
        let snapshot = SessionSnapshot::opening_with_options(
            session_id,
            self.profile_name.clone(),
            vmid,
            options.view_only,
            options.clipboard_enabled,
            options.dynamic_resolution,
        );
        self.sessions.push(SessionRecord {
            snapshot: snapshot.clone(),
            options,
            session: None,
            error_emitted: false,
            resize: ResizePolicy::new(options.dynamic_resolution),
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
        let requested_view_only = match &action {
            InputAction::SetViewOnly(enabled) => Some(*enabled),
            _ => None,
        };
        let result = self.sessions[index]
            .session
            .as_mut()
            .map(|session| session.send_input(action));
        let accepted = result.as_ref().is_some_and(Result::is_ok);
        let mut snapshot_changed = false;
        if let Some(enabled) = requested_view_only {
            let authoritative = enabled || accepted;
            if authoritative {
                self.sessions[index].options.view_only = enabled;
                self.sessions[index].snapshot.view_only = enabled;
                snapshot_changed = true;
            }
        }
        if snapshot_changed {
            self.emit_session_snapshot(index).await;
        }
        match result {
            Some(Ok(Some(text))) => {
                self.emit_critical(AppEvent::ClipboardReceived { session_id, text })
                    .await;
            }
            Some(Ok(None)) => {}
            Some(Err(reason)) => {
                self.emit_critical(AppEvent::InputRejected { session_id, reason })
                    .await;
            }
            None => {
                self.emit_critical(AppEvent::Error(
                    PublicError::new(PublicErrorKind::Queue).for_session(session_id, vmid),
                ))
                .await;
            }
        }
    }

    async fn viewport_changed(
        &mut self,
        session_id: SessionId,
        backing_width: u32,
        backing_height: u32,
    ) {
        let Some(index) = self.session_index(session_id) else {
            self.emit_critical(AppEvent::Error(PublicError::new(
                PublicErrorKind::VmNotFound,
            )))
            .await;
            return;
        };
        let limits = self.sessions[index].options.vnc.limits;
        let requested = normalize_resize_request(backing_width, backing_height, limits).ok();
        let previous_status = self.sessions[index].snapshot.resize_status;
        self.sessions[index].resize.desired = requested;
        self.sessions[index].resize.pending_replacement = None;
        self.sessions[index].resize.debounce_deadline = None;
        if requested.is_some()
            && self.sessions[index].options.dynamic_resolution
            && self.sessions[index].resize.automatic_allowed
            && self.sessions[index].snapshot.phase == SessionPhase::Ready
        {
            self.sessions[index].resize.debounce_deadline = Some(Instant::now() + RESIZE_DEBOUNCE);
            if self.sessions[index].resize.in_flight.is_none() {
                self.sessions[index].snapshot.resize_status = ResizeStatus::Waiting;
            }
        } else if requested.is_none()
            && self.sessions[index].resize.in_flight.is_none()
            && self.sessions[index].resize.automatic_allowed
        {
            self.sessions[index].snapshot.resize_status =
                if self.sessions[index].options.dynamic_resolution {
                    ResizeStatus::Waiting
                } else {
                    ResizeStatus::Disabled
                };
        }
        if self.sessions[index].snapshot.resize_status != previous_status {
            self.emit_session_snapshot(index).await;
        }
    }

    async fn set_dynamic_resolution(&mut self, session_id: SessionId, enabled: bool) {
        let Some(index) = self.session_index(session_id) else {
            self.emit_critical(AppEvent::Error(PublicError::new(
                PublicErrorKind::VmNotFound,
            )))
            .await;
            return;
        };
        self.sessions[index].options.dynamic_resolution = enabled;
        self.sessions[index].snapshot.dynamic_resolution_enabled = enabled;
        self.sessions[index].resize.cancel_automatic_work();
        self.sessions[index].resize.in_flight = None;
        self.sessions[index].resize.automatic_allowed = enabled;
        self.sessions[index].snapshot.resize_status = if enabled {
            ResizeStatus::Waiting
        } else {
            ResizeStatus::Disabled
        };
        if enabled
            && self.sessions[index].resize.desired.is_some()
            && self.sessions[index].snapshot.phase == SessionPhase::Ready
        {
            self.sessions[index].resize.debounce_deadline = Some(Instant::now() + RESIZE_DEBOUNCE);
        }
        self.emit_session_snapshot(index).await;
    }

    async fn retry_dynamic_resolution(&mut self, session_id: SessionId) {
        let Some(index) = self.session_index(session_id) else {
            self.emit_critical(AppEvent::Error(PublicError::new(
                PublicErrorKind::VmNotFound,
            )))
            .await;
            return;
        };
        if !self.sessions[index].options.dynamic_resolution {
            return;
        }
        self.sessions[index].resize.automatic_allowed = true;
        self.sessions[index].resize.in_flight = None;
        self.sessions[index].resize.pending_replacement = None;
        self.sessions[index].snapshot.resize_status = ResizeStatus::Waiting;
        self.sessions[index].resize.debounce_deadline = self.sessions[index]
            .resize
            .desired
            .filter(|_| self.sessions[index].snapshot.phase == SessionPhase::Ready)
            .map(|_| Instant::now() + RESIZE_DEBOUNCE);
        self.emit_session_snapshot(index).await;
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
                        if let Some(session) = self.sessions[index].session.as_mut() {
                            session.mark_ready();
                        }
                        self.transition(index, SessionPhase::Ready).await;
                        self.arm_resize_after_ready(index);
                    }
                    let session_id = self.sessions[index].snapshot.session_id;
                    self.emit_framebuffer(AppEvent::Framebuffer { session_id, rects });
                }
                SessionTransportEvent::DesktopSize(size) => {
                    self.handle_desktop_size(index, size).await;
                }
                SessionTransportEvent::ResizeOutcome(outcome) => {
                    self.handle_resize_outcome(index, outcome).await;
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
        self.poll_resize_policy().await;
    }

    fn arm_resize_after_ready(&mut self, index: usize) {
        let record = &mut self.sessions[index];
        if record.options.dynamic_resolution
            && record.resize.automatic_allowed
            && record.resize.desired.is_some()
        {
            record.resize.debounce_deadline = Some(Instant::now() + RESIZE_DEBOUNCE);
            record.snapshot.resize_status = ResizeStatus::Waiting;
        }
    }

    async fn handle_desktop_size(&mut self, index: usize, size: DesktopSize) {
        self.sessions[index].snapshot.guest_size = Some(size);
        let matching = self.sessions[index]
            .resize
            .in_flight
            .is_some_and(|request| request.requested == size);
        if matching {
            self.sessions[index].resize.in_flight = None;
            self.sessions[index].snapshot.resize_status = ResizeStatus::Applied(size);
            let follow_up = self.sessions[index].resize.pending_replacement.take();
            self.emit_session_snapshot(index).await;
            if let Some(follow_up) = follow_up.filter(|requested| *requested != size) {
                if self.sessions[index].options.dynamic_resolution
                    && self.sessions[index].resize.automatic_allowed
                    && self.sessions[index].snapshot.phase == SessionPhase::Ready
                {
                    self.issue_resize(index, follow_up).await;
                }
            }
        } else {
            self.emit_session_snapshot(index).await;
        }
    }

    async fn handle_resize_outcome(&mut self, index: usize, outcome: ResizeProtocolOutcome) {
        let Some(in_flight) = self.sessions[index].resize.in_flight else {
            return;
        };
        match outcome {
            ResizeProtocolOutcome::Forwarded(size) if size == in_flight.requested => {
                self.sessions[index].snapshot.resize_status = ResizeStatus::Pending(size);
            }
            ResizeProtocolOutcome::Forwarded(_) | ResizeProtocolOutcome::Unsupported => {
                self.sessions[index].resize.in_flight = None;
                self.sessions[index].resize.automatic_allowed = false;
                self.sessions[index].resize.cancel_automatic_work();
                self.sessions[index].snapshot.resize_status = ResizeStatus::Unsupported;
            }
            ResizeProtocolOutcome::Rejected => {
                self.sessions[index].resize.in_flight = None;
                self.sessions[index].resize.automatic_allowed = false;
                self.sessions[index].resize.cancel_automatic_work();
                self.sessions[index].snapshot.resize_status = ResizeStatus::Rejected;
            }
        }
        self.emit_session_snapshot(index).await;
    }

    async fn poll_resize_policy(&mut self) {
        let now = Instant::now();
        for index in 0..self.sessions.len() {
            let mut emit_snapshot = false;
            let mut issue = None;
            {
                let record = &mut self.sessions[index];
                if record
                    .resize
                    .in_flight
                    .is_some_and(|request| now >= request.deadline)
                {
                    record.resize.in_flight = None;
                    record.resize.automatic_allowed = false;
                    record.resize.cancel_automatic_work();
                    record.snapshot.resize_status = ResizeStatus::TimedOut;
                    emit_snapshot = true;
                } else if record
                    .resize
                    .debounce_deadline
                    .is_some_and(|deadline| now >= deadline)
                {
                    record.resize.debounce_deadline = None;
                    if let Some(desired) = record.resize.desired {
                        if record.resize.in_flight.is_some() {
                            record.resize.pending_replacement = Some(desired);
                        } else if record.options.dynamic_resolution
                            && record.resize.automatic_allowed
                            && record.snapshot.phase == SessionPhase::Ready
                        {
                            issue = Some(desired);
                        }
                    }
                }
            }
            if emit_snapshot {
                self.emit_session_snapshot(index).await;
            }
            if let Some(requested) = issue {
                self.issue_resize(index, requested).await;
            }
        }
    }

    async fn issue_resize(&mut self, index: usize, requested: DesktopSize) {
        if self.sessions[index].resize.in_flight.is_some()
            || self.sessions[index].snapshot.phase != SessionPhase::Ready
        {
            return;
        }
        let result = self.sessions[index]
            .session
            .as_mut()
            .ok_or_else(|| PublicError::new(PublicErrorKind::Queue))
            .and_then(|session| session.request_resize(requested));
        match result {
            Ok(()) => {
                self.sessions[index].resize.in_flight = Some(InFlightResize {
                    requested,
                    deadline: Instant::now() + RESIZE_OUTCOME_DEADLINE,
                });
                self.sessions[index].snapshot.resize_status = ResizeStatus::Requested(requested);
                self.emit_session_snapshot(index).await;
            }
            Err(error) => {
                self.sessions[index].resize.automatic_allowed = false;
                self.sessions[index].snapshot.resize_status = ResizeStatus::Waiting;
                let snapshot = &self.sessions[index].snapshot;
                let contextual = error.for_session(snapshot.session_id, snapshot.vmid);
                self.emit_session_snapshot(index).await;
                self.emit_critical(AppEvent::Error(contextual)).await;
            }
        }
    }

    async fn emit_session_snapshot(&mut self, index: usize) {
        let snapshot = self.sessions[index].snapshot.clone();
        self.emit_critical(AppEvent::SessionChanged(snapshot)).await;
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
            let deadline = Instant::now() + graceful_close_timeout();
            let release_result = session.release_all_keys();
            let transport_close = session.close(deadline).await;
            compose_release_and_transport(release_result, transport_close)
        } else {
            Ok(())
        };
        let result = match (primary_error, close_result) {
            (Some(primary), Err(_)) => Err(primary.with_cleanup_failure()),
            (Some(primary), Ok(())) => Err(primary),
            (None, result) => result,
        };
        if let Err(error) = result {
            if !self.sessions[index].error_emitted {
                self.sessions[index].error_emitted = true;
                self.emit_critical(AppEvent::Error(error)).await;
            }
        }
        self.transition(index, SessionPhase::Disconnected).await;
        result
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
            ProductionSession::spawn(proxy, options)
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
    input: InputController<VncConnection>,
    task: Option<JoinHandle<()>>,
    terminal: Arc<Mutex<Option<Result<(), PublicError>>>>,
    terminal_reported: bool,
    cancel: Option<oneshot::Sender<()>>,
}

impl ProductionSession {
    fn spawn(proxy: TrustedSshProxy, options: OpenOptions) -> Result<Self, PublicError> {
        let (connection, channels) = bounded_vnc_channels();
        let input = InputController::for_connection(
            connection,
            options.view_only,
            options.clipboard_enabled,
            options.vnc.limits.max_clipboard_bytes as usize,
        )
        .map_err(|_| PublicError::new(PublicErrorKind::RfbLimit))?;
        let terminal = Arc::new(Mutex::new(None));
        let task_terminal = terminal.clone();
        let terminal_sender = channels.event_tx.clone();
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = VncClient::run_cancellable(
                proxy,
                options.vnc,
                channels,
                options.clipboard_enabled,
                cancelled,
            )
            .await
            .map_err(public_rfb_error);
            *task_terminal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
            drop(terminal_sender);
        });
        Ok(Self {
            input,
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        })
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
            match self.input.connection().event_rx.try_recv() {
                Ok(VncEvent::FramebufferRects(rects)) => {
                    return Ok(Some(SessionTransportEvent::Framebuffer(rects)));
                }
                Ok(VncEvent::DesktopSize(size)) => {
                    return Ok(Some(SessionTransportEvent::DesktopSize(size)));
                }
                Ok(VncEvent::ResizeOutcome(outcome)) => {
                    return Ok(Some(SessionTransportEvent::ResizeOutcome(outcome)));
                }
                Ok(VncEvent::Error(error)) => {
                    self.terminal_reported = true;
                    return Ok(Some(SessionTransportEvent::Error(public_rfb_error(error))));
                }
                Ok(VncEvent::Disconnected) => {
                    self.terminal_reported = true;
                    return Ok(Some(SessionTransportEvent::Disconnected));
                }
                Ok(VncEvent::DesktopName(_)) => {}
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

    fn mark_ready(&mut self) {
        self.input.mark_ready();
    }

    fn send_input(&mut self, action: InputAction) -> Result<Option<ClipboardText>, InputError> {
        match action {
            InputAction::Key { down, keysym } => self.input.key(down, keysym).map(|()| None),
            InputAction::Pointer { buttons, x, y } => {
                self.input.pointer(buttons, x, y).map(|()| None)
            }
            InputAction::CtrlAltDelete => self.input.ctrl_alt_delete().map(|()| None),
            InputAction::ReleaseAllKeys => self.input.release_all_keys().map(|()| None),
            InputAction::FocusLost => self.input.focus_lost().map(|()| None),
            InputAction::SetViewOnly(enabled) => self.input.set_view_only(enabled).map(|()| None),
            InputAction::SendClipboard(text) => self.input.send_clipboard(text).map(|()| None),
            InputAction::ReceiveClipboard => self.input.receive_clipboard(),
        }
    }

    fn request_resize(&mut self, requested: DesktopSize) -> Result<(), PublicError> {
        self.input
            .connection()
            .request_desktop_size(requested)
            .map_err(|error| match error {
                TrySendError::Full(_) => PublicError::new(PublicErrorKind::Queue),
                TrySendError::Disconnected(_) => PublicError::new(PublicErrorKind::Cleanup),
            })
    }

    fn release_all_keys(&mut self) -> Result<(), PublicError> {
        self.input
            .release_all_keys()
            .map_err(public_input_cleanup_error)
    }

    fn close(&mut self, deadline: Instant) -> BackendFuture<'_, Result<(), PublicError>> {
        Box::pin(async move {
            let session_loop_ready = self.input.is_ready();
            let mut primary = None;
            let mut cleanup_failed = false;
            if let Err(error) = self.input.clear_session() {
                match public_input_cleanup_error(error).kind() {
                    PublicErrorKind::Queue => {
                        primary = Some(PublicError::new(PublicErrorKind::Queue));
                    }
                    _ => cleanup_failed = true,
                }
            }
            let task_running = self.task.as_ref().is_some_and(|task| !task.is_finished());
            let mut use_cancellation = !session_loop_ready || !task_running;

            if session_loop_ready && task_running {
                match self.input.connection().begin_graceful_close() {
                    Ok(acknowledged) => {
                        if !matches!(
                            tokio::time::timeout_at(deadline, acknowledged).await,
                            Ok(Ok(()))
                        ) {
                            cleanup_failed = true;
                            use_cancellation = true;
                        }
                    }
                    Err(TrySendError::Full(_)) => {
                        primary.get_or_insert(PublicError::new(PublicErrorKind::Queue));
                        use_cancellation = true;
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        cleanup_failed = true;
                        use_cancellation = true;
                    }
                }
            }

            if use_cancellation {
                if let Some(cancel) = self.cancel.take() {
                    let _ = cancel.send(());
                }
            }
            if let Some(mut task) = self.task.take() {
                match tokio::time::timeout_at(deadline, &mut task).await {
                    Ok(result) => {
                        if result.is_err() {
                            cleanup_failed = true;
                        }
                    }
                    Err(_) => {
                        cleanup_failed = true;
                        if let Some(cancel) = self.cancel.take() {
                            let _ = cancel.send(());
                        }
                        tokio::task::yield_now().await;
                        task.abort();
                        if task.await.is_err_and(|error| !error.is_cancelled()) {
                            cleanup_failed = true;
                        }
                    }
                }
            }
            drop(self.cancel.take());
            self.input.clear_clipboard();
            let terminal_result = self
                .terminal
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .unwrap_or(Ok(()));
            let terminal_error = match terminal_result {
                Err(error) if error.kind() == PublicErrorKind::Cleanup => Some(error),
                Err(error) if !self.terminal_reported => {
                    self.terminal_reported = true;
                    Some(error)
                }
                Ok(()) | Err(_) => None,
            };
            if let Some(error) = terminal_error {
                if error.kind() == PublicErrorKind::Cleanup {
                    cleanup_failed = true;
                } else {
                    primary.get_or_insert(error);
                }
            }
            match (primary, cleanup_failed) {
                (Some(error), true) => Err(error.with_cleanup_failure()),
                (Some(error), false) => Err(error),
                (None, true) => Err(PublicError::new(PublicErrorKind::Cleanup)),
                (None, false) => Ok(()),
            }
        })
    }
}

fn graceful_close_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_millis(100)
    }
    #[cfg(not(test))]
    {
        Duration::from_secs(3)
    }
}

fn compose_release_and_transport(
    release: Result<(), PublicError>,
    transport: Result<(), PublicError>,
) -> Result<(), PublicError> {
    match (release, transport) {
        (Ok(()), result) => result,
        (Err(release), Ok(())) => Err(release),
        (Err(release), Err(transport))
            if release.kind() == PublicErrorKind::Cleanup
                && transport.kind() != PublicErrorKind::Cleanup =>
        {
            Err(transport.with_cleanup_failure())
        }
        (Err(release), Err(_)) => Err(release.with_cleanup_failure()),
    }
}

fn public_input_cleanup_error(error: InputError) -> PublicError {
    match error {
        InputError::QueueUnavailable => PublicError::new(PublicErrorKind::Queue),
        InputError::TransportDisconnected => PublicError::new(PublicErrorKind::Cleanup),
        _ => PublicError::new(PublicErrorKind::Cleanup),
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
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{tempdir, TempDir};
    use tokio::sync::oneshot;
    use tokio::time::{sleep, timeout, Instant};

    use super::{
        BackendFuture, ManagedSession, OpenOptions, ProductionSession, SessionBackend,
        SessionManager,
    };
    use crate::{
        config::AppConfig,
        connection::{bounded_vnc_channels, FbRect, VncCommand, VncEvent, VNC_QUEUE_CAPACITY},
        model::{NodeName, PveProfile, SshTarget, VmId},
        runtime::RuntimeDir,
        session::{
            AppCommand, AppEvent, InputAction, PublicError, PublicErrorKind, SessionPhase,
            SessionTransportEvent,
        },
        ssh::{
            InventorySnapshot, SshCommandFactory, SshMaster, TrustedSshProxy, VmInventoryItem,
            VmStatus,
        },
        vnc::{
            ClipboardText, InputController, RfbError, RfbErrorKind, RfbPhase, CLIPBOARD_TEXT_LIMIT,
        },
    };

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    async fn wait_for_flag(flag: &AtomicBool) {
        for _ in 0..1_000 {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("controlled task did not reach the expected state");
    }

    fn close_deadline() -> Instant {
        Instant::now() + super::graceful_close_timeout()
    }

    fn production_session(
        terminal: Option<Result<(), PublicError>>,
        terminal_reported: bool,
    ) -> (ProductionSession, crate::connection::VncSessionChannels) {
        let (connection, channels) = bounded_vnc_channels();
        (
            ProductionSession {
                input: InputController::for_connection(
                    connection,
                    false,
                    false,
                    CLIPBOARD_TEXT_LIMIT,
                )
                .unwrap(),
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
            input: InputController::for_connection(connection, false, false, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
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

        let error = session.close(close_deadline()).await.unwrap_err();
        assert_eq!(error.kind(), PublicErrorKind::Decoder);
        assert!(session.close(close_deadline()).await.is_ok());
    }

    #[tokio::test]
    async fn production_session_buffers_remote_clipboard_until_one_explicit_receive_and_close() {
        let (connection, channels) = bounded_vnc_channels();
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, true, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: None,
            terminal: Arc::new(Mutex::new(None)),
            terminal_reported: false,
            cancel: None,
        };
        session.mark_ready();
        channels
            .clipboard
            .replace(ClipboardText::try_from(b"first".to_vec()).unwrap());
        channels
            .clipboard
            .replace(ClipboardText::try_from(b"second".to_vec()).unwrap());
        assert_eq!(channels.clipboard.retained_count(), 1);
        assert!(session.input.connection().event_rx.try_recv().is_err());

        let received = session
            .send_input(InputAction::ReceiveClipboard)
            .unwrap()
            .expect("explicit receive must surface the pending text");
        assert_eq!(received.as_str(), "second");
        assert!(session
            .send_input(InputAction::ReceiveClipboard)
            .unwrap()
            .is_none());

        channels
            .clipboard
            .replace(ClipboardText::try_from(b"clear on close".to_vec()).unwrap());
        assert_eq!(channels.clipboard.retained_count(), 1);
        session.close(close_deadline()).await.unwrap();
        assert_eq!(channels.clipboard.retained_count(), 0);
        session.mark_ready();
        assert!(session
            .send_input(InputAction::ReceiveClipboard)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn production_session_release_then_close_has_one_effective_key_up() {
        let (mut session, channels) = production_session(None, false);
        session.mark_ready();
        session
            .send_input(InputAction::Key {
                down: true,
                keysym: 0x41,
            })
            .unwrap();
        assert!(matches!(
            channels.command_rx.try_recv(),
            Ok(VncCommand::KeyEvent {
                down: true,
                keysym: 0x41,
            })
        ));

        session.release_all_keys().unwrap();
        assert!(matches!(
            channels.command_rx.try_recv(),
            Ok(VncCommand::KeyEvent {
                down: false,
                keysym: 0x41,
            })
        ));
        session.close(close_deadline()).await.unwrap();
        assert!(channels.command_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stalled_graceful_close_ack_falls_back_to_cancellation_within_policy() {
        let (connection, channels) = bounded_vnc_channels();
        let terminal = Arc::new(Mutex::new(None));
        let task_terminal = Arc::clone(&terminal);
        let cancelled_flag = Arc::new(AtomicBool::new(false));
        let task_cancelled_flag = Arc::clone(&cancelled_flag);
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = cancelled.await;
            task_cancelled_flag.store(true, Ordering::SeqCst);
            *task_terminal.lock().unwrap() = Some(Ok(()));
        });
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, false, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        };
        session.mark_ready();
        session
            .send_input(InputAction::Key {
                down: true,
                keysym: 0xffe3,
            })
            .unwrap();

        let started = Instant::now();
        let error = session.close(close_deadline()).await.unwrap_err();

        assert_eq!(error.kind(), PublicErrorKind::Cleanup);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(cancelled_flag.load(Ordering::SeqCst));
        assert!(matches!(
            channels.command_rx.try_recv(),
            Ok(VncCommand::KeyEvent {
                down: true,
                keysym: 0xffe3,
            })
        ));
        assert!(matches!(
            channels.command_rx.try_recv(),
            Ok(VncCommand::KeyEvent {
                down: false,
                keysym: 0xffe3,
            })
        ));
        assert!(channels.command_rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn stalled_ack_preserves_transport_primary_and_marks_cleanup_failure() {
        let (connection, _channels) = bounded_vnc_channels();
        let primary = PublicError::new(PublicErrorKind::RfbSecurity);
        let terminal = Arc::new(Mutex::new(Some(Err(primary))));
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = cancelled.await;
        });
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, false, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        };
        session.mark_ready();

        let error = session.close(close_deadline()).await.unwrap_err();

        assert_eq!(error.kind(), PublicErrorKind::RfbSecurity);
        assert!(error.has_cleanup_failure());
    }

    #[tokio::test(start_paused = true)]
    async fn total_close_deadline_aborts_post_ack_shutdown_stall_and_preserves_primary() {
        let (connection, channels) = bounded_vnc_channels();
        let terminal = Arc::new(Mutex::new(Some(Err(PublicError::new(
            PublicErrorKind::RfbSecurity,
        )))));
        let (cancel, cancelled) = oneshot::channel();
        let barrier_seen = Arc::new(AtomicBool::new(false));
        let task_barrier_seen = Arc::clone(&barrier_seen);
        let task_dropped = Arc::new(AtomicBool::new(false));
        let task_drop_probe = Arc::clone(&task_dropped);
        let task = tokio::spawn(async move {
            let _drop_probe = DropProbe(task_drop_probe);
            let _ignored_cancellation = cancelled;
            loop {
                match channels.command_rx.try_recv() {
                    Ok(VncCommand::GracefulDisconnect(barrier)) => {
                        task_barrier_seen.store(true, Ordering::SeqCst);
                        sleep(Duration::from_millis(60)).await;
                        barrier.acknowledge();
                        std::future::pending::<()>().await;
                    }
                    Ok(_) | Err(crossbeam_channel::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => return,
                }
            }
        });
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, false, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        };
        session.mark_ready();

        let close = tokio::spawn(async move {
            let result = session.close(close_deadline()).await;
            (result, session)
        });
        wait_for_flag(&barrier_seen).await;
        tokio::time::advance(Duration::from_millis(60)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(41)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(
            close.is_finished(),
            "complete close exceeded one total deadline"
        );
        let (result, _session) = close.await.unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.kind(), PublicErrorKind::RfbSecurity);
        assert!(error.has_cleanup_failure());
        assert!(task_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn total_close_deadline_aborts_cancellation_ignored_task_and_finally_clears_clipboard() {
        let (connection, channels) = bounded_vnc_channels();
        let retained = channels.clipboard.clone();
        let task_retained = retained.clone();
        let terminal = Arc::new(Mutex::new(None));
        let (cancel, cancelled) = oneshot::channel();
        let clipboard_inserted = Arc::new(AtomicBool::new(false));
        let task_clipboard_inserted = Arc::clone(&clipboard_inserted);
        let task_dropped = Arc::new(AtomicBool::new(false));
        let task_drop_probe = Arc::clone(&task_dropped);
        let task = tokio::spawn(async move {
            let _drop_probe = DropProbe(task_drop_probe);
            let _ = cancelled.await;
            task_retained.replace(ClipboardText::try_from(b"late".to_vec()).unwrap());
            task_clipboard_inserted.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        });
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, true, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        };

        let close = tokio::spawn(async move {
            let result = session.close(close_deadline()).await;
            (result, session)
        });
        wait_for_flag(&clipboard_inserted).await;
        assert_eq!(retained.retained_count(), 1);
        tokio::time::advance(Duration::from_millis(101)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(
            close.is_finished(),
            "cancellation-ignored close exceeded its deadline"
        );
        let (result, mut session) = close.await.unwrap();
        assert_eq!(result.unwrap_err().kind(), PublicErrorKind::Cleanup);
        assert!(task_dropped.load(Ordering::SeqCst));
        assert_eq!(retained.retained_count(), 0);
        session.mark_ready();
        assert!(session
            .send_input(InputAction::ReceiveClipboard)
            .unwrap()
            .is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn consumed_manager_deadline_reaches_exact_task_abort_without_a_fresh_budget() {
        let (connection, channels) = bounded_vnc_channels();
        let terminal = Arc::new(Mutex::new(None));
        let (cancel, cancelled) = oneshot::channel();
        let task_started = Arc::new(AtomicBool::new(false));
        let task_started_probe = Arc::clone(&task_started);
        let task_dropped = Arc::new(AtomicBool::new(false));
        let task_drop_probe = Arc::clone(&task_dropped);
        let task = tokio::spawn(async move {
            let _drop_probe = DropProbe(task_drop_probe);
            let _ignored_cancellation = cancelled;
            let _owned_channels = channels;
            task_started_probe.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        });
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, false, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        };
        session.mark_ready();
        wait_for_flag(&task_started).await;

        let close = tokio::spawn(async move { session.close(Instant::now()).await });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(
            close.is_finished(),
            "an already-consumed manager deadline must not be renewed"
        );
        assert_eq!(
            close.await.unwrap().unwrap_err().kind(),
            PublicErrorKind::Cleanup
        );
        assert!(task_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn normal_ack_close_finally_clears_clipboard_repopulated_after_initial_clear() {
        let (connection, channels) = bounded_vnc_channels();
        let retained = channels.clipboard.clone();
        let task_retained = retained.clone();
        let terminal = Arc::new(Mutex::new(None));
        let task_terminal = Arc::clone(&terminal);
        let (cancel, cancelled) = oneshot::channel();
        let clipboard_inserted = Arc::new(AtomicBool::new(false));
        let task_clipboard_inserted = Arc::clone(&clipboard_inserted);
        let task = tokio::spawn(async move {
            let _ignored_cancellation = cancelled;
            loop {
                match channels.command_rx.try_recv() {
                    Ok(VncCommand::GracefulDisconnect(barrier)) => {
                        task_retained.replace(ClipboardText::try_from(b"late".to_vec()).unwrap());
                        task_clipboard_inserted.store(true, Ordering::SeqCst);
                        barrier.acknowledge();
                        *task_terminal.lock().unwrap() = Some(Ok(()));
                        return;
                    }
                    Ok(_) | Err(crossbeam_channel::TryRecvError::Empty) => {
                        tokio::task::yield_now().await;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => return,
                }
            }
        });
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, true, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        };
        session.mark_ready();

        let result = session.close(close_deadline()).await;

        assert!(clipboard_inserted.load(Ordering::SeqCst));
        result.unwrap();
        assert_eq!(retained.retained_count(), 0);
        session.mark_ready();
        assert!(session
            .send_input(InputAction::ReceiveClipboard)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn graceful_close_full_queue_remains_bounded_queue_pressure() {
        let (connection, channels) = bounded_vnc_channels();
        for _ in 0..VNC_QUEUE_CAPACITY {
            connection.send_pointer(0, 0, 0).unwrap();
        }
        let terminal = Arc::new(Mutex::new(None));
        let task_terminal = Arc::clone(&terminal);
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = cancelled.await;
            *task_terminal.lock().unwrap() = Some(Ok(()));
            drop(channels);
        });
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, false, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        };
        session.mark_ready();

        let error = timeout(Duration::from_secs(2), session.close(close_deadline()))
            .await
            .expect("full-queue close was not bounded")
            .unwrap_err();

        assert_eq!(error.kind(), PublicErrorKind::Queue);
        assert!(!error.has_cleanup_failure());
    }

    #[tokio::test]
    async fn graceful_close_disconnected_queue_preserves_terminal_transport_primary() {
        let (connection, channels) = bounded_vnc_channels();
        let terminal = Arc::new(Mutex::new(Some(Err(PublicError::new(
            PublicErrorKind::RfbSecurity,
        )))));
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = cancelled.await;
        });
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, false, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        };
        session.mark_ready();
        session
            .send_input(InputAction::Key {
                down: true,
                keysym: 0x51,
            })
            .unwrap();
        assert!(matches!(
            channels.command_rx.try_recv(),
            Ok(VncCommand::KeyEvent {
                down: true,
                keysym: 0x51,
            })
        ));
        drop(channels.command_rx);

        let error = timeout(Duration::from_secs(2), session.close(close_deadline()))
            .await
            .expect("disconnected-queue close was not bounded")
            .unwrap_err();

        assert_eq!(error.kind(), PublicErrorKind::RfbSecurity);
        assert!(error.has_cleanup_failure());
    }

    #[tokio::test]
    async fn graceful_close_disconnected_queue_without_terminal_reports_cleanup() {
        let (connection, channels) = bounded_vnc_channels();
        let terminal = Arc::new(Mutex::new(None));
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = cancelled.await;
        });
        let mut session = ProductionSession {
            input: InputController::for_connection(connection, false, false, CLIPBOARD_TEXT_LIMIT)
                .unwrap(),
            task: Some(task),
            terminal,
            terminal_reported: false,
            cancel: Some(cancel),
        };
        session.mark_ready();
        session
            .send_input(InputAction::Key {
                down: true,
                keysym: 0x52,
            })
            .unwrap();
        assert!(matches!(
            channels.command_rx.try_recv(),
            Ok(VncCommand::KeyEvent {
                down: true,
                keysym: 0x52,
            })
        ));
        drop(channels.command_rx);

        let error = timeout(Duration::from_secs(2), session.close(close_deadline()))
            .await
            .expect("disconnected-queue close was not bounded")
            .unwrap_err();

        assert_eq!(error.kind(), PublicErrorKind::Cleanup);
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

        session.close(close_deadline()).await.unwrap();
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

    struct ProductionPathBackend {
        session: Option<ProductionSession>,
        opens: Arc<AtomicUsize>,
    }

    struct ProductionWireBackend {
        master: Option<SshMaster>,
        opens: Arc<AtomicUsize>,
    }

    impl SessionBackend for ProductionPathBackend {
        type Session = ProductionSession;

        fn load_cache(
            &mut self,
        ) -> BackendFuture<'_, Result<Option<InventorySnapshot>, PublicError>> {
            Box::pin(async { Ok(None) })
        }

        fn start_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
            Box::pin(async { Ok(()) })
        }

        fn fetch_inventory(&mut self) -> BackendFuture<'_, Result<InventorySnapshot, PublicError>> {
            Box::pin(async {
                Ok(InventorySnapshot {
                    observed_at_unix_ms: 1,
                    stale: false,
                    vms: vec![VmInventoryItem {
                        vmid: VmId::new(107).unwrap(),
                        name: "SYNTHETIC-107".to_owned(),
                        node: NodeName::parse("pve2").unwrap(),
                        status: VmStatus::Running,
                        template: false,
                    }],
                })
            })
        }

        fn save_cache(
            &mut self,
            _snapshot: &InventorySnapshot,
        ) -> BackendFuture<'_, Result<(), PublicError>> {
            Box::pin(async { Ok(()) })
        }

        fn open_session(
            &mut self,
            _vmid: VmId,
            _options: OpenOptions,
        ) -> BackendFuture<'_, Result<Self::Session, PublicError>> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            let result = self
                .session
                .take()
                .ok_or_else(|| PublicError::new(PublicErrorKind::Proxy));
            Box::pin(async move { result })
        }

        fn close_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl SessionBackend for ProductionWireBackend {
        type Session = ProductionSession;

        fn load_cache(
            &mut self,
        ) -> BackendFuture<'_, Result<Option<InventorySnapshot>, PublicError>> {
            Box::pin(async { Ok(None) })
        }

        fn start_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
            Box::pin(async move {
                self.master
                    .as_mut()
                    .ok_or_else(|| PublicError::new(PublicErrorKind::SshUnavailable))?
                    .verify()
                    .await
                    .map(|_| ())
                    .map_err(super::public_master_error)
            })
        }

        fn fetch_inventory(&mut self) -> BackendFuture<'_, Result<InventorySnapshot, PublicError>> {
            Box::pin(async {
                Ok(InventorySnapshot {
                    observed_at_unix_ms: 1,
                    stale: false,
                    vms: vec![VmInventoryItem {
                        vmid: VmId::new(107).unwrap(),
                        name: "SYNTHETIC-107".to_owned(),
                        node: NodeName::parse("pve2").unwrap(),
                        status: VmStatus::Running,
                        template: false,
                    }],
                })
            })
        }

        fn save_cache(
            &mut self,
            _snapshot: &InventorySnapshot,
        ) -> BackendFuture<'_, Result<(), PublicError>> {
            Box::pin(async { Ok(()) })
        }

        fn open_session(
            &mut self,
            vmid: VmId,
            options: OpenOptions,
        ) -> BackendFuture<'_, Result<Self::Session, PublicError>> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let master = self
                    .master
                    .as_mut()
                    .ok_or_else(|| PublicError::new(PublicErrorKind::SshUnavailable))?;
                let mut verified = master.verify().await.map_err(super::public_master_error)?;
                let proxy = TrustedSshProxy::connect(&mut verified, vmid)
                    .await
                    .map_err(super::public_proxy_error)?;
                ProductionSession::spawn(proxy, options)
            })
        }

        fn close_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
            Box::pin(async move {
                match self.master.as_mut() {
                    Some(master) => master.close().await.map_err(super::public_master_error),
                    None => Ok(()),
                }
            })
        }
    }

    async fn production_path_manager(
        runtime: &RuntimeDir,
        executable: PathBuf,
    ) -> (SshMaster, SessionManager, Arc<AtomicUsize>, u32) {
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let mut verified = master.verify().await.unwrap();
        let proxy = TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap())
            .await
            .unwrap();
        let session = ProductionSession::spawn(proxy, OpenOptions::default()).unwrap();
        let proxy_pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&proxy_pid_path).await;
        let proxy_pid = helper_pid(&proxy_pid_path);
        let opens = Arc::new(AtomicUsize::new(0));
        let mut manager = SessionManager::spawn(
            AppConfig::new(fixture_profile()),
            ProductionPathBackend {
                session: Some(session),
                opens: Arc::clone(&opens),
            },
        );
        timeout(Duration::from_secs(2), async {
            loop {
                if matches!(manager.recv().await, Some(AppEvent::LiveInventory(_))) {
                    break;
                }
            }
        })
        .await
        .expect("production-path manager did not publish live inventory");
        manager
            .send(AppCommand::Open {
                vmid: VmId::new(107).unwrap(),
                options: OpenOptions::default(),
            })
            .await
            .unwrap();
        (master, manager, opens, proxy_pid)
    }

    async fn production_wire_manager(
        runtime: &RuntimeDir,
        executable: PathBuf,
    ) -> (SessionManager, Arc<AtomicUsize>, u32) {
        fs::write(
            runtime.control_socket().with_extension("proxy_track_opens"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let opens = Arc::new(AtomicUsize::new(0));
        let mut manager = SessionManager::spawn(
            AppConfig::new(fixture_profile()),
            ProductionWireBackend {
                master: Some(master),
                opens: Arc::clone(&opens),
            },
        );
        timeout(Duration::from_secs(2), async {
            loop {
                if matches!(manager.recv().await, Some(AppEvent::LiveInventory(_))) {
                    break;
                }
            }
        })
        .await
        .expect("production-wire manager did not publish live inventory");
        manager
            .send(AppCommand::Open {
                vmid: VmId::new(107).unwrap(),
                options: OpenOptions::default(),
            })
            .await
            .unwrap();
        let opens_path = runtime.control_socket().with_extension("proxy.opens");
        wait_for_proxy_open_count(&opens_path, 1).await;
        let proxy_pid = fs::read_to_string(opens_path)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        (manager, opens, proxy_pid)
    }

    async fn assert_one_production_error_then_disconnected(
        manager: &mut SessionManager,
        expected: PublicErrorKind,
    ) {
        let mut terminal = Vec::new();
        let mut ready_count = 0;
        let mut framebuffer_count = 0;
        timeout(Duration::from_secs(2), async {
            while terminal.len() < 2 {
                match manager.recv().await.unwrap() {
                    AppEvent::Error(error) => {
                        assert_eq!(error.kind(), expected);
                        terminal.push("error");
                    }
                    AppEvent::SessionChanged(snapshot)
                        if snapshot.phase == SessionPhase::Disconnected =>
                    {
                        terminal.push("disconnected");
                    }
                    AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::Ready => {
                        ready_count += 1;
                    }
                    AppEvent::Framebuffer { .. } => framebuffer_count += 1,
                    _ => {}
                }
            }
        })
        .await
        .expect("production-path terminal sequence did not complete");
        sleep(Duration::from_millis(20)).await;
        while let Ok(event) = manager.try_recv() {
            match event {
                AppEvent::Error(error) => {
                    assert_eq!(error.kind(), expected);
                    terminal.push("error");
                }
                AppEvent::SessionChanged(snapshot)
                    if snapshot.phase == SessionPhase::Disconnected =>
                {
                    terminal.push("disconnected");
                }
                AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::Ready => {
                    ready_count += 1;
                }
                AppEvent::Framebuffer { .. } => framebuffer_count += 1,
                _ => {}
            }
        }
        assert_eq!(terminal, ["error", "disconnected"]);
        assert_eq!(ready_count, 0);
        assert_eq!(framebuffer_count, 0);
    }

    async fn wait_for_fixture_marker(path: &Path) {
        timeout(Duration::from_secs(2), async {
            while !path.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synthetic RFB fixture did not reach the required protocol stage");
    }

    async fn wait_for_proxy_open_count(path: &Path, expected: usize) {
        timeout(Duration::from_secs(2), async {
            loop {
                let observed = fs::read_to_string(path)
                    .map(|contents| contents.lines().count())
                    .unwrap_or(0);
                if observed >= expected {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synthetic proxy did not reach the expected open count");
    }

    #[derive(Clone, Copy)]
    enum ProductionCloseCase {
        Close,
        Reconnect,
        Shutdown,
    }

    async fn assert_production_key_release_wire_order(case: ProductionCloseCase) {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime
                .control_socket()
                .with_extension("proxy_rfb_input_capture"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let (mut manager, opens, proxy_pid) = production_wire_manager(&runtime, executable).await;

        let ready = timeout(Duration::from_secs(2), async {
            loop {
                if let Some(AppEvent::SessionChanged(snapshot)) = manager.recv().await {
                    if snapshot.phase == SessionPhase::Ready {
                        break snapshot.session_id;
                    }
                }
            }
        })
        .await
        .expect("production-path session did not become ready");
        manager
            .send(AppCommand::SendInput {
                session_id: ready,
                action: InputAction::Key {
                    down: true,
                    keysym: 0xffe3,
                },
            })
            .await
            .unwrap();
        wait_for_fixture_marker(
            &runtime
                .control_socket()
                .with_extension("proxy.key-down-read"),
        )
        .await;

        match case {
            ProductionCloseCase::Close | ProductionCloseCase::Reconnect => {
                let command = match case {
                    ProductionCloseCase::Close => AppCommand::Close { session_id: ready },
                    ProductionCloseCase::Reconnect => AppCommand::Reconnect { session_id: ready },
                    ProductionCloseCase::Shutdown => unreachable!(),
                };
                manager.send(command).await.unwrap();
                timeout(Duration::from_secs(2), async {
                    loop {
                        if let Some(AppEvent::SessionChanged(snapshot)) = manager.recv().await {
                            if snapshot.session_id == ready
                                && snapshot.phase == SessionPhase::Disconnected
                            {
                                assert!(runtime
                                    .control_socket()
                                    .with_extension("proxy.key-up-read")
                                    .exists());
                                break;
                            }
                        }
                    }
                })
                .await
                .expect("production close completed without the wire key-up");
                manager.shutdown().await.unwrap();
            }
            ProductionCloseCase::Shutdown => {
                manager.shutdown().await.unwrap();
                assert!(runtime
                    .control_socket()
                    .with_extension("proxy.key-up-read")
                    .exists());
            }
        }

        if matches!(case, ProductionCloseCase::Reconnect) {
            wait_for_proxy_open_count(&runtime.control_socket().with_extension("proxy.opens"), 2)
                .await;
            wait_for_fixture_marker(
                &runtime
                    .control_socket()
                    .with_extension("proxy.input-capture-skipped"),
            )
            .await;
        }
        wait_for_fixture_marker(&runtime.control_socket().with_extension("proxy.key-up-read"))
            .await;
        assert!(runtime
            .control_socket()
            .with_extension("proxy.input-capture-claimed")
            .exists());
        assert_eq!(
            fs::read(runtime.control_socket().with_extension("proxy.input-wire")).unwrap(),
            [
                4, 1, 0, 0, 0, 0, 0xff, 0xe3, // Control_L down
                4, 0, 0, 0, 0, 0, 0xff, 0xe3, // Control_L up
            ]
        );
        let expected_opens = match case {
            ProductionCloseCase::Reconnect => 2,
            ProductionCloseCase::Close | ProductionCloseCase::Shutdown => 1,
        };
        assert_eq!(opens.load(Ordering::SeqCst), expected_opens);
        assert_exact_pid_is_gone(proxy_pid).await;
        for replacement_pid in
            fs::read_to_string(runtime.control_socket().with_extension("proxy.opens"))
                .unwrap()
                .lines()
                .skip(1)
                .map(|line| line.parse().unwrap())
        {
            assert_exact_pid_is_gone(replacement_pid).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_production_close_reconnect_and_shutdown_write_modifier_release_before_completion()
    {
        let _process_guard = crate::ssh::process_test_guard().await;
        for case in [
            ProductionCloseCase::Close,
            ProductionCloseCase::Reconnect,
            ProductionCloseCase::Shutdown,
        ] {
            assert_production_key_release_wire_order(case).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_post_ack_shutdown_stall_aborts_vnc_task_and_reaps_owned_proxy() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime
                .control_socket()
                .with_extension("proxy_rfb_input_capture"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        fs::write(
            runtime
                .control_socket()
                .with_extension("proxy_input_capture_hang_after_keyup"),
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
        let mut session = ProductionSession::spawn(proxy, OpenOptions::default()).unwrap();
        let proxy_pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&proxy_pid_path).await;
        let proxy_pid = helper_pid(&proxy_pid_path);
        timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    session.try_recv().unwrap(),
                    Some(SessionTransportEvent::Framebuffer(ref rects)) if !rects.is_empty()
                ) {
                    session.mark_ready();
                    return;
                }
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("real stalled-shutdown session did not become ready");
        session
            .send_input(InputAction::Key {
                down: true,
                keysym: 0xffe3,
            })
            .unwrap();
        wait_for_fixture_marker(
            &runtime
                .control_socket()
                .with_extension("proxy.key-down-read"),
        )
        .await;

        let error = timeout(Duration::from_secs(2), session.close(close_deadline()))
            .await
            .expect("post-ack shutdown stall exceeded the total close policy")
            .unwrap_err();

        assert_eq!(error.kind(), PublicErrorKind::Cleanup);
        wait_for_fixture_marker(&runtime.control_socket().with_extension("proxy.key-up-read"))
            .await;
        assert_exact_pid_is_gone(proxy_pid).await;
        master.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_rfb_negotiation_failure_reaches_manager_and_reaps_exact_proxy() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime
                .control_socket()
                .with_extension("proxy_rfb_security_failure"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let (mut master, mut manager, opens, proxy_pid) =
            production_path_manager(&runtime, executable).await;

        wait_for_fixture_marker(
            &runtime
                .control_socket()
                .with_extension("proxy.negotiation-failure-sent"),
        )
        .await;
        assert_one_production_error_then_disconnected(&mut manager, PublicErrorKind::RfbSecurity)
            .await;
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert_exact_pid_is_gone(proxy_pid).await;
        manager.shutdown().await.unwrap();
        master.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_real_first_frame_never_reaches_ready_and_reaps_exact_proxy() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime
                .control_socket()
                .with_extension("proxy_malformed_first_frame"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let (mut master, mut manager, opens, proxy_pid) =
            production_path_manager(&runtime, executable).await;

        wait_for_fixture_marker(
            &runtime
                .control_socket()
                .with_extension("proxy.first-frame-sent"),
        )
        .await;
        assert_one_production_error_then_disconnected(&mut manager, PublicErrorKind::RfbProtocol)
            .await;
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert_exact_pid_is_gone(proxy_pid).await;
        manager.shutdown().await.unwrap();
        master.close().await.unwrap();
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
        let mut session = ProductionSession::spawn(proxy, OpenOptions::default()).unwrap();
        let proxy_pid_path = runtime.control_socket().with_extension("proxy.pid");
        wait_for(&proxy_pid_path).await;
        let proxy_pid = helper_pid(&proxy_pid_path);

        timeout(Duration::from_secs(2), session.close(close_deadline()))
            .await
            .expect("pre-session cancellation did not complete")
            .unwrap();
        assert_exact_pid_is_gone(proxy_pid).await;
        master.close().await.unwrap();
    }
}
