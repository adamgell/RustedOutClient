use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use rustedoutclient::{
    config::AppConfig,
    connection::FbRect,
    model::{NodeName, PveProfile, SshTarget, VmId},
    session::{
        AppCommand, AppEvent, BackendFuture, InputAction, ManagedSession, OpenOptions, PublicError,
        PublicErrorKind, SessionBackend, SessionId, SessionManager, SessionPhase, SessionSnapshot,
        SessionTransportEvent, APP_QUEUE_CAPACITY,
    },
    ssh::{InventorySnapshot, VmInventoryItem, VmStatus},
    vnc::{ClipboardText, InputController, InputError, InputSink, ProtocolLimits, VncOptions},
};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operation {
    LoadCache,
    StartMaster,
    FetchInventory,
    SaveCache,
    Open(VmId),
    CleanupFailedOpen(VmId),
    ReleaseKeys(VmId),
    Key(VmId, bool, u32),
    Pointer(VmId, u8, u16, u16),
    Clipboard(VmId, usize),
    CloseSession(VmId),
    CloseMaster,
}

#[derive(Default)]
struct FakeState {
    cached: Option<InventorySnapshot>,
    start_results: VecDeque<Result<(), PublicError>>,
    inventories: VecDeque<Result<InventorySnapshot, PublicError>>,
    open_results: VecDeque<Result<(), PublicError>>,
    key_results: VecDeque<Result<(), InputError>>,
    release_results: VecDeque<Result<(), PublicError>>,
    close_results: VecDeque<Result<(), PublicError>>,
    release_delay: Option<Duration>,
    release_started_at: Option<Instant>,
    release_finished_at: Option<Instant>,
    close_deadline: Option<Instant>,
    session_events: VecDeque<SessionTransportEvent>,
    operations: Vec<Operation>,
    tickets_generated: usize,
}

#[derive(Clone, Default)]
struct FakeControl(Arc<Mutex<FakeState>>);

impl FakeControl {
    fn with_cached_and_inventories(
        cached: Option<InventorySnapshot>,
        inventories: impl IntoIterator<Item = InventorySnapshot>,
    ) -> Self {
        let control = Self::default();
        {
            let mut state = control.0.lock().unwrap();
            state.cached = cached;
            state.inventories = inventories.into_iter().map(Ok).collect();
        }
        control
    }

    fn push_session_event(&self, event: SessionTransportEvent) {
        self.0.lock().unwrap().session_events.push_back(event);
    }

    fn operations(&self) -> Vec<Operation> {
        self.0.lock().unwrap().operations.clone()
    }

    fn tickets_generated(&self) -> usize {
        self.0.lock().unwrap().tickets_generated
    }

    fn close_timing(&self) -> (Instant, Instant, Instant) {
        let state = self.0.lock().unwrap();
        (
            state.release_started_at.unwrap(),
            state.release_finished_at.unwrap(),
            state.close_deadline.unwrap(),
        )
    }
}

struct FakeBackend {
    control: FakeControl,
}

struct FakeSession {
    vmid: VmId,
    control: FakeControl,
    input: InputController<FakeInputSink>,
}

struct FakeInputSink {
    vmid: VmId,
    control: FakeControl,
}

impl InputSink for FakeInputSink {
    fn key(&mut self, down: bool, keysym: u32) -> Result<(), InputError> {
        let mut state = self.control.0.lock().unwrap();
        state
            .operations
            .push(Operation::Key(self.vmid, down, keysym));
        state.key_results.pop_front().unwrap_or(Ok(()))
    }

    fn pointer(&mut self, buttons: u8, x: u16, y: u16) -> Result<(), InputError> {
        self.control
            .0
            .lock()
            .unwrap()
            .operations
            .push(Operation::Pointer(self.vmid, buttons, x, y));
        Ok(())
    }

    fn send_clipboard(&mut self, text: String) -> Result<(), InputError> {
        self.control
            .0
            .lock()
            .unwrap()
            .operations
            .push(Operation::Clipboard(self.vmid, text.len()));
        Ok(())
    }
}

impl ManagedSession for FakeSession {
    fn try_recv(&mut self) -> Result<Option<SessionTransportEvent>, PublicError> {
        Ok(self.control.0.lock().unwrap().session_events.pop_front())
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
            InputAction::ReleasePointer { x, y } => self.input.release_pointer(x, y).map(|()| None),
            InputAction::CtrlAltDelete => self.input.ctrl_alt_delete().map(|()| None),
            InputAction::ReleaseAllKeys => self.input.release_all_keys().map(|()| None),
            InputAction::FocusLost => self.input.focus_lost().map(|()| None),
            InputAction::SetViewOnly(enabled) => self.input.set_view_only(enabled).map(|()| None),
            InputAction::SendClipboard(text) => self.input.send_clipboard(text).map(|()| None),
            InputAction::ReceiveClipboard => self.input.receive_clipboard(),
        }
    }

    fn release_all_keys(&mut self) -> Result<(), PublicError> {
        let mut state = self.control.0.lock().unwrap();
        state.operations.push(Operation::ReleaseKeys(self.vmid));
        state.release_started_at = Some(Instant::now());
        let release_delay = state.release_delay;
        let configured = state.release_results.pop_front().unwrap_or(Ok(()));
        drop(state);
        if let Some(delay) = release_delay {
            std::thread::sleep(delay);
        }
        let release = self
            .input
            .release_all_keys()
            .map_err(public_input_cleanup_error);
        self.control.0.lock().unwrap().release_finished_at = Some(Instant::now());
        configured.and(release)
    }

    fn close(&mut self, deadline: Instant) -> BackendFuture<'_, Result<(), PublicError>> {
        let vmid = self.vmid;
        let control = self.control.clone();
        Box::pin(async move {
            let input_cleanup = self
                .input
                .clear_session()
                .map_err(|_| PublicError::new(PublicErrorKind::Queue));
            let mut state = control.0.lock().unwrap();
            state.operations.push(Operation::CloseSession(vmid));
            state.close_deadline = Some(deadline);
            let close = state.close_results.pop_front().unwrap_or(Ok(()));
            input_cleanup.and(close)
        })
    }
}

fn public_input_cleanup_error(error: InputError) -> PublicError {
    match error {
        InputError::QueueUnavailable => PublicError::new(PublicErrorKind::Queue),
        InputError::TransportDisconnected => PublicError::new(PublicErrorKind::Cleanup),
        _ => PublicError::new(PublicErrorKind::Cleanup),
    }
}

impl SessionBackend for FakeBackend {
    type Session = FakeSession;

    fn load_cache(&mut self) -> BackendFuture<'_, Result<Option<InventorySnapshot>, PublicError>> {
        let control = self.control.clone();
        Box::pin(async move {
            let mut state = control.0.lock().unwrap();
            state.operations.push(Operation::LoadCache);
            Ok(state.cached.clone())
        })
    }

    fn start_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
        let control = self.control.clone();
        Box::pin(async move {
            let mut state = control.0.lock().unwrap();
            state.operations.push(Operation::StartMaster);
            state.start_results.pop_front().unwrap_or(Ok(()))
        })
    }

    fn fetch_inventory(&mut self) -> BackendFuture<'_, Result<InventorySnapshot, PublicError>> {
        let control = self.control.clone();
        Box::pin(async move {
            let mut state = control.0.lock().unwrap();
            state.operations.push(Operation::FetchInventory);
            state.inventories.pop_front().unwrap_or_else(|| {
                Ok(InventorySnapshot {
                    observed_at_unix_ms: 99,
                    stale: false,
                    vms: Vec::new(),
                })
            })
        })
    }

    fn save_cache(
        &mut self,
        _snapshot: &InventorySnapshot,
    ) -> BackendFuture<'_, Result<(), PublicError>> {
        let control = self.control.clone();
        Box::pin(async move {
            control
                .0
                .lock()
                .unwrap()
                .operations
                .push(Operation::SaveCache);
            Ok(())
        })
    }

    fn open_session(
        &mut self,
        vmid: VmId,
        options: OpenOptions,
    ) -> BackendFuture<'_, Result<Self::Session, PublicError>> {
        let control = self.control.clone();
        Box::pin(async move {
            let mut state = control.0.lock().unwrap();
            state.operations.push(Operation::Open(vmid));
            if let Some(Err(error)) = state.open_results.pop_front() {
                state.operations.push(Operation::CleanupFailedOpen(vmid));
                return Err(error);
            }
            let current = state
                .inventories
                .front()
                .and_then(|result| result.as_ref().ok());
            let item = current.and_then(|snapshot| snapshot.vms.iter().find(|vm| vm.vmid == vmid));
            match item {
                None => return Err(PublicError::new(PublicErrorKind::VmNotFound)),
                Some(item) if item.status != VmStatus::Running => {
                    return Err(PublicError::new(PublicErrorKind::VmNotRunning));
                }
                Some(_) => {}
            }
            state.tickets_generated += 1;
            let sink = FakeInputSink {
                vmid,
                control: control.clone(),
            };
            let input = InputController::with_clipboard_limit(
                sink,
                options.view_only,
                options.clipboard_enabled,
                options.vnc.limits.max_clipboard_bytes as usize,
            )
            .map_err(|_| PublicError::new(PublicErrorKind::RfbLimit))?;
            Ok(FakeSession {
                vmid,
                control: control.clone(),
                input,
            })
        })
    }

    fn close_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
        let control = self.control.clone();
        Box::pin(async move {
            control
                .0
                .lock()
                .unwrap()
                .operations
                .push(Operation::CloseMaster);
            Ok(())
        })
    }
}

fn config() -> AppConfig {
    AppConfig::new(PveProfile {
        name: "Synthetic Proxmox".to_owned(),
        ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
        node: NodeName::parse("pve2").unwrap(),
    })
}

fn vm(vmid: u32, status: VmStatus) -> VmInventoryItem {
    VmInventoryItem {
        vmid: VmId::new(vmid).unwrap(),
        name: format!("vm-{vmid}"),
        node: NodeName::parse("pve2").unwrap(),
        status,
        template: false,
    }
}

fn inventory(observed_at_unix_ms: u64, vms: Vec<VmInventoryItem>) -> InventorySnapshot {
    InventorySnapshot {
        observed_at_unix_ms,
        stale: false,
        vms,
    }
}

fn backend(control: &FakeControl) -> FakeBackend {
    FakeBackend {
        control: control.clone(),
    }
}

async fn recv_matching<F>(manager: &mut SessionManager, mut predicate: F) -> AppEvent
where
    F: FnMut(&AppEvent) -> bool,
{
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = manager.recv().await.expect("manager event channel closed");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("timed out waiting for manager event")
}

async fn wait_for_live_inventory(manager: &mut SessionManager) {
    recv_matching(manager, |event| matches!(event, AppEvent::LiveInventory(_))).await;
}

fn operation_count(control: &FakeControl, expected: Operation) -> usize {
    control
        .operations()
        .iter()
        .filter(|operation| **operation == expected)
        .count()
}

async fn open_to_negotiation(manager: &mut SessionManager) -> SessionId {
    manager
        .send(AppCommand::Open {
            vmid: VmId::new(100).unwrap(),
            options: OpenOptions::default(),
        })
        .await
        .unwrap();
    match recv_matching(manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::NegotiatingRfb)
    })
    .await
    {
        AppEvent::SessionChanged(snapshot) => snapshot.session_id,
        _ => unreachable!(),
    }
}

async fn press_ready_key(
    manager: &mut SessionManager,
    control: &FakeControl,
    session_id: SessionId,
    keysym: u32,
) {
    control.push_session_event(SessionTransportEvent::Framebuffer(vec![FbRect {
        x: 0,
        y: 0,
        w: 1,
        h: 1,
        rgba: vec![0, 0, 0, 255],
    }]));
    recv_matching(manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot)
            if snapshot.session_id == session_id && snapshot.phase == SessionPhase::Ready)
    })
    .await;
    send_key_and_wait(manager, control, session_id, keysym).await;
}

async fn send_key_and_wait(
    manager: &SessionManager,
    control: &FakeControl,
    session_id: SessionId,
    keysym: u32,
) {
    manager
        .send(AppCommand::SendInput {
            session_id,
            action: InputAction::Key { down: true, keysym },
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while operation_count(
            control,
            Operation::Key(VmId::new(100).unwrap(), true, keysym),
        ) == 0
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("semantic key-down did not reach the real input controller");
}

fn assert_one_key_release(control: &FakeControl, keysym: u32) {
    assert_eq!(
        operation_count(
            control,
            Operation::Key(VmId::new(100).unwrap(), false, keysym),
        ),
        1,
        "lifecycle cleanup must release the tracked key exactly once in effect"
    );
}

async fn assert_one_error_then_disconnected(
    manager: &mut SessionManager,
    expected: PublicErrorKind,
) {
    let mut terminal_events = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while terminal_events.len() < 2 {
            match manager.recv().await.unwrap() {
                AppEvent::Error(error) => {
                    assert_eq!(error.kind(), expected);
                    terminal_events.push("error");
                }
                AppEvent::SessionChanged(snapshot)
                    if snapshot.phase == SessionPhase::Disconnected =>
                {
                    terminal_events.push("disconnected");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for terminal session events");
    tokio::time::sleep(Duration::from_millis(20)).await;
    while let Ok(event) = manager.try_recv() {
        match event {
            AppEvent::Error(error) => {
                assert_eq!(error.kind(), expected);
                terminal_events.push("error");
            }
            AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::Disconnected => {
                terminal_events.push("disconnected");
            }
            _ => {}
        }
    }
    assert_eq!(terminal_events, ["error", "disconnected"]);
}

async fn recv_one_error_then_disconnected(manager: &mut SessionManager) -> PublicError {
    let mut error = None;
    let mut disconnected = false;
    tokio::time::timeout(Duration::from_secs(2), async {
        while error.is_none() || !disconnected {
            match manager.recv().await.unwrap() {
                AppEvent::Error(observed) => {
                    assert!(
                        error.replace(observed).is_none(),
                        "duplicate terminal error"
                    );
                }
                AppEvent::SessionChanged(snapshot)
                    if snapshot.phase == SessionPhase::Disconnected =>
                {
                    disconnected = true;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for terminal session events");
    error.unwrap()
}

async fn assert_clean_disconnected(manager: &mut SessionManager) {
    let mut error_count = 0;
    let mut disconnected_count = 0;
    tokio::time::timeout(Duration::from_secs(2), async {
        while disconnected_count == 0 {
            match manager.recv().await.unwrap() {
                AppEvent::Error(_) => error_count += 1,
                AppEvent::SessionChanged(snapshot)
                    if snapshot.phase == SessionPhase::Disconnected =>
                {
                    disconnected_count += 1;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for clean disconnection");
    tokio::time::sleep(Duration::from_millis(20)).await;
    while let Ok(event) = manager.try_recv() {
        match event {
            AppEvent::Error(_) => error_count += 1,
            AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::Disconnected => {
                disconnected_count += 1;
            }
            _ => {}
        }
    }
    assert_eq!(error_count, 0);
    assert_eq!(disconnected_count, 1);
}

#[tokio::test]
async fn state_machine_reaches_ready_only_after_first_non_empty_frame_and_closes_in_order() {
    let live = inventory(2, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    assert_eq!(manager.command_capacity(), APP_QUEUE_CAPACITY);
    assert_eq!(manager.event_capacity(), APP_QUEUE_CAPACITY);
    wait_for_live_inventory(&mut manager).await;

    manager
        .send(AppCommand::Open {
            vmid: VmId::new(100).unwrap(),
            options: OpenOptions::default(),
        })
        .await
        .unwrap();

    let mut phases = Vec::new();
    while phases.last() != Some(&SessionPhase::NegotiatingRfb) {
        if let AppEvent::SessionChanged(snapshot) = manager.recv().await.unwrap() {
            phases.push(snapshot.phase);
        }
    }
    assert_eq!(
        phases,
        [
            SessionPhase::Opening,
            SessionPhase::StartingProxy,
            SessionPhase::NegotiatingRfb,
        ]
    );

    control.push_session_event(SessionTransportEvent::Framebuffer(Vec::new()));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!matches!(
        manager.try_recv(),
        Ok(AppEvent::SessionChanged(SessionSnapshot {
            phase: SessionPhase::Ready,
            ..
        }))
    ));

    control.push_session_event(SessionTransportEvent::Framebuffer(vec![FbRect {
        x: 0,
        y: 0,
        w: 1,
        h: 1,
        rgba: vec![0, 0, 0, 255],
    }]));
    let ready = recv_matching(&mut manager, |event| {
        matches!(
            event,
            AppEvent::SessionChanged(SessionSnapshot {
                phase: SessionPhase::Ready,
                ..
            })
        )
    })
    .await;
    let session_id = match ready {
        AppEvent::SessionChanged(snapshot) => snapshot.session_id,
        _ => unreachable!(),
    };
    send_key_and_wait(&manager, &control, session_id, 0x41).await;

    manager
        .send(AppCommand::Close { session_id })
        .await
        .unwrap();
    for expected in [SessionPhase::Disconnecting, SessionPhase::Disconnected] {
        recv_matching(&mut manager, |event| {
            matches!(event, AppEvent::SessionChanged(snapshot) if snapshot.phase == expected)
        })
        .await;
    }
    let operations = control.operations();
    let release_index = operations
        .iter()
        .position(|operation| *operation == Operation::ReleaseKeys(VmId::new(100).unwrap()))
        .unwrap();
    let close_index = operations
        .iter()
        .position(|operation| *operation == Operation::CloseSession(VmId::new(100).unwrap()))
        .unwrap();
    assert!(release_index < close_index);
    assert_one_key_release(&control, 0x41);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn manager_keeps_semantic_rejections_non_terminal_and_marks_controller_ready_from_frame() {
    let live = inventory(2, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let session_id = open_to_negotiation(&mut manager).await;

    manager
        .send(AppCommand::SendInput {
            session_id,
            action: InputAction::Key {
                down: true,
                keysym: 0x43,
            },
        })
        .await
        .unwrap();
    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::InputRejected { session_id: rejected, reason: InputError::NotReady }
            if *rejected == session_id)
    })
    .await;
    assert_eq!(
        operation_count(
            &control,
            Operation::Key(VmId::new(100).unwrap(), true, 0x43),
        ),
        0
    );

    press_ready_key(&mut manager, &control, session_id, 0x43).await;
    manager
        .send(AppCommand::SendInput {
            session_id,
            action: InputAction::SetViewOnly(true),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while operation_count(
            &control,
            Operation::Key(VmId::new(100).unwrap(), false, 0x43),
        ) == 0
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("view-only activation did not release the tracked key");
    manager
        .send(AppCommand::SendInput {
            session_id,
            action: InputAction::Pointer {
                buttons: 1,
                x: 10,
                y: 20,
            },
        })
        .await
        .unwrap();
    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::InputRejected { session_id: rejected, reason: InputError::ViewOnly }
            if *rejected == session_id)
    })
    .await;
    assert_eq!(
        operation_count(&control, Operation::CloseSession(VmId::new(100).unwrap())),
        0
    );

    manager.shutdown().await.unwrap();
    assert_one_key_release(&control, 0x43);
}

#[tokio::test]
async fn tightened_clipboard_limit_rejects_before_queue_without_terminating_session() {
    let live = inventory(2, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let options = OpenOptions {
        vnc: VncOptions {
            limits: ProtocolLimits {
                max_clipboard_bytes: 4,
                ..ProtocolLimits::default()
            },
            ..VncOptions::default()
        },
        clipboard_enabled: true,
        ..OpenOptions::default()
    };
    manager
        .send(AppCommand::Open {
            vmid: VmId::new(100).unwrap(),
            options,
        })
        .await
        .unwrap();
    let session_id = match recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::NegotiatingRfb)
    })
    .await
    {
        AppEvent::SessionChanged(snapshot) => snapshot.session_id,
        _ => unreachable!(),
    };
    control.push_session_event(SessionTransportEvent::Framebuffer(vec![FbRect {
        x: 0,
        y: 0,
        w: 1,
        h: 1,
        rgba: vec![0, 0, 0, 255],
    }]));
    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::Ready)
    })
    .await;

    manager
        .send(AppCommand::SendInput {
            session_id,
            action: InputAction::SendClipboard("1234".to_owned()),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while operation_count(&control, Operation::Clipboard(VmId::new(100).unwrap(), 4)) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    manager
        .send(AppCommand::SendInput {
            session_id,
            action: InputAction::SendClipboard("12345".to_owned()),
        })
        .await
        .unwrap();
    let rejection = recv_matching(&mut manager, |event| {
        matches!(
            event,
            AppEvent::InputRejected {
                reason: InputError::ClipboardTooLarge,
                ..
            }
        )
    })
    .await;
    assert!(matches!(
        rejection,
        AppEvent::InputRejected {
            session_id: rejected_session,
            reason: InputError::ClipboardTooLarge,
        } if rejected_session == session_id
    ));
    assert_eq!(
        operation_count(&control, Operation::Clipboard(VmId::new(100).unwrap(), 5)),
        0
    );

    send_key_and_wait(&manager, &control, session_id, 0x41).await;
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnect_and_terminal_error_each_release_a_real_controller_key_once() {
    for terminal in [
        SessionTransportEvent::Disconnected,
        SessionTransportEvent::Error(PublicError::new(PublicErrorKind::RfbProtocol)),
    ] {
        let live = inventory(2, vec![vm(100, VmStatus::Running)]);
        let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
        let mut manager = SessionManager::spawn(config(), backend(&control));
        wait_for_live_inventory(&mut manager).await;
        let session_id = open_to_negotiation(&mut manager).await;
        press_ready_key(&mut manager, &control, session_id, 0x44).await;

        control.push_session_event(terminal);
        recv_matching(&mut manager, |event| {
            matches!(event, AppEvent::SessionChanged(snapshot)
                if snapshot.session_id == session_id
                    && snapshot.phase == SessionPhase::Disconnected)
        })
        .await;
        assert_one_key_release(&control, 0x44);
        manager.shutdown().await.unwrap();
        assert_one_key_release(&control, 0x44);
    }
}

#[test]
fn transition_contract_rejects_ready_before_negotiation_and_reopen_without_cleanup() {
    let vmid = VmId::new(100).unwrap();
    let mut snapshot = SessionSnapshot::opening(SessionId::new(), "profile".to_owned(), vmid);
    assert!(snapshot.transition_to(SessionPhase::Ready).is_err());
    snapshot.transition_to(SessionPhase::StartingProxy).unwrap();
    snapshot
        .transition_to(SessionPhase::NegotiatingRfb)
        .unwrap();
    snapshot.transition_to(SessionPhase::Ready).unwrap();
    snapshot.transition_to(SessionPhase::Disconnecting).unwrap();
    snapshot.transition_to(SessionPhase::Disconnected).unwrap();
    assert!(snapshot.transition_to(SessionPhase::Opening).is_err());
}

#[tokio::test]
async fn cached_running_inventory_is_stale_live_stopped_replaces_it_and_open_makes_no_ticket() {
    let cached = inventory(1, vec![vm(100, VmStatus::Running)]);
    let stopped = inventory(2, vec![vm(100, VmStatus::Stopped)]);
    let control = FakeControl::with_cached_and_inventories(
        Some(cached.clone()),
        [stopped.clone(), stopped.clone()],
    );
    let mut manager = SessionManager::spawn(config(), backend(&control));

    match manager.recv().await.unwrap() {
        AppEvent::CachedInventory(snapshot) => {
            assert!(snapshot.stale);
            assert_eq!(snapshot.vms, cached.vms);
        }
        _ => panic!("cached inventory must be published first"),
    }
    match manager.recv().await.unwrap() {
        AppEvent::LiveInventory(snapshot) => assert_eq!(snapshot, stopped),
        _ => panic!("live inventory must replace cached inventory"),
    }

    manager
        .send(AppCommand::Open {
            vmid: VmId::new(100).unwrap(),
            options: OpenOptions::default(),
        })
        .await
        .unwrap();
    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::Error(error) if error.kind() == PublicErrorKind::VmNotRunning)
    })
    .await;
    assert_eq!(control.tickets_generated(), 0);
    manager.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn startup_refreshes_live_inventory_every_fifteen_seconds() {
    let snapshots = [
        inventory(1, vec![]),
        inventory(2, vec![]),
        inventory(3, vec![]),
    ];
    let control = FakeControl::with_cached_and_inventories(None, snapshots);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    assert_eq!(
        control
            .operations()
            .iter()
            .filter(|operation| **operation == Operation::FetchInventory)
            .count(),
        1
    );

    tokio::time::advance(Duration::from_secs(14)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        control
            .operations()
            .iter()
            .filter(|operation| **operation == Operation::FetchInventory)
            .count(),
        1
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for_live_inventory(&mut manager).await;
    assert_eq!(
        control
            .operations()
            .iter()
            .filter(|operation| **operation == Operation::FetchInventory)
            .count(),
        2
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_focuses_existing_two_distinct_sessions_are_admitted_and_third_is_rejected() {
    let live = inventory(
        1,
        vec![
            vm(100, VmStatus::Running),
            vm(101, VmStatus::Running),
            vm(102, VmStatus::Running),
        ],
    );
    let control = FakeControl::with_cached_and_inventories(
        None,
        [live.clone(), live.clone(), live.clone(), live],
    );
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;

    for vmid in [100, 100, 101, 102] {
        manager
            .send(AppCommand::Open {
                vmid: VmId::new(vmid).unwrap(),
                options: OpenOptions::default(),
            })
            .await
            .unwrap();
    }

    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::FocusExisting { .. })
    })
    .await;
    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::Error(error) if error.kind() == PublicErrorKind::Capacity)
    })
    .await;
    let opens = control
        .operations()
        .into_iter()
        .filter(|operation| matches!(operation, Operation::Open(_)))
        .count();
    assert_eq!(opens, 2);
    assert_eq!(control.tickets_generated(), 2);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconnect_closes_old_session_before_fresh_validation_and_proxy_open() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control =
        FakeControl::with_cached_and_inventories(None, [live.clone(), live.clone(), live.clone()]);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    manager
        .send(AppCommand::Open {
            vmid: VmId::new(100).unwrap(),
            options: OpenOptions::default(),
        })
        .await
        .unwrap();
    let opened = recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::NegotiatingRfb)
    })
    .await;
    let old_id = match opened {
        AppEvent::SessionChanged(snapshot) => snapshot.session_id,
        _ => unreachable!(),
    };
    press_ready_key(&mut manager, &control, old_id, 0x42).await;

    manager
        .send(AppCommand::Reconnect { session_id: old_id })
        .await
        .unwrap();
    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::Disconnected)
    })
    .await;
    let reopened = recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::Opening)
    })
    .await;
    let new_id = match reopened {
        AppEvent::SessionChanged(snapshot) => snapshot.session_id,
        _ => unreachable!(),
    };
    assert_ne!(old_id, new_id);

    let operations = control.operations();
    let close_index = operations
        .iter()
        .rposition(|operation| *operation == Operation::CloseSession(VmId::new(100).unwrap()))
        .unwrap();
    let reopen_index = operations
        .iter()
        .rposition(|operation| *operation == Operation::Open(VmId::new(100).unwrap()))
        .unwrap();
    assert!(close_index < reopen_index);
    assert_one_key_release(&control, 0x42);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn master_check_failure_uses_its_own_seam_and_closes_master_once() {
    let control = FakeControl::default();
    control
        .0
        .lock()
        .unwrap()
        .start_results
        .push_back(Err(PublicError::new(PublicErrorKind::HostKeyChanged)));
    let mut manager = SessionManager::spawn(config(), backend(&control));

    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::Error(error) if error.kind() == PublicErrorKind::HostKeyChanged)
    })
    .await;
    manager.shutdown().await.unwrap();

    assert_eq!(operation_count(&control, Operation::StartMaster), 1);
    assert_eq!(operation_count(&control, Operation::FetchInventory), 0);
    assert_eq!(operation_count(&control, Operation::CloseMaster), 1);
}

#[tokio::test]
async fn inventory_failure_uses_its_own_seam_and_closes_master_once() {
    let control = FakeControl::default();
    control
        .0
        .lock()
        .unwrap()
        .inventories
        .push_back(Err(PublicError::new(PublicErrorKind::Inventory)));
    let mut manager = SessionManager::spawn(config(), backend(&control));

    recv_matching(&mut manager, |event| {
        matches!(event, AppEvent::Error(error) if error.kind() == PublicErrorKind::Inventory)
    })
    .await;
    manager.shutdown().await.unwrap();

    assert_eq!(operation_count(&control, Operation::StartMaster), 1);
    assert_eq!(operation_count(&control, Operation::FetchInventory), 1);
    assert_eq!(operation_count(&control, Operation::CloseMaster), 1);
}

#[tokio::test]
async fn proxy_creation_failure_cleans_failed_open_and_never_reopens() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    control
        .0
        .lock()
        .unwrap()
        .open_results
        .push_back(Err(PublicError::new(PublicErrorKind::Proxy)));
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;

    manager
        .send(AppCommand::Open {
            vmid: VmId::new(100).unwrap(),
            options: OpenOptions::default(),
        })
        .await
        .unwrap();
    assert_one_error_then_disconnected(&mut manager, PublicErrorKind::Proxy).await;

    let vmid = VmId::new(100).unwrap();
    assert_eq!(operation_count(&control, Operation::Open(vmid)), 1);
    assert_eq!(
        operation_count(&control, Operation::CleanupFailedOpen(vmid)),
        1
    );
    assert_eq!(operation_count(&control, Operation::ReleaseKeys(vmid)), 0);
    assert_eq!(operation_count(&control, Operation::CloseSession(vmid)), 0);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn rfb_negotiation_failure_closes_once_and_never_reopens() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    open_to_negotiation(&mut manager).await;

    control.push_session_event(SessionTransportEvent::Error(PublicError::new(
        PublicErrorKind::RfbSecurity,
    )));
    assert_one_error_then_disconnected(&mut manager, PublicErrorKind::RfbSecurity).await;

    let vmid = VmId::new(100).unwrap();
    assert_eq!(operation_count(&control, Operation::Open(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::ReleaseKeys(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::CloseSession(vmid)), 1);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_frame_failure_closes_once_and_never_reopens() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    open_to_negotiation(&mut manager).await;

    control.push_session_event(SessionTransportEvent::Error(PublicError::new(
        PublicErrorKind::Decoder,
    )));
    assert_one_error_then_disconnected(&mut manager, PublicErrorKind::Decoder).await;

    let vmid = VmId::new(100).unwrap();
    assert_eq!(operation_count(&control, Operation::Open(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::ReleaseKeys(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::CloseSession(vmid)), 1);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn ui_cancellation_closes_once_without_reopening() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let session_id = open_to_negotiation(&mut manager).await;

    manager
        .send(AppCommand::Close { session_id })
        .await
        .unwrap();
    assert_clean_disconnected(&mut manager).await;

    let vmid = VmId::new(100).unwrap();
    assert_eq!(operation_count(&control, Operation::Open(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::ReleaseKeys(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::CloseSession(vmid)), 1);
    manager.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manager_close_deadline_starts_before_release_and_is_not_renewed_for_transport() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    control.0.lock().unwrap().release_delay = Some(Duration::from_millis(100));
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let session_id = open_to_negotiation(&mut manager).await;

    manager
        .send(AppCommand::Close { session_id })
        .await
        .unwrap();
    assert_clean_disconnected(&mut manager).await;

    let (release_started, release_finished, close_deadline) = control.close_timing();
    assert!(release_started < release_finished);
    assert!(release_started < close_deadline);
    assert!(
        close_deadline.duration_since(release_started) <= Duration::from_secs(3),
        "the absolute deadline must exist before manager-level release"
    );
    assert!(
        close_deadline.saturating_duration_since(release_finished) < Duration::from_secs(3),
        "transport close must receive the already-consumed deadline"
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnected_key_release_preserves_terminal_rfb_primary_with_cleanup_evidence() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    {
        let mut state = control.0.lock().unwrap();
        state
            .key_results
            .extend([Ok(()), Err(InputError::TransportDisconnected)]);
        state
            .close_results
            .push_back(Err(PublicError::new(PublicErrorKind::RfbSecurity)));
    }
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let session_id = open_to_negotiation(&mut manager).await;
    press_ready_key(&mut manager, &control, session_id, 0x51).await;

    manager
        .send(AppCommand::Close { session_id })
        .await
        .unwrap();
    let error = recv_one_error_then_disconnected(&mut manager).await;

    assert_eq!(error.kind(), PublicErrorKind::RfbSecurity);
    assert!(error.has_cleanup_failure());
    assert_one_key_release(&control, 0x51);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnected_key_release_without_terminal_primary_reports_cleanup() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    control
        .0
        .lock()
        .unwrap()
        .key_results
        .extend([Ok(()), Err(InputError::TransportDisconnected)]);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let session_id = open_to_negotiation(&mut manager).await;
    press_ready_key(&mut manager, &control, session_id, 0x52).await;

    manager
        .send(AppCommand::Close { session_id })
        .await
        .unwrap();
    let error = recv_one_error_then_disconnected(&mut manager).await;

    assert_eq!(error.kind(), PublicErrorKind::Cleanup);
    assert_one_key_release(&control, 0x52);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_queue_key_release_remains_queue_primary() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    {
        let mut state = control.0.lock().unwrap();
        state
            .key_results
            .extend([Ok(()), Err(InputError::QueueUnavailable)]);
        state
            .close_results
            .push_back(Err(PublicError::new(PublicErrorKind::RfbSecurity)));
    }
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let session_id = open_to_negotiation(&mut manager).await;
    press_ready_key(&mut manager, &control, session_id, 0x53).await;

    manager
        .send(AppCommand::Close { session_id })
        .await
        .unwrap();
    let error = recv_one_error_then_disconnected(&mut manager).await;

    assert_eq!(error.kind(), PublicErrorKind::Queue);
    assert!(error.has_cleanup_failure());
    assert_one_key_release(&control, 0x53);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn key_release_failure_still_closes_once_and_never_reopens() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    control
        .0
        .lock()
        .unwrap()
        .release_results
        .push_back(Err(PublicError::new(PublicErrorKind::Cleanup)));
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let session_id = open_to_negotiation(&mut manager).await;

    manager
        .send(AppCommand::Close { session_id })
        .await
        .unwrap();
    assert_one_error_then_disconnected(&mut manager, PublicErrorKind::Cleanup).await;

    let vmid = VmId::new(100).unwrap();
    assert_eq!(operation_count(&control, Operation::Open(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::ReleaseKeys(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::CloseSession(vmid)), 1);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_close_failure_reports_once_and_never_reopens() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control = FakeControl::with_cached_and_inventories(None, [live.clone(), live]);
    control
        .0
        .lock()
        .unwrap()
        .close_results
        .push_back(Err(PublicError::new(PublicErrorKind::Cleanup)));
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let session_id = open_to_negotiation(&mut manager).await;

    manager
        .send(AppCommand::Close { session_id })
        .await
        .unwrap();
    assert_one_error_then_disconnected(&mut manager, PublicErrorKind::Cleanup).await;

    let vmid = VmId::new(100).unwrap();
    assert_eq!(operation_count(&control, Operation::Open(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::ReleaseKeys(vmid)), 1);
    assert_eq!(operation_count(&control, Operation::CloseSession(vmid)), 1);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn failures_close_once_emit_one_typed_error_and_never_auto_reconnect() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    for kind in [
        PublicErrorKind::HostKeyUnknown,
        PublicErrorKind::HostKeyChanged,
        PublicErrorKind::SshAuthentication,
        PublicErrorKind::Inventory,
        PublicErrorKind::Proxy,
        PublicErrorKind::RfbSecurity,
        PublicErrorKind::RfbProtocol,
        PublicErrorKind::Decoder,
    ] {
        let transport_failure = matches!(
            kind,
            PublicErrorKind::RfbSecurity | PublicErrorKind::RfbProtocol | PublicErrorKind::Decoder
        );
        let control = FakeControl::with_cached_and_inventories(
            None,
            [live.clone(), live.clone(), live.clone()],
        );
        control
            .0
            .lock()
            .unwrap()
            .open_results
            .push_back(if transport_failure {
                Ok(())
            } else {
                Err(PublicError::new(kind))
            });
        let mut manager = SessionManager::spawn(config(), backend(&control));
        wait_for_live_inventory(&mut manager).await;
        manager
            .send(AppCommand::Open {
                vmid: VmId::new(100).unwrap(),
                options: OpenOptions::default(),
            })
            .await
            .unwrap();
        if transport_failure {
            recv_matching(&mut manager, |event| {
                matches!(event, AppEvent::SessionChanged(snapshot) if snapshot.phase == SessionPhase::NegotiatingRfb)
            })
            .await;
            control.push_session_event(SessionTransportEvent::Error(PublicError::new(kind)));
        }

        let mut error_count = 0;
        let mut disconnected_count = 0;
        while error_count == 0 || disconnected_count == 0 {
            match manager.recv().await.unwrap() {
                AppEvent::Error(error) if error.kind() == kind => error_count += 1,
                AppEvent::SessionChanged(snapshot)
                    if snapshot.phase == SessionPhase::Disconnected =>
                {
                    disconnected_count += 1
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        while let Ok(event) = manager.try_recv() {
            match event {
                AppEvent::Error(error) if error.kind() == kind => error_count += 1,
                AppEvent::SessionChanged(snapshot)
                    if snapshot.phase == SessionPhase::Disconnected =>
                {
                    disconnected_count += 1
                }
                _ => {}
            }
        }
        assert_eq!(error_count, 1);
        assert_eq!(disconnected_count, 1);
        assert_eq!(
            control
                .operations()
                .iter()
                .filter(|operation| matches!(operation, Operation::Open(_)))
                .count(),
            1
        );
        assert_eq!(
            control
                .operations()
                .iter()
                .filter(|operation| matches!(operation, Operation::CloseSession(_)))
                .count(),
            usize::from(transport_failure)
        );
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn shutdown_drains_a_full_app_queue_and_completes_owned_cleanup() {
    let live = inventory(1, vec![vm(100, VmStatus::Running)]);
    let control =
        FakeControl::with_cached_and_inventories(None, [live.clone(), live.clone(), live]);
    let mut manager = SessionManager::spawn(config(), backend(&control));
    wait_for_live_inventory(&mut manager).await;
    let session_id = open_to_negotiation(&mut manager).await;
    press_ready_key(&mut manager, &control, session_id, 0x45).await;

    for _ in 0..(APP_QUEUE_CAPACITY * 2) {
        control.push_session_event(SessionTransportEvent::Framebuffer(vec![FbRect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            rgba: vec![0, 0, 0, 255],
        }]));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.queued_event_count() < APP_QUEUE_CAPACITY {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("app queue did not fill");
    assert_eq!(manager.queued_event_count(), APP_QUEUE_CAPACITY);

    tokio::time::timeout(Duration::from_secs(2), manager.shutdown())
        .await
        .expect("shutdown deadlocked behind the full event queue")
        .unwrap();
    let operations = control.operations();
    assert_eq!(
        operations
            .iter()
            .filter(|operation| matches!(operation, Operation::CloseSession(_)))
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == Operation::CloseMaster)
            .count(),
        1
    );
    assert_one_key_release(&control, 0x45);
}
