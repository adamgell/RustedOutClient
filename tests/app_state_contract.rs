use std::cell::{Cell, RefCell};

use rustedoutclient::{
    app::{
        apply_ui_effects, dispatch_action, ActionAvailability, AppCommandSink, AppState,
        ClipboardAdapter, ClipboardAdapterError, ClipboardStatus, CommandQueueError,
        DispatchOutcome, FramebufferUploadKind, UiAction,
    },
    config::{AppConfig, FavoriteVm},
    connection::FbRect,
    model::{NodeName, PveProfile, ScaleMode, SshTarget, VmId},
    session::{
        AppCommand, AppEvent, DesktopSize, OpenOptions, PublicError, PublicErrorKind, ResizeStatus,
        SessionId, SessionPhase, SessionSnapshot,
    },
    ssh::{InventorySnapshot, VmInventoryItem, VmStatus},
    vnc::ClipboardText,
};

fn vmid(value: u32) -> VmId {
    VmId::new(value).unwrap()
}

fn config() -> AppConfig {
    let mut config = AppConfig::new(PveProfile {
        name: "Lab profile".to_owned(),
        ssh_target: SshTarget::parse("root@do-not-render.example.invalid").unwrap(),
        node: NodeName::parse("pve2").unwrap(),
    });
    config.clipboard_enabled = true;
    config.favorites = vec![
        FavoriteVm {
            vmid: vmid(205),
            alias: Some("Domain Controller".to_owned()),
            scale_mode: ScaleMode::Fit,
            view_only: false,
            sort_position: 1,
        },
        FavoriteVm {
            vmid: vmid(107),
            alias: Some("Primary Lab".to_owned()),
            scale_mode: ScaleMode::OneToOne,
            view_only: true,
            sort_position: 0,
        },
    ];
    config
}

fn inventory(observed_at_unix_ms: u64, stale: bool) -> InventorySnapshot {
    InventorySnapshot::new(
        observed_at_unix_ms,
        stale,
        vec![
            VmInventoryItem {
                vmid: vmid(300),
                name: "Build Agent".to_owned(),
                node: NodeName::parse("pve2").unwrap(),
                status: VmStatus::Running,
                template: false,
            },
            VmInventoryItem {
                vmid: vmid(205),
                name: "LABZ1-DC01".to_owned(),
                node: NodeName::parse("pve2").unwrap(),
                status: VmStatus::Stopped,
                template: false,
            },
            VmInventoryItem {
                vmid: vmid(107),
                name: "LABZ1-CM01".to_owned(),
                node: NodeName::parse("pve2").unwrap(),
                status: VmStatus::Running,
                template: false,
            },
        ],
    )
}

fn snapshot(
    session_id: SessionId,
    vmid: VmId,
    phase: SessionPhase,
    view_only: bool,
) -> SessionSnapshot {
    SessionSnapshot {
        session_id,
        profile_name: "Lab profile".to_owned(),
        vmid,
        phase,
        view_only,
        clipboard_enabled: true,
        dynamic_resolution_enabled: true,
        guest_size: Some(DesktopSize::new(1_024, 768)),
        resize_status: ResizeStatus::Waiting,
    }
}

fn configured_state() -> AppState {
    let mut state = AppState::from_config(&config());
    state
        .apply(AppEvent::LiveInventory(inventory(2_000, false)))
        .unwrap();
    state.select_inventory(Some(vmid(107)));
    state
}

#[test]
fn visible_actions_are_gated_for_no_session_connecting_ready_view_only_error_and_disconnected() {
    let mut state = configured_state();
    assert_eq!(
        state.action_availability(),
        ActionAvailability {
            open: true,
            reconnect: false,
            close: false,
            open_in_tigervnc: false,
            ctrl_alt_delete: false,
            release_all_keys: false,
            view_only: false,
            dynamic_resolution: false,
            retry_dynamic_resolution: false,
            fit_to_window: false,
            one_to_one: false,
            fullscreen: false,
            send_clipboard: false,
            receive_clipboard: false,
            diagnostics: true,
            keyboard: false,
            pointer: false,
        }
    );

    let session_id = SessionId::new();
    state
        .apply(AppEvent::SessionChanged(snapshot(
            session_id,
            vmid(107),
            SessionPhase::StartingProxy,
            false,
        )))
        .unwrap();
    state.set_viewport(session_id, 1_600, 900).unwrap();
    let connecting = state.action_availability();
    assert!(connecting.reconnect && connecting.close && connecting.release_all_keys);
    assert!(connecting.view_only && connecting.fit_to_window && connecting.one_to_one);
    assert!(!connecting.ctrl_alt_delete && !connecting.keyboard && !connecting.pointer);
    assert!(!connecting.send_clipboard && !connecting.dynamic_resolution);

    state
        .apply(AppEvent::SessionChanged(snapshot(
            session_id,
            vmid(107),
            SessionPhase::Ready,
            false,
        )))
        .unwrap();
    let ready = state.action_availability();
    assert!(ready.ctrl_alt_delete && ready.keyboard && ready.pointer);
    assert!(ready.send_clipboard && ready.receive_clipboard);
    assert!(ready.dynamic_resolution && ready.fit_to_window && ready.one_to_one);
    assert!(!ready.open_in_tigervnc, "Task 12 remains unavailable");

    state
        .apply(AppEvent::SessionChanged(snapshot(
            session_id,
            vmid(107),
            SessionPhase::Ready,
            true,
        )))
        .unwrap();
    let view_only = state.action_availability();
    assert!(!view_only.ctrl_alt_delete && !view_only.keyboard && !view_only.pointer);
    assert!(!view_only.send_clipboard && !view_only.receive_clipboard);
    assert!(view_only.release_all_keys && view_only.dynamic_resolution);

    state
        .apply(AppEvent::Error(
            PublicError::new(PublicErrorKind::RfbProtocol)
                .with_public_context(session_id, vmid(107)),
        ))
        .unwrap();
    let error = state.action_availability();
    assert!(error.reconnect && error.close && error.release_all_keys);
    assert!(!error.ctrl_alt_delete && !error.keyboard && !error.pointer);
    assert!(error.fit_to_window && error.one_to_one && error.diagnostics);

    state
        .apply(AppEvent::SessionChanged(snapshot(
            session_id,
            vmid(107),
            SessionPhase::Disconnected,
            true,
        )))
        .unwrap();
    let disconnected = state.action_availability();
    assert!(!disconnected.reconnect && !disconnected.close && !disconnected.release_all_keys);
    assert!(!disconnected.dynamic_resolution);
    assert!(disconnected.fit_to_window && disconnected.one_to_one);
}

#[test]
fn dynamic_resolution_requires_a_ready_native_session_and_valid_usable_viewport() {
    let mut state = configured_state();
    let session_id = SessionId::new();
    state
        .apply(AppEvent::SessionChanged(snapshot(
            session_id,
            vmid(107),
            SessionPhase::Ready,
            false,
        )))
        .unwrap();

    assert!(!state.action_availability().dynamic_resolution);
    state.set_viewport(session_id, 639, 480).unwrap();
    assert!(!state.action_availability().dynamic_resolution);
    state.set_viewport(session_id, 1_600, 900).unwrap();
    assert!(state.action_availability().dynamic_resolution);

    let mut rejected = snapshot(session_id, vmid(107), SessionPhase::Ready, false);
    rejected.resize_status = ResizeStatus::Rejected;
    state.apply(AppEvent::SessionChanged(rejected)).unwrap();
    let availability = state.action_availability();
    assert!(availability.fit_to_window && availability.one_to_one);
    assert!(availability.dynamic_resolution && availability.retry_dynamic_resolution);
}

#[test]
fn inventory_favorites_search_staleness_and_tab_identity_are_deterministic() {
    let mut first = AppState::from_config(&config());
    let mut second = AppState::from_config(&config());
    for state in [&mut first, &mut second] {
        state
            .apply(AppEvent::CachedInventory(inventory(1_000, true)))
            .unwrap();
    }
    assert_eq!(
        first, second,
        "the same event stream must produce the same state"
    );

    let rows = first.inventory_rows();
    assert_eq!(
        rows.iter().map(|row| row.vmid.get()).collect::<Vec<_>>(),
        vec![107, 205, 300]
    );
    assert!(rows[0].favorite && rows[1].favorite && !rows[2].favorite);
    assert_eq!(rows[0].alias.as_deref(), Some("Primary Lab"));
    assert!(rows.iter().all(|row| row.stale));
    assert!(rows.iter().all(|row| row.observed_at_unix_ms == 1_000));

    first.set_search("domain");
    assert_eq!(first.inventory_rows()[0].vmid, vmid(205));
    first.set_search("labz1-cm");
    assert_eq!(first.inventory_rows()[0].vmid, vmid(107));
    first.set_search("300");
    assert_eq!(first.inventory_rows()[0].vmid, vmid(300));

    first.set_search("");
    first.select_inventory(Some(vmid(205)));
    assert!(!first.action_availability().open, "stopped VMs cannot Open");
    assert_eq!(first.inventory_age_source(), Some((1_000, true)));
    first
        .apply(AppEvent::Error(PublicError::new(
            PublicErrorKind::Inventory,
        )))
        .unwrap();
    assert_eq!(
        first.inventory_age_source(),
        Some((1_000, true)),
        "stale evidence remains visible while live inventory is unavailable"
    );
    first
        .apply(AppEvent::LiveInventory(inventory(2_000, false)))
        .unwrap();
    assert_eq!(first.inventory_age_source(), Some((2_000, false)));

    let session_a = SessionId::new();
    let session_b = SessionId::new();
    first
        .apply(AppEvent::SessionChanged(snapshot(
            session_a,
            vmid(107),
            SessionPhase::Ready,
            false,
        )))
        .unwrap();
    first
        .apply(AppEvent::SessionChanged(snapshot(
            session_b,
            vmid(300),
            SessionPhase::Ready,
            false,
        )))
        .unwrap();
    assert_eq!(first.tabs().len(), 2);
    assert_eq!(first.selected_session_id(), Some(session_b));
    first
        .apply(AppEvent::FocusExisting {
            session_id: session_a,
        })
        .unwrap();
    assert_eq!(first.tabs().len(), 2);
    assert_eq!(first.selected_session_id(), Some(session_a));
    assert!(first
        .apply(AppEvent::SessionChanged(snapshot(
            SessionId::new(),
            vmid(400),
            SessionPhase::Ready,
            false,
        )))
        .is_err());
    assert_eq!(
        first.tabs().len(),
        2,
        "render state retains the two-session cap"
    );

    first
        .apply(AppEvent::SessionChanged(snapshot(
            session_b,
            vmid(300),
            SessionPhase::Disconnected,
            false,
        )))
        .unwrap();
    assert_eq!(
        first.selected_session_id(),
        Some(session_a),
        "background lifecycle events must not steal tab focus"
    );
    let session_c = SessionId::new();
    first
        .apply(AppEvent::SessionChanged(snapshot(
            session_c,
            vmid(300),
            SessionPhase::Opening,
            false,
        )))
        .unwrap();
    assert_eq!(first.tabs().len(), 2, "terminal reconnect tabs are pruned");
    assert!(first
        .tabs()
        .iter()
        .all(|tab| tab.snapshot.session_id != session_b));
}

#[test]
fn framebuffer_updates_are_checked_bounded_and_transactional() {
    let mut state = configured_state();
    let session_id = SessionId::new();
    let mut ready = snapshot(session_id, vmid(107), SessionPhase::Ready, false);
    ready.guest_size = Some(DesktopSize::new(4, 3));
    state.apply(AppEvent::SessionChanged(ready)).unwrap();

    state
        .apply(AppEvent::Framebuffer {
            session_id,
            rects: vec![FbRect {
                x: 1,
                y: 1,
                w: 2,
                h: 1,
                rgba: vec![1, 2, 3, 255, 4, 5, 6, 255],
            }],
        })
        .unwrap();
    let before = state.framebuffer(session_id).unwrap().rgba().to_vec();
    assert_eq!(before.len(), 4 * 3 * 4);
    assert_eq!(&before[20..28], &[1, 2, 3, 255, 4, 5, 6, 255]);

    let result = state.apply(AppEvent::Framebuffer {
        session_id,
        rects: vec![
            FbRect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
                rgba: vec![9, 9, 9, 255],
            },
            FbRect {
                x: 3,
                y: 2,
                w: 2,
                h: 1,
                rgba: vec![7; 8],
            },
        ],
    });
    assert!(result.is_err());
    assert_eq!(state.framebuffer(session_id).unwrap().rgba(), before);
}

#[test]
fn framebuffer_upload_plans_are_bounded_coalesced_and_acknowledged_transactionally() {
    let mut state = configured_state();
    let session_id = SessionId::new();
    let mut ready = snapshot(session_id, vmid(107), SessionPhase::Ready, false);
    ready.guest_size = Some(DesktopSize::new(4, 3));
    state.apply(AppEvent::SessionChanged(ready)).unwrap();

    let initial = state
        .framebuffer_upload_plan(session_id, false)
        .unwrap()
        .expect("creation marks the whole image dirty");
    assert_eq!(initial.kind(), FramebufferUploadKind::Full);
    assert_eq!(
        (initial.x(), initial.y(), initial.width(), initial.height()),
        (0, 0, 4, 3)
    );
    assert_eq!(initial.rgba().len(), 4 * 3 * 4);
    state
        .acknowledge_framebuffer_upload(session_id, initial.revision())
        .unwrap();
    assert!(state
        .framebuffer_upload_plan(session_id, false)
        .unwrap()
        .is_none());

    state
        .apply(AppEvent::Framebuffer {
            session_id,
            rects: vec![
                FbRect {
                    x: 1,
                    y: 0,
                    w: 1,
                    h: 1,
                    rgba: vec![1, 2, 3, 4],
                },
                FbRect {
                    x: 2,
                    y: 2,
                    w: 1,
                    h: 1,
                    rgba: vec![5, 6, 7, 8],
                },
            ],
        })
        .unwrap();
    let coalesced = state
        .framebuffer_upload_plan(session_id, false)
        .unwrap()
        .unwrap();
    assert_eq!(coalesced.kind(), FramebufferUploadKind::Partial);
    assert_eq!(
        (
            coalesced.x(),
            coalesced.y(),
            coalesced.width(),
            coalesced.height(),
        ),
        (1, 0, 2, 3)
    );
    assert_eq!(
        coalesced.rgba(),
        [1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 6, 7, 8,],
        "partial extraction must preserve source stride and exact clipping"
    );

    state
        .apply(AppEvent::Framebuffer {
            session_id,
            rects: vec![FbRect {
                x: 0,
                y: 1,
                w: 1,
                h: 1,
                rgba: vec![9, 10, 11, 12],
            }],
        })
        .unwrap();
    state
        .acknowledge_framebuffer_upload(session_id, coalesced.revision())
        .unwrap();
    let retained = state
        .framebuffer_upload_plan(session_id, false)
        .unwrap()
        .expect("acknowledging an old plan must retain a later update");
    assert!(retained.revision() > coalesced.revision());
    assert_eq!(retained.kind(), FramebufferUploadKind::Partial);
}

#[test]
fn maximum_geometry_one_pixel_dirty_stages_only_four_bytes() {
    let mut state = configured_state();
    let session_id = SessionId::new();
    let mut ready = snapshot(session_id, vmid(107), SessionPhase::Ready, false);
    ready.guest_size = Some(DesktopSize::new(8_192, 4_096));
    state.apply(AppEvent::SessionChanged(ready)).unwrap();
    let initial_revision = state.framebuffer(session_id).unwrap().revision();
    state
        .acknowledge_framebuffer_upload(session_id, initial_revision)
        .unwrap();

    state
        .apply(AppEvent::Framebuffer {
            session_id,
            rects: vec![FbRect {
                x: 8_191,
                y: 4_095,
                w: 1,
                h: 1,
                rgba: vec![20, 21, 22, 23],
            }],
        })
        .unwrap();
    let upload = state
        .framebuffer_upload_plan(session_id, false)
        .unwrap()
        .unwrap();
    assert_eq!(upload.kind(), FramebufferUploadKind::Partial);
    assert_eq!(
        (upload.x(), upload.y(), upload.width(), upload.height()),
        (8_191, 4_095, 1, 1)
    );
    assert_eq!(upload.rgba(), [20, 21, 22, 23]);
    assert_eq!(upload.rgba().len(), 4);
}

#[test]
fn resize_and_two_sessions_keep_full_and_partial_upload_state_independent() {
    let mut state = configured_state();
    let first = SessionId::new();
    let second = SessionId::new();
    for (session_id, vmid) in [(first, vmid(107)), (second, vmid(300))] {
        let mut ready = snapshot(session_id, vmid, SessionPhase::Ready, false);
        ready.guest_size = Some(DesktopSize::new(2, 2));
        state.apply(AppEvent::SessionChanged(ready)).unwrap();
        let revision = state.framebuffer(session_id).unwrap().revision();
        state
            .acknowledge_framebuffer_upload(session_id, revision)
            .unwrap();
    }
    state
        .apply(AppEvent::Framebuffer {
            session_id: first,
            rects: vec![FbRect {
                x: 1,
                y: 1,
                w: 1,
                h: 1,
                rgba: vec![1, 2, 3, 4],
            }],
        })
        .unwrap();
    assert_eq!(
        state
            .framebuffer_upload_plan(first, false)
            .unwrap()
            .unwrap()
            .kind(),
        FramebufferUploadKind::Partial
    );
    assert!(state
        .framebuffer_upload_plan(second, false)
        .unwrap()
        .is_none());

    let mut resized = snapshot(second, vmid(300), SessionPhase::Ready, false);
    resized.guest_size = Some(DesktopSize::new(3, 2));
    state.apply(AppEvent::SessionChanged(resized)).unwrap();
    let full = state
        .framebuffer_upload_plan(second, false)
        .unwrap()
        .unwrap();
    assert_eq!(full.kind(), FramebufferUploadKind::Full);
    assert_eq!((full.width(), full.height()), (3, 2));
    assert_eq!(full.rgba().len(), 3 * 2 * 4);
}

#[test]
fn clipboard_payload_bypasses_state_and_diagnostics_are_strictly_redacted() {
    let mut state = configured_state();
    let session_id = SessionId::new();
    state
        .apply(AppEvent::SessionChanged(snapshot(
            session_id,
            vmid(107),
            SessionPhase::Ready,
            false,
        )))
        .unwrap();
    let effects = state
        .apply(AppEvent::ClipboardReceived {
            session_id,
            text: ClipboardText::try_from("clipboard-secret-value".to_owned()).unwrap(),
        })
        .unwrap();
    assert_eq!(effects.len(), 1);

    let debug = format!("{state:?}");
    let diagnostics = state.diagnostics_summary();
    for forbidden in [
        "password",
        "ticket",
        "ssh_target",
        "root@do-not-render.example.invalid",
        "clipboard-secret-value",
        "environment",
        "private key",
        "raw stderr",
        "host fingerprint",
        "guest pixels",
    ] {
        assert!(!debug.to_ascii_lowercase().contains(forbidden));
        assert!(!diagnostics.to_ascii_lowercase().contains(forbidden));
    }
    assert!(diagnostics.contains("Lab profile"));
    assert!(diagnostics.contains("pve2"));
    assert!(diagnostics.contains("107"));

    let mut clipboard = RecordingClipboard::default();
    apply_ui_effects(&mut state, &mut clipboard, effects);
    assert_eq!(
        clipboard.writes.borrow().as_slice(),
        ["clipboard-secret-value"]
    );
    assert_eq!(
        state.selected_session().unwrap().clipboard_status,
        ClipboardStatus::Received
    );
}

#[derive(Default)]
struct RecordingSink {
    sends: Cell<usize>,
}

impl AppCommandSink for RecordingSink {
    fn try_send(&self, _command: AppCommand) -> Result<(), CommandQueueError> {
        self.sends.set(self.sends.get() + 1);
        Ok(())
    }
}

struct DelayedFakeSink {
    sender: tokio::sync::mpsc::Sender<AppCommand>,
    _receiver: tokio::sync::mpsc::Receiver<AppCommand>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetedInput {
    ReleasePointer(SessionId, u16, u16),
    FocusLost(SessionId),
    Key(SessionId, bool, u32),
}

#[derive(Default)]
struct TargetedInputSink {
    inputs: RefCell<Vec<TargetedInput>>,
}

impl AppCommandSink for TargetedInputSink {
    fn try_send(&self, command: AppCommand) -> Result<(), CommandQueueError> {
        if let AppCommand::SendInput { session_id, action } = command {
            let recorded = match action {
                rustedoutclient::session::InputAction::ReleasePointer { x, y } => {
                    Some(TargetedInput::ReleasePointer(session_id, x, y))
                }
                rustedoutclient::session::InputAction::FocusLost => {
                    Some(TargetedInput::FocusLost(session_id))
                }
                rustedoutclient::session::InputAction::Key { down, keysym } => {
                    Some(TargetedInput::Key(session_id, down, keysym))
                }
                _ => None,
            };
            if let Some(recorded) = recorded {
                self.inputs.borrow_mut().push(recorded);
            }
        }
        Ok(())
    }
}

impl DelayedFakeSink {
    fn with_capacity(capacity: usize) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        Self {
            sender,
            _receiver: receiver,
        }
    }
}

impl AppCommandSink for DelayedFakeSink {
    fn try_send(&self, command: AppCommand) -> Result<(), CommandQueueError> {
        self.sender.try_send(command).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => CommandQueueError::Full,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => CommandQueueError::Disconnected,
        })
    }
}

struct RecoveringViewportSink {
    sender: tokio::sync::mpsc::Sender<AppCommand>,
    receiver: RefCell<tokio::sync::mpsc::Receiver<AppCommand>>,
}

impl RecoveringViewportSink {
    fn new() -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        Self {
            sender,
            receiver: RefCell::new(receiver),
        }
    }

    fn fill(&self) {
        self.sender
            .try_send(AppCommand::RefreshInventory)
            .expect("test queue should accept its filler");
    }

    fn discard_one(&self) {
        self.receiver
            .borrow_mut()
            .try_recv()
            .expect("test queue should contain one command");
    }

    fn take_viewport(&self) -> (SessionId, u32, u32) {
        match self
            .receiver
            .borrow_mut()
            .try_recv()
            .expect("viewport command should be queued")
        {
            AppCommand::ViewportChanged {
                session_id,
                backing_width,
                backing_height,
            } => (session_id, backing_width, backing_height),
            _ => panic!("expected only a viewport command"),
        }
    }
}

impl AppCommandSink for RecoveringViewportSink {
    fn try_send(&self, command: AppCommand) -> Result<(), CommandQueueError> {
        self.sender.try_send(command).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => CommandQueueError::Full,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => CommandQueueError::Disconnected,
        })
    }
}

#[derive(Default)]
struct RecordingClipboard {
    reads: Cell<usize>,
    next_read: RefCell<Option<String>>,
    writes: RefCell<Vec<String>>,
}

impl ClipboardAdapter for RecordingClipboard {
    fn read_text(&mut self) -> Result<Option<String>, ClipboardAdapterError> {
        self.reads.set(self.reads.get() + 1);
        Ok(self.next_read.borrow_mut().take())
    }

    fn write_text(&mut self, text: String) -> Result<(), ClipboardAdapterError> {
        self.writes.borrow_mut().push(text);
        Ok(())
    }
}

#[test]
fn explicit_clipboard_and_bounded_command_dispatch_never_wait_for_a_worker() {
    let mut state = configured_state();
    let session_id = SessionId::new();
    state
        .apply(AppEvent::SessionChanged(snapshot(
            session_id,
            vmid(107),
            SessionPhase::Ready,
            false,
        )))
        .unwrap();
    state.set_viewport(session_id, 1_600, 900).unwrap();
    let sink = RecordingSink::default();
    let mut clipboard = RecordingClipboard::default();
    clipboard
        .next_read
        .replace(Some("operator selected text".to_owned()));

    assert_eq!(clipboard.reads.get(), 0, "there is no clipboard polling");
    assert_eq!(
        dispatch_action(&mut state, &sink, &mut clipboard, UiAction::SendClipboard,),
        DispatchOutcome::Sent
    );
    assert_eq!(clipboard.reads.get(), 1);
    assert_eq!(sink.sends.get(), 1);

    assert_eq!(
        dispatch_action(&mut state, &sink, &mut clipboard, UiAction::CopyDiagnostics,),
        DispatchOutcome::AppliedLocally
    );
    assert_eq!(clipboard.writes.borrow().len(), 1);
    assert!(clipboard.writes.borrow()[0].contains("RustedOutClient"));

    for index in 0..1_000 {
        let outcome = dispatch_action(
            &mut state,
            &sink,
            &mut clipboard,
            UiAction::ViewportChanged {
                backing_width: 1_600 + (index % 8),
                backing_height: 900,
            },
        );
        assert_eq!(outcome, DispatchOutcome::Sent);
    }
    assert_eq!(sink.sends.get(), 1_001);

    let delayed = DelayedFakeSink::with_capacity(1);
    assert_eq!(
        dispatch_action(
            &mut state,
            &delayed,
            &mut clipboard,
            UiAction::ReleaseAllKeys,
        ),
        DispatchOutcome::Sent
    );
    assert_eq!(
        dispatch_action(
            &mut state,
            &delayed,
            &mut clipboard,
            UiAction::CtrlAltDelete,
        ),
        DispatchOutcome::Busy
    );
    assert!(state.queue_status().is_busy());
    assert_eq!(
        dispatch_action(&mut state, &sink, &mut clipboard, UiAction::OpenInTigerVnc,),
        DispatchOutcome::NotAvailable
    );
    assert_eq!(sink.sends.get(), 1_001);
}

#[test]
fn targeted_cleanup_never_follows_the_newly_selected_session() {
    let mut state = configured_state();
    let outgoing = SessionId::new();
    let incoming = SessionId::new();
    state
        .apply(AppEvent::SessionChanged(snapshot(
            outgoing,
            vmid(107),
            SessionPhase::Ready,
            false,
        )))
        .unwrap();
    state
        .apply(AppEvent::SessionChanged(snapshot(
            incoming,
            vmid(300),
            SessionPhase::Ready,
            false,
        )))
        .unwrap();
    state
        .apply(AppEvent::SessionChanged(snapshot(
            outgoing,
            vmid(107),
            SessionPhase::Disconnected,
            true,
        )))
        .unwrap();
    assert_eq!(state.selected_session_id(), Some(incoming));

    let sink = TargetedInputSink::default();
    let mut clipboard = RecordingClipboard::default();
    assert_eq!(
        dispatch_action(
            &mut state,
            &sink,
            &mut clipboard,
            UiAction::ReleasePointer {
                session_id: outgoing,
                x: 120,
                y: 220,
            },
        ),
        DispatchOutcome::Sent
    );
    assert_eq!(
        dispatch_action(
            &mut state,
            &sink,
            &mut clipboard,
            UiAction::FocusLost {
                session_id: outgoing,
            },
        ),
        DispatchOutcome::Sent
    );
    assert_eq!(
        dispatch_action(
            &mut state,
            &sink,
            &mut clipboard,
            UiAction::Key {
                session_id: outgoing,
                down: true,
                keysym: 0x41,
            },
        ),
        DispatchOutcome::NotAvailable,
        "fresh input remains blocked for the terminal/view-only owner"
    );
    assert_eq!(
        dispatch_action(
            &mut state,
            &sink,
            &mut clipboard,
            UiAction::Key {
                session_id: incoming,
                down: true,
                keysym: 0x42,
            },
        ),
        DispatchOutcome::Sent
    );
    assert_eq!(
        sink.inputs.borrow().as_slice(),
        [
            TargetedInput::ReleasePointer(outgoing, 120, 220),
            TargetedInput::FocusLost(outgoing),
            TargetedInput::Key(incoming, true, 0x42),
        ]
    );
}

#[test]
fn viewport_acknowledgement_waits_for_queue_acceptance_and_retries_only_the_newest_value() {
    let mut state = configured_state();
    let session_id = SessionId::new();
    state
        .apply(AppEvent::SessionChanged(snapshot(
            session_id,
            vmid(107),
            SessionPhase::Ready,
            false,
        )))
        .unwrap();
    let sink = RecoveringViewportSink::new();
    let mut clipboard = RecordingClipboard::default();
    sink.fill();

    for index in 0..1_000_u32 {
        assert_eq!(
            dispatch_action(
                &mut state,
                &sink,
                &mut clipboard,
                UiAction::ViewportChanged {
                    backing_width: 1_600 + index,
                    backing_height: 900 + index,
                },
            ),
            DispatchOutcome::Busy
        );
    }
    assert_eq!(
        state.selected_session().unwrap().viewport,
        None,
        "a Full queue must leave the acknowledged viewport unchanged"
    );
    assert!(state.queue_status().is_busy());

    sink.discard_one();
    assert_eq!(
        dispatch_action(
            &mut state,
            &sink,
            &mut clipboard,
            UiAction::ViewportChanged {
                backing_width: 2_599,
                backing_height: 1_899,
            },
        ),
        DispatchOutcome::Sent
    );
    assert_eq!(
        sink.take_viewport(),
        (session_id, 2_599, 1_899),
        "only the latest measured viewport is retried"
    );
    assert_eq!(
        state.selected_session().unwrap().viewport,
        Some(rustedoutclient::app::BackingViewport {
            width: 2_599,
            height: 1_899,
        })
    );
    assert_eq!(
        state.queue_status(),
        rustedoutclient::app::QueueStatus::Ready
    );
}

#[test]
fn open_options_keep_clipboard_and_view_only_off_but_dynamic_resolution_on_by_default() {
    let options = OpenOptions::default();
    assert!(!options.view_only);
    assert!(!options.clipboard_enabled);
    assert!(options.dynamic_resolution);
}
