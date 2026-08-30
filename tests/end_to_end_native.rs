use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use rustedoutclient::{
    app::{
        dispatch_action, AppCommandSink, AppState, ClipboardAdapter, ClipboardAdapterError,
        CommandQueueError, DispatchOutcome, UiAction,
    },
    config::AppConfig,
    connection::{DesktopSize, FbRect},
    model::{NodeName, PveProfile, ScaleMode, SshTarget, VmId},
    session::{
        AppCommand, AppEvent, BackendFuture, InputAction, ManagedSession, OpenOptions, PublicError,
        PublicErrorKind, ResizeProtocolOutcome, ResizeStatus, SessionBackend, SessionId,
        SessionManager, SessionPhase, SessionSnapshot, SessionTransportEvent,
    },
    ssh::{InventorySnapshot, VmInventoryItem, VmStatus},
    vnc::{ClipboardText, InputError},
};
use tokio::time::{sleep, timeout, Instant};

mod support {
    pub mod rfb_peer;
}

fn vmid(value: u32) -> VmId {
    VmId::new(value).unwrap()
}

fn profile() -> PveProfile {
    PveProfile {
        name: "Synthetic profile".to_owned(),
        ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
        node: NodeName::parse("pve2").unwrap(),
    }
}

fn item(value: u32, name: &str, status: VmStatus) -> VmInventoryItem {
    VmInventoryItem {
        vmid: vmid(value),
        name: name.to_owned(),
        node: profile().node,
        status,
        template: false,
    }
}

fn snapshot(observed: u64, stale: bool, names: [&str; 3]) -> InventorySnapshot {
    InventorySnapshot::new(
        observed,
        stale,
        vec![
            item(107, names[0], VmStatus::Running),
            item(205, names[1], VmStatus::Running),
            item(301, names[2], VmStatus::Running),
        ],
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InputFact {
    Key(bool, u32),
    Pointer(u8, u16, u16),
    ReleaseOwned(Option<(u16, u16)>),
    Cad,
    ReleaseAll,
    ViewOnly(bool),
    Clipboard(usize),
    ReceiveClipboard,
}

#[derive(Default)]
struct SessionControl {
    events: Mutex<VecDeque<SessionTransportEvent>>,
    inputs: Mutex<Vec<InputFact>>,
    resize_requests: Mutex<Vec<DesktopSize>>,
    ready: Mutex<bool>,
    closed: Mutex<bool>,
}

#[derive(Default)]
struct Evidence {
    latest_live: Option<InventorySnapshot>,
    revalidations: Vec<VmId>,
    authorization_generations: Vec<u64>,
    next_generation: u64,
    sessions: Vec<Arc<SessionControl>>,
    active_sessions: usize,
    master_active: bool,
    saves: usize,
}

struct SyntheticSession {
    control: Arc<SessionControl>,
    evidence: Arc<Mutex<Evidence>>,
    owner_released: bool,
}

impl SyntheticSession {
    fn release_owner(&mut self) {
        if self.owner_released {
            return;
        }
        self.owner_released = true;
        let mut evidence = self.evidence.lock().unwrap();
        evidence.active_sessions = evidence.active_sessions.saturating_sub(1);
        *self.control.closed.lock().unwrap() = true;
    }
}

impl Drop for SyntheticSession {
    fn drop(&mut self) {
        self.release_owner();
    }
}

impl ManagedSession for SyntheticSession {
    fn try_recv(&mut self) -> Result<Option<SessionTransportEvent>, PublicError> {
        Ok(self.control.events.lock().unwrap().pop_front())
    }

    fn mark_ready(&mut self) {
        *self.control.ready.lock().unwrap() = true;
    }

    fn send_input(&mut self, action: InputAction) -> Result<Option<ClipboardText>, InputError> {
        let fact = match action {
            InputAction::Key { down, keysym } => InputFact::Key(down, keysym),
            InputAction::Pointer { buttons, x, y } => InputFact::Pointer(buttons, x, y),
            InputAction::ReleaseOwnedInput { pointer_position } => {
                InputFact::ReleaseOwned(pointer_position)
            }
            InputAction::CtrlAltDelete => InputFact::Cad,
            InputAction::ReleaseAllKeys => InputFact::ReleaseAll,
            InputAction::SetViewOnly(enabled) => InputFact::ViewOnly(enabled),
            InputAction::SendClipboard(text) => InputFact::Clipboard(text.len()),
            InputAction::ReceiveClipboard => InputFact::ReceiveClipboard,
        };
        self.control.inputs.lock().unwrap().push(fact);
        Ok(None)
    }

    fn request_resize(&mut self, requested: DesktopSize) -> Result<(), PublicError> {
        self.control.resize_requests.lock().unwrap().push(requested);
        Ok(())
    }

    fn release_all_keys(&mut self) -> Result<(), PublicError> {
        self.control
            .inputs
            .lock()
            .unwrap()
            .push(InputFact::ReleaseAll);
        Ok(())
    }

    fn close(&mut self, _deadline: Instant) -> BackendFuture<'_, Result<(), PublicError>> {
        Box::pin(async move {
            self.release_owner();
            Ok(())
        })
    }
}

struct SyntheticBackend {
    cache: Option<InventorySnapshot>,
    live: VecDeque<InventorySnapshot>,
    evidence: Arc<Mutex<Evidence>>,
    open_delay: Duration,
}

impl SyntheticBackend {
    fn new(
        cache: Option<InventorySnapshot>,
        live: impl IntoIterator<Item = InventorySnapshot>,
    ) -> (Self, Arc<Mutex<Evidence>>) {
        let evidence = Arc::new(Mutex::new(Evidence::default()));
        (
            Self {
                cache,
                live: live.into_iter().collect(),
                evidence: Arc::clone(&evidence),
                open_delay: Duration::ZERO,
            },
            evidence,
        )
    }

    fn with_open_delay(mut self, delay: Duration) -> Self {
        self.open_delay = delay;
        self
    }
}

impl SessionBackend for SyntheticBackend {
    type Session = SyntheticSession;

    fn load_cache(&mut self) -> BackendFuture<'_, Result<Option<InventorySnapshot>, PublicError>> {
        let cache = self.cache.take();
        Box::pin(async move { Ok(cache) })
    }

    fn start_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
        self.evidence.lock().unwrap().master_active = true;
        Box::pin(async { Ok(()) })
    }

    fn fetch_inventory(&mut self) -> BackendFuture<'_, Result<InventorySnapshot, PublicError>> {
        let next = self
            .live
            .pop_front()
            .or_else(|| self.evidence.lock().unwrap().latest_live.clone())
            .ok_or_else(|| PublicError::new(PublicErrorKind::Inventory));
        if let Ok(snapshot) = &next {
            self.evidence.lock().unwrap().latest_live = Some(snapshot.clone());
        }
        Box::pin(async move { next })
    }

    fn save_cache(
        &mut self,
        _snapshot: &InventorySnapshot,
    ) -> BackendFuture<'_, Result<(), PublicError>> {
        self.evidence.lock().unwrap().saves += 1;
        Box::pin(async { Ok(()) })
    }

    fn open_session(
        &mut self,
        selected: VmId,
        _options: OpenOptions,
    ) -> BackendFuture<'_, Result<Self::Session, PublicError>> {
        let mut evidence = self.evidence.lock().unwrap();
        evidence.revalidations.push(selected);
        let openable = evidence.latest_live.as_ref().and_then(|snapshot| {
            snapshot
                .vms
                .iter()
                .find(|item| item.vmid == selected)
                .map(|item| item.status == VmStatus::Running)
        });
        let error = match openable {
            Some(true) => None,
            Some(false) => Some(PublicError::new(PublicErrorKind::VmNotRunning)),
            None => Some(PublicError::new(PublicErrorKind::VmNotFound)),
        };
        if let Some(error) = error {
            return Box::pin(async move { Err(error) });
        }
        evidence.next_generation = evidence.next_generation.saturating_add(1);
        let generation = evidence.next_generation;
        evidence.authorization_generations.push(generation);
        let control = Arc::new(SessionControl::default());
        control.events.lock().unwrap().extend([
            SessionTransportEvent::DesktopSize(DesktopSize::new(64, 64)),
            SessionTransportEvent::Framebuffer(vec![FbRect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
                rgba: support::rfb_peer::non_black_rgba(support::rfb_peer::EncodingCase::Raw),
            }]),
        ]);
        evidence.sessions.push(Arc::clone(&control));
        evidence.active_sessions += 1;
        let session = SyntheticSession {
            control,
            evidence: Arc::clone(&self.evidence),
            owner_released: false,
        };
        let delay = self.open_delay;
        Box::pin(async move {
            sleep(delay).await;
            Ok(session)
        })
    }

    fn close_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
        self.evidence.lock().unwrap().master_active = false;
        Box::pin(async { Ok(()) })
    }
}

fn config() -> AppConfig {
    let mut config = AppConfig::new(profile());
    config.inventory_refresh_seconds = 300;
    config
}

async fn recv_matching(
    manager: &mut SessionManager,
    mut predicate: impl FnMut(&AppEvent) -> bool,
) -> AppEvent {
    timeout(Duration::from_secs(3), async {
        loop {
            let event = manager.recv().await.expect("manager event channel closed");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("expected manager event was not emitted")
}

async fn open_ready(manager: &mut SessionManager, selected: VmId) -> SessionId {
    manager
        .send(AppCommand::Open {
            vmid: selected,
            options: OpenOptions::default(),
        })
        .await
        .unwrap();
    let ready = recv_matching(manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot)
            if snapshot.vmid == selected && snapshot.phase == SessionPhase::Ready)
    })
    .await;
    let AppEvent::SessionChanged(snapshot) = ready else {
        unreachable!()
    };
    recv_matching(manager, |event| {
        matches!(event, AppEvent::Framebuffer { session_id, rects }
            if *session_id == snapshot.session_id
                && rects.iter().any(|rect| rect.rgba.chunks_exact(4).any(|pixel| pixel[..3] != [0, 0, 0])))
    })
    .await;
    snapshot.session_id
}

async fn close_disconnected(manager: &mut SessionManager, session_id: SessionId) {
    manager
        .send(AppCommand::Close { session_id })
        .await
        .unwrap();
    recv_matching(manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot)
            if snapshot.session_id == session_id && snapshot.phase == SessionPhase::Disconnected)
    })
    .await;
}

#[tokio::test]
async fn cache_live_revalidation_reconnect_duplicate_focus_and_two_sessions_are_exact() {
    let (backend, evidence) = SyntheticBackend::new(
        Some(snapshot(1, true, ["CACHE-107", "CACHE-205", "CACHE-301"])),
        [snapshot(2, false, ["LIVE-107", "LIVE-205", "LIVE-301"])],
    );
    let mut manager = SessionManager::spawn(config(), backend);
    let cached = recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::CachedInventory(_))
    })
    .await;
    let AppEvent::CachedInventory(cached) = cached else {
        unreachable!()
    };
    assert_eq!(cached.vms[0].name, "CACHE-107");
    assert!(evidence.lock().unwrap().revalidations.is_empty());
    let live = recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::LiveInventory(_))
    })
    .await;
    let AppEvent::LiveInventory(live) = live else {
        unreachable!()
    };
    assert_eq!(live.vms[0].name, "LIVE-107");

    let first = open_ready(&mut manager, vmid(107)).await;
    manager
        .send(AppCommand::Open {
            vmid: vmid(107),
            options: OpenOptions::default(),
        })
        .await
        .unwrap();
    let focused = recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::FocusExisting { .. })
    })
    .await;
    assert!(matches!(focused, AppEvent::FocusExisting { session_id } if session_id == first));
    assert_eq!(evidence.lock().unwrap().authorization_generations.len(), 1);

    manager
        .send(AppCommand::Reconnect { session_id: first })
        .await
        .unwrap();
    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot)
            if snapshot.session_id == first && snapshot.phase == SessionPhase::Disconnected)
    })
    .await;
    let replacement = recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot)
            if snapshot.vmid == vmid(107)
                && snapshot.session_id != first
                && snapshot.phase == SessionPhase::Ready)
    })
    .await;
    let AppEvent::SessionChanged(replacement) = replacement else {
        unreachable!()
    };
    let second = open_ready(&mut manager, vmid(205)).await;
    assert_ne!(replacement.session_id, second);

    {
        let locked = evidence.lock().unwrap();
        assert_eq!(locked.revalidations, [vmid(107), vmid(107), vmid(205)]);
        assert_eq!(locked.authorization_generations.len(), 3);
        assert!(locked
            .authorization_generations
            .windows(2)
            .all(|pair| pair[0] != pair[1]));
        assert_eq!(locked.active_sessions, 2);
    }

    manager
        .send(AppCommand::Open {
            vmid: vmid(301),
            options: OpenOptions::default(),
        })
        .await
        .unwrap();
    let capacity = recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::Error(error) if error.kind() == PublicErrorKind::Capacity)
    })
    .await;
    assert!(matches!(capacity, AppEvent::Error(_)));

    manager
        .send(AppCommand::SendInput {
            session_id: replacement.session_id,
            action: InputAction::CtrlAltDelete,
        })
        .await
        .unwrap();
    manager
        .send(AppCommand::SendInput {
            session_id: replacement.session_id,
            action: InputAction::SendClipboard("bounded synthetic clipboard".to_owned()),
        })
        .await
        .unwrap();
    sleep(Duration::from_millis(20)).await;
    assert_eq!(
        evidence.lock().unwrap().sessions[1]
            .inputs
            .lock()
            .unwrap()
            .as_slice(),
        [InputFact::Cad, InputFact::Clipboard(27)]
    );

    close_disconnected(&mut manager, replacement.session_id).await;
    close_disconnected(&mut manager, second).await;
    manager.shutdown().await.unwrap();
    let locked = evidence.lock().unwrap();
    assert_eq!(locked.active_sessions, 0);
    assert!(!locked.master_active);
}

#[tokio::test(start_paused = true)]
async fn manager_emits_ordered_monotonic_phase_timings_without_wall_clock_data() {
    use rustedoutclient::diagnostics::PhaseTiming;

    let (backend, evidence) = SyntheticBackend::new(
        None,
        [snapshot(2, false, ["LIVE-107", "LIVE-205", "LIVE-301"])],
    );
    let mut manager =
        SessionManager::spawn(config(), backend.with_open_delay(Duration::from_millis(25)));
    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::LiveInventory(_))
    })
    .await;
    manager
        .send(AppCommand::Open {
            vmid: vmid(107),
            options: OpenOptions::default(),
        })
        .await
        .unwrap();

    let mut timings: Vec<PhaseTiming> = Vec::new();
    timeout(Duration::from_secs(2), async {
        while timings.len() < 3 {
            if let Some(AppEvent::PhaseTiming { timing, .. }) = manager.recv().await {
                timings.push(timing);
            }
        }
    })
    .await
    .expect("manager did not publish phase timing telemetry");
    assert_eq!(
        timings
            .iter()
            .map(|timing| timing.phase())
            .collect::<Vec<_>>(),
        [
            SessionPhase::Opening,
            SessionPhase::StartingProxy,
            SessionPhase::NegotiatingRfb,
        ]
    );
    assert!(timings[1].duration_ms() >= 25);
    assert!(timings.iter().all(|timing| timing.duration_ms() < 2_000));

    manager.shutdown().await.unwrap();
    assert_eq!(evidence.lock().unwrap().active_sessions, 0);
}

async fn wait_resize_status(
    manager: &mut SessionManager,
    session_id: SessionId,
    expected: impl Fn(ResizeStatus) -> bool,
) -> SessionSnapshot {
    let event = recv_matching(manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot)
            if snapshot.session_id == session_id && expected(snapshot.resize_status))
    })
    .await;
    let AppEvent::SessionChanged(snapshot) = event else {
        unreachable!()
    };
    snapshot
}

#[derive(Default)]
struct NoClipboard;

impl ClipboardAdapter for NoClipboard {
    fn read_text(&mut self) -> Result<Option<String>, ClipboardAdapterError> {
        Ok(None)
    }
    fn write_text(&mut self, _text: String) -> Result<(), ClipboardAdapterError> {
        Ok(())
    }
}

struct NoSink;
impl AppCommandSink for NoSink {
    fn try_send(&self, _command: AppCommand) -> Result<(), CommandQueueError> {
        panic!("Fit is a pure local action")
    }
}

#[tokio::test(start_paused = true)]
async fn dynamic_resolution_acceptance_exceeds_1280_and_preserves_pending_applied_and_fit_truth() {
    let (backend, evidence) = SyntheticBackend::new(
        None,
        [snapshot(2, false, ["LIVE-107", "LIVE-205", "LIVE-301"])],
    );
    let mut manager = SessionManager::spawn(config(), backend);
    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::LiveInventory(_))
    })
    .await;
    let session_id = open_ready(&mut manager, vmid(107)).await;
    let control = Arc::clone(&evidence.lock().unwrap().sessions[0]);

    manager
        .send(AppCommand::ViewportChanged {
            session_id,
            backing_width: 1600,
            backing_height: 900,
        })
        .await
        .unwrap();
    tokio::time::advance(Duration::from_millis(251)).await;
    let requested = DesktopSize::new(1600, 896);
    wait_resize_status(&mut manager, session_id, |status| {
        status == ResizeStatus::Requested(requested)
    })
    .await;
    assert_eq!(
        control.resize_requests.lock().unwrap().as_slice(),
        [requested]
    );
    control
        .events
        .lock()
        .unwrap()
        .push_back(SessionTransportEvent::ResizeOutcome(
            ResizeProtocolOutcome::Forwarded(requested),
        ));
    wait_resize_status(&mut manager, session_id, |status| {
        status == ResizeStatus::Pending(requested)
    })
    .await;
    control
        .events
        .lock()
        .unwrap()
        .push_back(SessionTransportEvent::DesktopSize(requested));
    wait_resize_status(&mut manager, session_id, |status| {
        status == ResizeStatus::Applied(requested)
    })
    .await;

    manager
        .send(AppCommand::ViewportChanged {
            session_id,
            backing_width: 1920,
            backing_height: 1080,
        })
        .await
        .unwrap();
    tokio::time::advance(Duration::from_millis(251)).await;
    let large = DesktopSize::new(1920, 1080);
    wait_resize_status(&mut manager, session_id, |status| {
        status == ResizeStatus::Requested(large)
    })
    .await;
    control
        .events
        .lock()
        .unwrap()
        .push_back(SessionTransportEvent::ResizeOutcome(
            ResizeProtocolOutcome::Rejected,
        ));
    let rejected = wait_resize_status(&mut manager, session_id, |status| {
        status == ResizeStatus::Rejected
    })
    .await;

    manager
        .send(AppCommand::RetryDynamicResolution { session_id })
        .await
        .unwrap();
    tokio::time::advance(Duration::from_millis(251)).await;
    wait_resize_status(&mut manager, session_id, |status| {
        status == ResizeStatus::Requested(large)
    })
    .await;
    control
        .events
        .lock()
        .unwrap()
        .push_back(SessionTransportEvent::ResizeOutcome(
            ResizeProtocolOutcome::Unsupported,
        ));
    let unsupported = wait_resize_status(&mut manager, session_id, |status| {
        status == ResizeStatus::Unsupported
    })
    .await;
    assert!(control
        .resize_requests
        .lock()
        .unwrap()
        .iter()
        .all(|size| size.width > 1280));

    manager
        .send(AppCommand::RetryDynamicResolution { session_id })
        .await
        .unwrap();
    tokio::time::advance(Duration::from_millis(251)).await;
    wait_resize_status(&mut manager, session_id, |status| {
        status == ResizeStatus::Requested(large)
    })
    .await;
    tokio::time::advance(Duration::from_secs(2) + Duration::from_millis(1)).await;
    let timed_out = wait_resize_status(&mut manager, session_id, |status| {
        status == ResizeStatus::TimedOut
    })
    .await;

    let mut state = AppState::from_config(&config());
    let mut clipboard = NoClipboard;
    for snapshot in [rejected, unsupported, timed_out] {
        assert_eq!(snapshot.phase, SessionPhase::Ready);
        state.apply(AppEvent::SessionChanged(snapshot)).unwrap();
        assert_eq!(
            dispatch_action(&mut state, &NoSink, &mut clipboard, UiAction::FitToWindow),
            DispatchOutcome::AppliedLocally
        );
        assert_eq!(state.selected_session().unwrap().scale_mode, ScaleMode::Fit);
        assert_eq!(
            state.selected_session().unwrap().snapshot.phase,
            SessionPhase::Ready
        );
    }

    close_disconnected(&mut manager, session_id).await;
    manager.shutdown().await.unwrap();
    assert_eq!(evidence.lock().unwrap().active_sessions, 0);
}
