use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use rustedoutclient::{
    config::AppConfig,
    connection::FbRect,
    model::{NodeName, PveProfile, SshTarget, VmId},
    session::{
        AppCommand, AppEvent, BackendFuture, DesktopSize, InputAction, ManagedSession, OpenOptions,
        PublicError, PublicErrorKind, ResizeProtocolOutcome, ResizeStatus, SessionBackend,
        SessionId, SessionManager, SessionPhase, SessionSnapshot, SessionTransportEvent,
    },
    ssh::{InventorySnapshot, VmInventoryItem, VmStatus},
    vnc::{
        encode_set_desktop_size, encode_set_encodings, normalize_resize_request,
        parse_extended_desktop_size, ClipboardText, ExtendedDesktopSize, InputError,
        ProtocolLimits,
    },
};

fn size(width: u16, height: u16) -> DesktopSize {
    DesktopSize::new(width, height)
}

fn one_screen_payload(id: u32, x: u16, y: u16, width: u16, height: u16, flags: u32) -> Vec<u8> {
    screens_payload(&[(id, x, y, width, height, flags)])
}

fn screens_payload(screens: &[(u32, u16, u16, u16, u16, u32)]) -> Vec<u8> {
    let mut payload = vec![screens.len() as u8, 0, 0, 0];
    for (id, x, y, width, height, flags) in screens {
        payload.extend_from_slice(&id.to_be_bytes());
        payload.extend_from_slice(&x.to_be_bytes());
        payload.extend_from_slice(&y.to_be_bytes());
        payload.extend_from_slice(&width.to_be_bytes());
        payload.extend_from_slice(&height.to_be_bytes());
        payload.extend_from_slice(&flags.to_be_bytes());
    }
    payload
}

#[test]
fn set_encodings_and_set_desktop_size_wire_bytes_are_exact() {
    let encodings = encode_set_encodings();
    assert_eq!(encodings[0], 2);
    assert_eq!(&encodings[1..4], &[0, 0, 7]);
    let advertised = encodings[4..]
        .chunks_exact(4)
        .map(|bytes| i32::from_be_bytes(bytes.try_into().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(advertised, [16, 5, 1, 0, -223, -308, 7]);

    for (requested, expected) in [
        (
            size(1_600, 900),
            vec![
                251, 0, 0x06, 0x40, 0x03, 0x84, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x06, 0x40, 0x03,
                0x84, 0, 0, 0, 0,
            ],
        ),
        (
            size(1_920, 1_080),
            vec![
                251, 0, 0x07, 0x80, 0x04, 0x38, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x07, 0x80, 0x04,
                0x38, 0, 0, 0, 0,
            ],
        ),
    ] {
        assert_eq!(expected.len(), 24, "SetDesktopSize is exactly 24 bytes");
        assert_eq!(
            encode_set_desktop_size(requested, ProtocolLimits::default())
                .unwrap()
                .as_slice(),
            expected
        );
    }
}

#[test]
fn backing_pixel_requests_round_down_and_reject_before_queue_limits() {
    let limits = ProtocolLimits::default();
    assert_eq!(
        normalize_resize_request(1_600, 900, limits).unwrap(),
        size(1_600, 896)
    );
    assert_eq!(
        normalize_resize_request(1_920, 1_080, limits).unwrap(),
        size(1_920, 1_080)
    );
    assert_eq!(
        normalize_resize_request(640, 480, limits).unwrap(),
        size(640, 480)
    );

    for invalid in [
        (639, 480),
        (640, 479),
        (8_193, 1_000),
        (1_000, 8_193),
        (8_192, 4_097),
        (u32::MAX, u32::MAX),
    ] {
        assert!(
            normalize_resize_request(invalid.0, invalid.1, limits).is_err(),
            "invalid backing viewport {invalid:?} entered resize policy"
        );
    }
    assert_eq!(
        normalize_resize_request(8_192, 4_096, limits).unwrap(),
        size(8_192, 4_096)
    );
}

#[test]
fn extended_desktop_size_distinguishes_pending_actual_rejected_and_unsupported() {
    let limits = ProtocolLimits::default();
    let target = size(1_600, 900);
    let exact = one_screen_payload(0, 0, 0, 1_600, 900, 0);

    assert_eq!(
        parse_extended_desktop_size(1, 0, target, &exact, limits).unwrap(),
        ExtendedDesktopSize::Pending(target),
        "client-reason success only means QEMU forwarded the request"
    );
    assert_eq!(
        parse_extended_desktop_size(0, 0, target, &exact, limits).unwrap(),
        ExtendedDesktopSize::ServerSize(target)
    );
    for result in [1, 2] {
        assert_eq!(
            parse_extended_desktop_size(1, result, target, &exact, limits).unwrap(),
            ExtendedDesktopSize::Rejected
        );
    }
    assert_eq!(
        parse_extended_desktop_size(1, 3, target, &exact, limits).unwrap(),
        ExtendedDesktopSize::Unsupported
    );

    let two_screens = screens_payload(&[(0, 0, 0, 800, 900, 0), (1, 800, 0, 800, 900, 0)]);
    assert_eq!(
        parse_extended_desktop_size(0, 0, target, &two_screens, limits).unwrap(),
        ExtendedDesktopSize::Unsupported,
        "a structurally valid layout outside the one-screen subset is nonterminal"
    );
}

#[test]
fn malformed_extended_desktop_size_is_terminal_and_never_allocates_from_wire_counts() {
    let limits = ProtocolLimits::default();
    let target = size(1_600, 900);
    let exact = one_screen_payload(0, 0, 0, 1_600, 900, 0);

    for malformed in [
        Vec::new(),
        vec![1, 0, 0, 0],
        exact[..exact.len() - 1].to_vec(),
        {
            let mut trailing = exact.clone();
            trailing.push(0);
            trailing
        },
        one_screen_payload(0, 0, 0, 1_592, 900, 0),
        one_screen_payload(0, 1_599, 0, 2, 900, 0),
        one_screen_payload(0, 0, 0, 0, 900, 0),
    ] {
        assert!(parse_extended_desktop_size(0, 0, target, &malformed, limits).is_err());
    }

    let mut impossible_count = vec![255, 0, 0, 0];
    impossible_count.extend_from_slice(&[0; 16]);
    assert!(parse_extended_desktop_size(0, 0, target, &impossible_count, limits).is_err());
    assert!(parse_extended_desktop_size(9, 0, target, &exact, limits).is_err());
    assert!(parse_extended_desktop_size(1, 9, target, &exact, limits).is_err());
}

#[derive(Default)]
struct FakeState {
    events: VecDeque<SessionTransportEvent>,
    resize_requests: Vec<DesktopSize>,
    opens: Vec<OpenOptions>,
    input_results: VecDeque<Result<(), InputError>>,
}

#[derive(Clone, Default)]
struct FakeControl(Arc<Mutex<FakeState>>);

impl FakeControl {
    fn push_event(&self, event: SessionTransportEvent) {
        self.0.lock().unwrap().events.push_back(event);
    }

    fn resize_requests(&self) -> Vec<DesktopSize> {
        self.0.lock().unwrap().resize_requests.clone()
    }

    fn opens(&self) -> Vec<OpenOptions> {
        self.0.lock().unwrap().opens.clone()
    }

    fn reject_next_input(&self, error: InputError) {
        self.0.lock().unwrap().input_results.push_back(Err(error));
    }
}

struct FakeBackend {
    control: FakeControl,
}

struct FakeSession {
    control: FakeControl,
}

impl ManagedSession for FakeSession {
    fn try_recv(&mut self) -> Result<Option<SessionTransportEvent>, PublicError> {
        Ok(self.control.0.lock().unwrap().events.pop_front())
    }

    fn mark_ready(&mut self) {}

    fn send_input(&mut self, _action: InputAction) -> Result<Option<ClipboardText>, InputError> {
        self.control
            .0
            .lock()
            .unwrap()
            .input_results
            .pop_front()
            .unwrap_or(Ok(()))
            .map(|()| None)
    }

    fn request_resize(&mut self, requested: DesktopSize) -> Result<(), PublicError> {
        self.control
            .0
            .lock()
            .unwrap()
            .resize_requests
            .push(requested);
        Ok(())
    }

    fn release_all_keys(&mut self) -> Result<(), PublicError> {
        Ok(())
    }

    fn close(
        &mut self,
        _deadline: tokio::time::Instant,
    ) -> BackendFuture<'_, Result<(), PublicError>> {
        Box::pin(async { Ok(()) })
    }
}

impl SessionBackend for FakeBackend {
    type Session = FakeSession;

    fn load_cache(&mut self) -> BackendFuture<'_, Result<Option<InventorySnapshot>, PublicError>> {
        Box::pin(async { Ok(None) })
    }

    fn start_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
        Box::pin(async { Ok(()) })
    }

    fn fetch_inventory(&mut self) -> BackendFuture<'_, Result<InventorySnapshot, PublicError>> {
        Box::pin(async { Ok(live_inventory()) })
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
        options: OpenOptions,
    ) -> BackendFuture<'_, Result<Self::Session, PublicError>> {
        self.control.0.lock().unwrap().opens.push(options);
        let control = self.control.clone();
        Box::pin(async move { Ok(FakeSession { control }) })
    }

    fn close_master(&mut self) -> BackendFuture<'_, Result<(), PublicError>> {
        Box::pin(async { Ok(()) })
    }
}

fn vmid(value: u32) -> VmId {
    VmId::new(value).unwrap()
}

fn app_config() -> AppConfig {
    AppConfig::new(PveProfile {
        name: "Synthetic lab".to_owned(),
        ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
        node: NodeName::parse("pve2").unwrap(),
    })
}

fn live_inventory() -> InventorySnapshot {
    InventorySnapshot::new(
        1,
        false,
        vec![
            VmInventoryItem {
                vmid: vmid(107),
                name: "LABZ1-CM01".to_owned(),
                node: NodeName::parse("pve2").unwrap(),
                status: VmStatus::Running,
                template: false,
            },
            VmInventoryItem {
                vmid: vmid(300),
                name: "LABZ1-DP01".to_owned(),
                node: NodeName::parse("pve2").unwrap(),
                status: VmStatus::Running,
                template: false,
            },
        ],
    )
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

async fn poll_worker() {
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    settle().await;
}

fn drain(manager: &mut SessionManager) -> Vec<AppEvent> {
    let mut events = Vec::new();
    while let Ok(event) = manager.try_recv() {
        events.push(event);
    }
    events
}

async fn wait_for_snapshot(
    manager: &mut SessionManager,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    for _ in 0..100 {
        for event in drain(manager) {
            if let AppEvent::SessionChanged(snapshot) = event {
                if predicate(&snapshot) {
                    return snapshot;
                }
            }
        }
        poll_worker().await;
    }
    panic!("expected session snapshot was not emitted")
}

async fn open_ready_session(
    manager: &mut SessionManager,
    control: &FakeControl,
    options: OpenOptions,
) -> SessionId {
    manager
        .send(AppCommand::Open {
            vmid: vmid(107),
            options,
        })
        .await
        .unwrap();
    let opening = wait_for_snapshot(manager, |snapshot| {
        snapshot.vmid == vmid(107) && snapshot.phase == SessionPhase::NegotiatingRfb
    })
    .await;
    control.push_event(SessionTransportEvent::DesktopSize(size(1_024, 768)));
    control.push_event(SessionTransportEvent::Framebuffer(vec![FbRect {
        x: 0,
        y: 0,
        w: 1,
        h: 1,
        rgba: vec![0, 0, 0, 255],
    }]));
    let ready = wait_for_snapshot(manager, |snapshot| {
        snapshot.session_id == opening.session_id && snapshot.phase == SessionPhase::Ready
    })
    .await;
    assert!(ready.dynamic_resolution_enabled);
    assert_eq!(ready.guest_size, Some(size(1_024, 768)));
    ready.session_id
}

fn latest_resize_status(events: Vec<AppEvent>, session_id: SessionId) -> Option<ResizeStatus> {
    events.into_iter().rev().find_map(|event| match event {
        AppEvent::SessionChanged(snapshot) if snapshot.session_id == session_id => {
            Some(snapshot.resize_status)
        }
        _ => None,
    })
}

#[tokio::test(start_paused = true)]
async fn worker_debounces_a_storm_keeps_one_in_flight_and_requires_an_actual_size_to_apply() {
    let control = FakeControl::default();
    let mut manager = SessionManager::spawn(
        app_config(),
        FakeBackend {
            control: control.clone(),
        },
    );
    settle().await;
    drain(&mut manager);
    let session_id = open_ready_session(&mut manager, &control, OpenOptions::default()).await;
    drain(&mut manager);

    manager
        .send(AppCommand::ViewportChanged {
            session_id,
            backing_width: 1_600,
            backing_height: 900,
        })
        .await
        .unwrap();
    settle().await;
    tokio::time::advance(std::time::Duration::from_millis(249)).await;
    settle().await;
    assert!(control.resize_requests().is_empty());
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(control.resize_requests(), [size(1_600, 896)]);
    assert_eq!(
        latest_resize_status(drain(&mut manager), session_id),
        Some(ResizeStatus::Requested(size(1_600, 896)))
    );

    control.push_event(SessionTransportEvent::ResizeOutcome(
        ResizeProtocolOutcome::Forwarded(size(1_600, 896)),
    ));
    poll_worker().await;
    let pending = wait_for_snapshot(&mut manager, |snapshot| {
        snapshot.session_id == session_id
            && snapshot.resize_status == ResizeStatus::Pending(size(1_600, 896))
    })
    .await;
    assert_eq!(pending.guest_size, Some(size(1_024, 768)));

    for index in 0..1_000_u32 {
        manager
            .send(AppCommand::ViewportChanged {
                session_id,
                backing_width: 1_600 + index % 200,
                backing_height: 1_001,
            })
            .await
            .unwrap();
    }
    settle().await;
    tokio::time::advance(std::time::Duration::from_millis(250)).await;
    settle().await;
    assert_eq!(
        control.resize_requests(),
        [size(1_600, 896)],
        "an in-flight request retains only a replacement; it does not queue a storm"
    );

    control.push_event(SessionTransportEvent::DesktopSize(size(1_600, 896)));
    poll_worker().await;
    let events = drain(&mut manager);
    assert!(events.iter().any(|event| matches!(
        event,
        AppEvent::SessionChanged(snapshot)
            if snapshot.session_id == session_id
                && snapshot.resize_status == ResizeStatus::Applied(size(1_600, 896))
    )));
    assert_eq!(
        control.resize_requests(),
        [size(1_600, 896), size(1_792, 1_000)],
        "only the newest stable replacement is released after actual application"
    );

    control.push_event(SessionTransportEvent::ResizeOutcome(
        ResizeProtocolOutcome::Rejected,
    ));
    poll_worker().await;
    assert_eq!(
        latest_resize_status(drain(&mut manager), session_id),
        Some(ResizeStatus::Rejected)
    );
    manager
        .send(AppCommand::ViewportChanged {
            session_id,
            backing_width: 1_920,
            backing_height: 1_080,
        })
        .await
        .unwrap();
    settle().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(control.resize_requests().len(), 2);

    manager
        .send(AppCommand::RetryDynamicResolution { session_id })
        .await
        .unwrap();
    settle().await;
    tokio::time::advance(std::time::Duration::from_millis(250)).await;
    settle().await;
    assert_eq!(control.resize_requests()[2], size(1_920, 1_080));
    manager.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn dynamic_toggle_cancels_pending_work_and_timeout_is_nonterminal_until_retry() {
    let control = FakeControl::default();
    let mut manager = SessionManager::spawn(
        app_config(),
        FakeBackend {
            control: control.clone(),
        },
    );
    settle().await;
    drain(&mut manager);
    let session_id = open_ready_session(&mut manager, &control, OpenOptions::default()).await;
    drain(&mut manager);

    manager
        .send(AppCommand::ViewportChanged {
            session_id,
            backing_width: 1_600,
            backing_height: 900,
        })
        .await
        .unwrap();
    manager
        .send(AppCommand::SetDynamicResolution {
            session_id,
            enabled: false,
        })
        .await
        .unwrap();
    settle().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    settle().await;
    assert!(control.resize_requests().is_empty());
    assert_eq!(
        latest_resize_status(drain(&mut manager), session_id),
        Some(ResizeStatus::Disabled)
    );

    manager
        .send(AppCommand::SetDynamicResolution {
            session_id,
            enabled: true,
        })
        .await
        .unwrap();
    settle().await;
    tokio::time::advance(std::time::Duration::from_millis(249)).await;
    settle().await;
    assert!(control.resize_requests().is_empty());
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(control.resize_requests(), [size(1_600, 896)]);

    control.push_event(SessionTransportEvent::ResizeOutcome(
        ResizeProtocolOutcome::Forwarded(size(1_600, 896)),
    ));
    poll_worker().await;
    drain(&mut manager);
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    settle().await;
    let timed_out = wait_for_snapshot(&mut manager, |snapshot| {
        snapshot.session_id == session_id && snapshot.resize_status == ResizeStatus::TimedOut
    })
    .await;
    assert_eq!(timed_out.phase, SessionPhase::Ready);

    manager
        .send(AppCommand::ViewportChanged {
            session_id,
            backing_width: 1_920,
            backing_height: 1_080,
        })
        .await
        .unwrap();
    settle().await;
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(control.resize_requests().len(), 1);

    manager
        .send(AppCommand::RetryDynamicResolution { session_id })
        .await
        .unwrap();
    settle().await;
    tokio::time::advance(std::time::Duration::from_millis(250)).await;
    settle().await;
    assert_eq!(control.resize_requests()[1], size(1_920, 1_080));
    manager.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reconnect_carries_sticky_view_only_and_dynamic_state_while_new_sessions_default_on() {
    let control = FakeControl::default();
    let mut manager = SessionManager::spawn(
        app_config(),
        FakeBackend {
            control: control.clone(),
        },
    );
    settle().await;
    drain(&mut manager);
    let session_id = open_ready_session(&mut manager, &control, OpenOptions::default()).await;
    drain(&mut manager);

    control.reject_next_input(InputError::QueueUnavailable);
    manager
        .send(AppCommand::SendInput {
            session_id,
            action: InputAction::SetViewOnly(true),
        })
        .await
        .unwrap();
    let sticky = wait_for_snapshot(&mut manager, |snapshot| {
        snapshot.session_id == session_id && snapshot.view_only
    })
    .await;
    assert!(
        sticky.view_only,
        "enabling view-only is sticky on release failure"
    );

    control.reject_next_input(InputError::QueueUnavailable);
    manager
        .send(AppCommand::SendInput {
            session_id,
            action: InputAction::SetViewOnly(false),
        })
        .await
        .unwrap();
    poll_worker().await;
    assert!(
        drain(&mut manager).into_iter().all(|event| !matches!(
            event,
            AppEvent::SessionChanged(snapshot)
                if snapshot.session_id == session_id && !snapshot.view_only
        )),
        "disabling view-only updates authoritative options only when accepted"
    );

    manager
        .send(AppCommand::SetDynamicResolution {
            session_id,
            enabled: false,
        })
        .await
        .unwrap();
    wait_for_snapshot(&mut manager, |snapshot| {
        snapshot.session_id == session_id && !snapshot.dynamic_resolution_enabled
    })
    .await;
    manager
        .send(AppCommand::Reconnect { session_id })
        .await
        .unwrap();
    wait_for_snapshot(&mut manager, |snapshot| {
        snapshot.vmid == vmid(107)
            && snapshot.session_id != session_id
            && snapshot.phase == SessionPhase::NegotiatingRfb
    })
    .await;
    let opens = control.opens();
    assert_eq!(opens.len(), 2);
    assert!(opens[1].view_only);
    assert!(!opens[1].dynamic_resolution);

    manager
        .send(AppCommand::Open {
            vmid: vmid(300),
            options: OpenOptions::default(),
        })
        .await
        .unwrap();
    wait_for_snapshot(&mut manager, |snapshot| {
        snapshot.vmid == vmid(300) && snapshot.phase == SessionPhase::NegotiatingRfb
    })
    .await;
    assert!(control.opens()[2].dynamic_resolution);
    manager.shutdown().await.unwrap();
}

#[test]
fn public_error_context_remains_content_free_for_resize_queue_failures() {
    let error =
        PublicError::new(PublicErrorKind::Queue).with_public_context(SessionId::new(), vmid(107));
    assert_eq!(error.kind(), PublicErrorKind::Queue);
    assert_eq!(error.to_string(), "bounded session queue is unavailable");
}
