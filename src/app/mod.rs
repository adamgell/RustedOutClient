pub mod actions;
pub mod state;
mod view;

use std::{path::Path, time::Duration};

use eframe::egui;
use tokio::sync::mpsc;

use crate::{
    config::{default_config_path, load_config_from_path, AppConfig},
    session::{AppCommand, PublicError, PublicErrorKind, SessionManager, SessionPhase},
};

pub use actions::{
    apply_ui_effects, dispatch_action, ActionAvailability, AppCommandSink, ClipboardAdapter,
    ClipboardAdapterError, CommandQueueError, DispatchOutcome, SystemClipboard, UiAction,
};
pub use state::{
    AppState, AppStateError, BackingViewport, ClipboardStatus, FramebufferImage, FramebufferUpload,
    FramebufferUploadKind, InventoryRow, QueueStatus, SessionTabState, SetupState, UiEffect,
};

struct DisconnectedSink;

impl AppCommandSink for DisconnectedSink {
    fn try_send(&self, _command: AppCommand) -> Result<(), CommandQueueError> {
        Err(CommandQueueError::Disconnected)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeCloseAction {
    CancelClose,
    Close,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ShutdownEnqueueState {
    #[default]
    NotRequested,
    Pending,
    Enqueued,
    SenderDisconnected,
}

#[derive(Default)]
struct CloseCoordinator {
    close_started: bool,
    manager_completed: bool,
    final_close_issued: bool,
    shutdown: ShutdownEnqueueState,
}

impl CloseCoordinator {
    fn begin(&mut self, close_requested: bool, manager_present: bool) {
        if close_requested && !self.close_started && manager_present {
            self.close_started = true;
            self.shutdown = ShutdownEnqueueState::Pending;
        }
    }

    fn update<S>(
        &mut self,
        close_requested: bool,
        owner_cleanup_pending: bool,
        manager: Option<&S>,
    ) -> Vec<NativeCloseAction>
    where
        S: AppCommandSink + ?Sized,
    {
        self.begin(close_requested, manager.is_some());

        if self.close_started
            && !self.manager_completed
            && !owner_cleanup_pending
            && self.shutdown == ShutdownEnqueueState::Pending
        {
            if let Some(manager) = manager {
                self.shutdown = match manager.try_send(AppCommand::Shutdown) {
                    Ok(()) => ShutdownEnqueueState::Enqueued,
                    Err(CommandQueueError::Full) => ShutdownEnqueueState::Pending,
                    Err(CommandQueueError::Disconnected) => {
                        ShutdownEnqueueState::SenderDisconnected
                    }
                };
            }
        }

        if self.close_started && self.manager_completed && !self.final_close_issued {
            self.final_close_issued = true;
            return vec![NativeCloseAction::Close];
        }
        if close_requested && self.close_started {
            return vec![NativeCloseAction::CancelClose];
        }
        Vec::new()
    }

    fn reconcile_manager<T>(&mut self, event_channel_disconnected: bool, manager: &mut Option<T>) {
        if event_channel_disconnected {
            let _ = manager.take();
            self.manager_completed = true;
        }
    }
}

pub struct RustedOutClient {
    state: AppState,
    manager: Option<SessionManager>,
    clipboard: Box<dyn ClipboardAdapter>,
    view: view::ViewResources,
    close: CloseCoordinator,
}

impl RustedOutClient {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        view::configure_visuals(&cc.egui_ctx);
        let (state, manager) = load_application();
        Self {
            state,
            manager,
            clipboard: Box::<SystemClipboard>::default(),
            view: view::ViewResources::default(),
            close: CloseCoordinator::default(),
        }
    }

    fn drain_events(&mut self) -> bool {
        let Some(manager) = self.manager.as_mut() else {
            return false;
        };
        let mut disconnected = false;
        for _ in 0..crate::session::APP_QUEUE_CAPACITY {
            match manager.try_recv() {
                Ok(event) => {
                    match self.state.apply(event) {
                        Ok(effects) => {
                            apply_ui_effects(&mut self.state, &mut *self.clipboard, effects);
                        }
                        Err(_) => {
                            let _ = self.state.apply(crate::session::AppEvent::Error(
                                PublicError::new(PublicErrorKind::RfbLimit),
                            ));
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        disconnected
    }

    fn has_active_session(&self) -> bool {
        self.state.tabs().iter().any(|tab| {
            !matches!(
                tab.snapshot.phase,
                SessionPhase::Disconnecting | SessionPhase::Disconnected
            )
        })
    }
}

fn dispatch_rendered_actions<S, C>(
    state: &mut AppState,
    sink: &S,
    clipboard: &mut C,
    view: &mut view::ViewResources,
    actions: impl IntoIterator<Item = UiAction>,
) -> bool
where
    S: AppCommandSink + ?Sized,
    C: ClipboardAdapter + ?Sized,
{
    let initial_fullscreen = state.fullscreen();
    for action in actions {
        let owner_cleanup = matches!(action, UiAction::ReleaseOwnedInput { .. });
        let pointer = match action {
            UiAction::Pointer {
                session_id,
                buttons,
                x,
                y,
            } => Some((session_id, buttons, (x, y))),
            _ => None,
        };
        if let Some((session_id, buttons, position)) = pointer {
            view.begin_pointer_dispatch(session_id, buttons, position);
        }
        let outcome = dispatch_action(state, sink, clipboard, action);
        if let Some((session_id, buttons, _)) = pointer {
            view.acknowledge_pointer_dispatch(session_id, buttons, outcome);
            if view.owner_cleanup_pending() {
                break;
            }
        }
        if owner_cleanup {
            view.acknowledge_owner_cleanup(outcome);
            if outcome != DispatchOutcome::Sent {
                break;
            }
        }
    }
    state.fullscreen() != initial_fullscreen
}

impl eframe::App for RustedOutClient {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let close_requested = ctx.input(|input| input.viewport().close_requested());
        let manager_present = self.manager.is_some();
        self.close.begin(close_requested, manager_present);
        if close_requested && manager_present {
            self.view.request_owner_cleanup();
        }
        let manager_completed = self.drain_events();
        if manager_completed {
            self.view.manager_completed();
        }
        self.close
            .reconcile_manager(manager_completed, &mut self.manager);
        let actions = view::render(ctx, &mut self.state, &mut self.view);
        let fullscreen_changed = match self.manager.as_ref() {
            Some(manager) => dispatch_rendered_actions(
                &mut self.state,
                manager,
                &mut *self.clipboard,
                &mut self.view,
                actions,
            ),
            None => dispatch_rendered_actions(
                &mut self.state,
                &DisconnectedSink,
                &mut *self.clipboard,
                &mut self.view,
                actions,
            ),
        };
        if fullscreen_changed {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.state.fullscreen()));
        }

        for action in self.close.update(
            close_requested,
            self.view.owner_cleanup_pending(),
            self.manager.as_ref(),
        ) {
            match action {
                NativeCloseAction::CancelClose => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
                NativeCloseAction::Close => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        if self.has_active_session() {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else if self.manager.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

pub type RustedOutClientApp = RustedOutClient;

fn load_application() -> (AppState, Option<SessionManager>) {
    let path = match default_config_path() {
        Ok(path) => path,
        Err(_) => return (AppState::invalid_configuration(), None),
    };
    if !Path::new(&path).exists() {
        return (AppState::missing_configuration(), None);
    }
    let config = match load_config_from_path(&path) {
        Ok(config) => config,
        Err(_) => return (AppState::invalid_configuration(), None),
    };
    start_configured_application(config, &path)
}

fn start_configured_application(
    config: AppConfig,
    config_path: &Path,
) -> (AppState, Option<SessionManager>) {
    let mut state = AppState::from_config(&config);
    let Some(directory) = config_path.parent() else {
        return (AppState::invalid_configuration(), None);
    };
    let cache_path = directory.join("inventory-v1.json");
    match SessionManager::spawn_production(config, cache_path) {
        Ok(manager) => (state, Some(manager)),
        Err(error) => {
            let _ = state.apply(crate::session::AppEvent::Error(error));
            (state, None)
        }
    }
}

#[cfg(test)]
mod close_coordinator_tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        rc::Rc,
    };

    use super::view::ViewResources;
    use super::{
        dispatch_rendered_actions, AppCommand, AppCommandSink, AppState, ClipboardAdapter,
        ClipboardAdapterError, CloseCoordinator, CommandQueueError, NativeCloseAction, UiAction,
    };
    use crate::{
        config::AppConfig,
        model::{NodeName, PveProfile, SshTarget, VmId},
        session::{AppEvent, InputAction, ResizeStatus, SessionId, SessionPhase, SessionSnapshot},
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RecordedCommand {
        Pointer(SessionId, u8, u16, u16),
        ReleaseOwnedInput(SessionId, Option<(u16, u16)>),
        Fallback(VmId, bool, bool),
        Shutdown,
        Other,
    }

    struct ScriptedSink {
        outcomes: RefCell<VecDeque<Result<(), CommandQueueError>>>,
        shutdown_attempts: Cell<usize>,
        accepted: RefCell<Vec<RecordedCommand>>,
    }

    impl ScriptedSink {
        fn new(outcomes: impl IntoIterator<Item = Result<(), CommandQueueError>>) -> Self {
            Self {
                outcomes: RefCell::new(outcomes.into_iter().collect()),
                shutdown_attempts: Cell::new(0),
                accepted: RefCell::new(Vec::new()),
            }
        }

        fn accepted(&self) -> Vec<RecordedCommand> {
            self.accepted.borrow().clone()
        }
    }

    impl AppCommandSink for ScriptedSink {
        fn try_send(&self, command: AppCommand) -> Result<(), CommandQueueError> {
            let recorded = match command {
                AppCommand::SendInput {
                    session_id,
                    action: InputAction::Pointer { buttons, x, y },
                } => RecordedCommand::Pointer(session_id, buttons, x, y),
                AppCommand::SendInput {
                    session_id,
                    action: InputAction::ReleaseOwnedInput { pointer_position },
                } => RecordedCommand::ReleaseOwnedInput(session_id, pointer_position),
                AppCommand::OpenInTigerVnc { vmid, preferences } => {
                    RecordedCommand::Fallback(vmid, preferences.fullscreen, preferences.view_only)
                }
                AppCommand::Shutdown => {
                    self.shutdown_attempts
                        .set(self.shutdown_attempts.get().saturating_add(1));
                    RecordedCommand::Shutdown
                }
                _ => RecordedCommand::Other,
            };
            let outcome = self.outcomes.borrow_mut().pop_front().unwrap_or(Ok(()));
            if outcome.is_ok() {
                self.accepted.borrow_mut().push(recorded);
            }
            outcome
        }
    }

    fn ready_state(first: SessionId, second: SessionId) -> AppState {
        let mut config = AppConfig::new(PveProfile {
            name: "Synthetic lab".to_owned(),
            ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
            node: NodeName::parse("pve2").unwrap(),
        });
        config.fallback_viewer = Some(std::path::PathBuf::from("/synthetic/viewer"));
        let mut state = AppState::from_config(&config);
        for (session_id, vmid) in [(first, 107), (second, 108)] {
            state
                .apply(AppEvent::SessionChanged(SessionSnapshot {
                    session_id,
                    profile_name: "Synthetic lab".to_owned(),
                    vmid: VmId::new(vmid).unwrap(),
                    phase: SessionPhase::Ready,
                    view_only: false,
                    clipboard_enabled: false,
                    dynamic_resolution_enabled: true,
                    guest_size: None,
                    resize_status: ResizeStatus::Waiting,
                }))
                .unwrap();
        }
        state.select_session(Some(first));
        state
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

    #[test]
    fn full_zero_button_release_retries_one_old_owner_cleanup_before_later_control() {
        let outgoing = SessionId::new();
        let incoming = SessionId::new();
        let sink = ScriptedSink::new([Ok(()), Err(CommandQueueError::Full), Ok(()), Ok(())]);
        let mut state = ready_state(outgoing, incoming);
        let mut clipboard = NoClipboard;
        let mut view = ViewResources::default();
        view.set_test_owner(outgoing, 0, 0, None);

        dispatch_rendered_actions(
            &mut state,
            &sink,
            &mut clipboard,
            &mut view,
            [
                UiAction::Pointer {
                    session_id: outgoing,
                    buttons: 1,
                    x: 10,
                    y: 20,
                },
                UiAction::Pointer {
                    session_id: outgoing,
                    buttons: 0,
                    x: 12,
                    y: 22,
                },
                UiAction::RefreshInventory,
            ],
        );

        assert_eq!(
            sink.accepted(),
            [RecordedCommand::Pointer(outgoing, 1, 10, 20)],
            "a Full zero-button release must block later controls"
        );
        assert!(view.owner_cleanup_pending());
        assert_eq!(
            view.owner_cleanup_action(),
            Some(UiAction::ReleaseOwnedInput {
                session_id: outgoing,
                pointer_position: Some((12, 22)),
            })
        );

        state.select_session(Some(incoming));
        let cleanup = view.owner_cleanup_action().unwrap();
        dispatch_rendered_actions(
            &mut state,
            &sink,
            &mut clipboard,
            &mut view,
            [cleanup, UiAction::RefreshInventory],
        );
        assert!(!view.owner_cleanup_pending());
        assert_eq!(
            sink.accepted(),
            [
                RecordedCommand::Pointer(outgoing, 1, 10, 20),
                RecordedCommand::ReleaseOwnedInput(outgoing, Some((12, 22))),
                RecordedCommand::Other,
            ],
            "selection changes must not retarget the retained release"
        );
    }

    #[test]
    fn fallback_waits_behind_retained_input_owner_cleanup_on_the_same_queue() {
        let outgoing = SessionId::new();
        let incoming = SessionId::new();
        let sink = ScriptedSink::new([Err(CommandQueueError::Full), Ok(()), Ok(())]);
        let mut state = ready_state(outgoing, incoming);
        let mut clipboard = NoClipboard;
        let mut view = ViewResources::default();
        view.set_test_owner(outgoing, 0b0010, 0b10, Some((40, 50)));
        view.request_owner_cleanup();

        let cleanup = view.owner_cleanup_action().unwrap();
        dispatch_rendered_actions(
            &mut state,
            &sink,
            &mut clipboard,
            &mut view,
            [cleanup, UiAction::OpenInTigerVnc],
        );
        assert!(sink.accepted().is_empty());
        assert!(view.owner_cleanup_pending());

        let cleanup = view.owner_cleanup_action().unwrap();
        dispatch_rendered_actions(
            &mut state,
            &sink,
            &mut clipboard,
            &mut view,
            [cleanup, UiAction::OpenInTigerVnc],
        );
        assert_eq!(
            sink.accepted(),
            [
                RecordedCommand::ReleaseOwnedInput(outgoing, Some((40, 50))),
                RecordedCommand::Fallback(VmId::new(107).unwrap(), false, false),
            ]
        );
    }

    #[test]
    fn ordinary_zero_button_acknowledgement_clears_only_sent_and_disconnects_without_spin() {
        let outgoing = SessionId::new();
        let incoming = SessionId::new();

        let sent_sink = ScriptedSink::new([Ok(()), Ok(())]);
        let mut sent_state = ready_state(outgoing, incoming);
        let mut clipboard = NoClipboard;
        let mut sent_view = ViewResources::default();
        sent_view.set_test_owner(outgoing, 0, 0, None);
        dispatch_rendered_actions(
            &mut sent_state,
            &sent_sink,
            &mut clipboard,
            &mut sent_view,
            [
                UiAction::Pointer {
                    session_id: outgoing,
                    buttons: 1,
                    x: 30,
                    y: 40,
                },
                UiAction::Pointer {
                    session_id: outgoing,
                    buttons: 0,
                    x: 31,
                    y: 41,
                },
            ],
        );
        assert!(!sent_view.owner_cleanup_pending());
        assert_eq!(sent_view.owner_cleanup_action(), None);
        sent_view.request_owner_cleanup();
        assert_eq!(
            sent_view.owner_cleanup_action(),
            Some(UiAction::ReleaseOwnedInput {
                session_id: outgoing,
                pointer_position: None,
            }),
            "an accepted zero-button release leaves no duplicate pointer recovery"
        );

        let disconnected_sink =
            ScriptedSink::new([Ok(()), Err(CommandQueueError::Disconnected), Ok(())]);
        let mut disconnected_state = ready_state(outgoing, incoming);
        let mut disconnected_view = ViewResources::default();
        disconnected_view.set_test_owner(outgoing, 0, 0, None);
        dispatch_rendered_actions(
            &mut disconnected_state,
            &disconnected_sink,
            &mut clipboard,
            &mut disconnected_view,
            [
                UiAction::Pointer {
                    session_id: outgoing,
                    buttons: 1,
                    x: 50,
                    y: 60,
                },
                UiAction::Pointer {
                    session_id: outgoing,
                    buttons: 0,
                    x: 51,
                    y: 61,
                },
                UiAction::RefreshInventory,
            ],
        );
        assert!(disconnected_view.owner_cleanup_pending());
        assert_eq!(
            disconnected_view.owner_cleanup_action(),
            None,
            "a disconnected cleanup record must not spin another send"
        );
        assert_eq!(
            disconnected_sink.accepted(),
            [RecordedCommand::Pointer(outgoing, 1, 50, 60)],
            "later controls remain blocked after sender disconnection"
        );
        disconnected_view.manager_completed();
        assert!(!disconnected_view.owner_cleanup_pending());
    }

    #[test]
    fn native_close_after_full_mouse_up_orders_retained_release_before_shutdown() {
        let outgoing = SessionId::new();
        let incoming = SessionId::new();
        let sink = ScriptedSink::new([Ok(()), Err(CommandQueueError::Full), Ok(()), Ok(())]);
        let mut state = ready_state(outgoing, incoming);
        let mut clipboard = NoClipboard;
        let mut view = ViewResources::default();
        let mut coordinator = CloseCoordinator::default();
        view.set_test_owner(outgoing, 0b0010, 0, None);

        dispatch_rendered_actions(
            &mut state,
            &sink,
            &mut clipboard,
            &mut view,
            [
                UiAction::Pointer {
                    session_id: outgoing,
                    buttons: 1,
                    x: 70,
                    y: 80,
                },
                UiAction::Pointer {
                    session_id: outgoing,
                    buttons: 0,
                    x: 71,
                    y: 81,
                },
            ],
        );
        assert_eq!(
            coordinator.update(true, view.owner_cleanup_pending(), Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        assert_eq!(sink.shutdown_attempts.get(), 0);

        let cleanup = view.owner_cleanup_action().unwrap();
        dispatch_rendered_actions(&mut state, &sink, &mut clipboard, &mut view, [cleanup]);
        assert_eq!(coordinator.update(false, false, Some(&sink)), Vec::new());
        assert_eq!(
            sink.accepted(),
            [
                RecordedCommand::Pointer(outgoing, 1, 70, 80),
                RecordedCommand::ReleaseOwnedInput(outgoing, Some((71, 81))),
                RecordedCommand::Shutdown,
            ]
        );
    }

    #[test]
    fn cleanup_dispatch_acknowledgement_retries_full_and_blocks_later_control_until_sent() {
        let outgoing = SessionId::new();
        let sink = ScriptedSink::new([Err(CommandQueueError::Full), Ok(()), Ok(())]);
        let mut state = AppState::missing_configuration();
        let mut clipboard = NoClipboard;
        let mut view = ViewResources::default();
        view.set_test_owner(outgoing, 0b1111, 0b1_1111, Some((20, 30)));
        view.request_owner_cleanup();

        let cleanup = view.owner_cleanup_action().unwrap();
        dispatch_rendered_actions(
            &mut state,
            &sink,
            &mut clipboard,
            &mut view,
            [cleanup, UiAction::RefreshInventory],
        );
        assert!(view.owner_cleanup_pending());
        assert!(sink.accepted().is_empty());

        let cleanup = view.owner_cleanup_action().unwrap();
        dispatch_rendered_actions(
            &mut state,
            &sink,
            &mut clipboard,
            &mut view,
            [cleanup, UiAction::RefreshInventory],
        );
        assert!(!view.owner_cleanup_pending());
        assert_eq!(
            sink.accepted(),
            [
                RecordedCommand::ReleaseOwnedInput(outgoing, Some((20, 30))),
                RecordedCommand::Other,
            ]
        );
    }

    #[test]
    fn native_close_waits_for_retryable_owner_cleanup_before_shutdown_fifo_and_final_close() {
        let outgoing = SessionId::new();
        let sink = ScriptedSink::new([
            Err(CommandQueueError::Full),
            Ok(()),
            Err(CommandQueueError::Full),
            Ok(()),
        ]);
        let mut coordinator = CloseCoordinator::default();

        assert_eq!(
            coordinator.update(true, true, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        assert_eq!(sink.shutdown_attempts.get(), 0);
        assert_eq!(
            sink.try_send(AppCommand::SendInput {
                session_id: outgoing,
                action: InputAction::ReleaseOwnedInput {
                    pointer_position: Some((123, 234)),
                },
            }),
            Err(CommandQueueError::Full)
        );
        assert_eq!(
            coordinator.update(true, true, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        assert_eq!(sink.shutdown_attempts.get(), 0);

        sink.try_send(AppCommand::SendInput {
            session_id: outgoing,
            action: InputAction::ReleaseOwnedInput {
                pointer_position: Some((123, 234)),
            },
        })
        .unwrap();
        assert_eq!(coordinator.update(false, false, Some(&sink)), Vec::new());
        assert_eq!(
            sink.accepted(),
            [RecordedCommand::ReleaseOwnedInput(
                outgoing,
                Some((123, 234))
            )],
            "one free slot accepts the unified cleanup without dropping key cleanup"
        );
        assert_eq!(sink.shutdown_attempts.get(), 1);
        assert_eq!(coordinator.update(false, false, Some(&sink)), Vec::new());
        assert_eq!(
            sink.accepted(),
            [
                RecordedCommand::ReleaseOwnedInput(outgoing, Some((123, 234))),
                RecordedCommand::Shutdown,
            ]
        );
        assert_eq!(sink.shutdown_attempts.get(), 2);

        let mut manager = Some(());
        coordinator.reconcile_manager(false, &mut manager);
        assert!(manager.is_some());
        coordinator.reconcile_manager(true, &mut manager);
        assert_eq!(
            coordinator.update(false, false, None::<&ScriptedSink>),
            vec![NativeCloseAction::Close]
        );
        assert_eq!(
            coordinator.update(false, false, None::<&ScriptedSink>),
            Vec::new()
        );
    }

    #[test]
    fn immediate_native_close_is_cancelled_and_enqueues_shutdown_once() {
        let sink = ScriptedSink::new([Ok(())]);
        let mut coordinator = CloseCoordinator::default();

        assert_eq!(
            coordinator.update(true, false, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        assert_eq!(coordinator.update(false, false, Some(&sink)), Vec::new());
        assert_eq!(sink.shutdown_attempts.get(), 1);
    }

    #[test]
    fn full_shutdown_queue_retains_one_intent_and_retries_on_a_later_frame() {
        let sink = ScriptedSink::new([Err(CommandQueueError::Full), Ok(())]);
        let mut coordinator = CloseCoordinator::default();

        assert_eq!(
            coordinator.update(true, false, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        assert_eq!(sink.shutdown_attempts.get(), 1);
        assert_eq!(coordinator.update(false, false, Some(&sink)), Vec::new());
        assert_eq!(sink.shutdown_attempts.get(), 2);
        assert_eq!(coordinator.update(false, false, Some(&sink)), Vec::new());
        assert_eq!(sink.shutdown_attempts.get(), 2);
    }

    #[test]
    fn duplicate_close_requests_are_each_cancelled_without_duplicate_shutdown() {
        let sink = ScriptedSink::new([Ok(())]);
        let mut coordinator = CloseCoordinator::default();

        for _ in 0..2 {
            assert_eq!(
                coordinator.update(true, false, Some(&sink)),
                vec![NativeCloseAction::CancelClose]
            );
        }
        assert_eq!(sink.shutdown_attempts.get(), 1);
    }

    #[test]
    fn final_close_is_issued_once_only_after_event_completion_releases_manager() {
        let sink = ScriptedSink::new([Ok(())]);
        let mut coordinator = CloseCoordinator::default();
        let mut manager = Some(());

        assert_eq!(
            coordinator.update(true, false, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        coordinator.reconcile_manager(false, &mut manager);
        assert!(manager.is_some());
        assert_eq!(coordinator.update(false, false, Some(&sink)), Vec::new());

        coordinator.reconcile_manager(true, &mut manager);
        assert!(manager.is_none());
        assert_eq!(
            coordinator.update(false, false, None::<&ScriptedSink>),
            vec![NativeCloseAction::Close]
        );
        assert_eq!(
            coordinator.update(false, false, None::<&ScriptedSink>),
            Vec::new()
        );
    }

    #[test]
    fn disconnected_command_sender_does_not_release_manager_early() {
        let sink = ScriptedSink::new([Err(CommandQueueError::Disconnected)]);
        let mut coordinator = CloseCoordinator::default();
        let drops = Rc::new(Cell::new(0));
        let mut manager = Some(DropProbe(Rc::clone(&drops)));

        assert_eq!(
            coordinator.update(true, false, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        coordinator.reconcile_manager(false, &mut manager);
        assert!(manager.is_some());
        assert_eq!(drops.get(), 0);
        assert_eq!(
            coordinator.update(true, false, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );

        coordinator.reconcile_manager(true, &mut manager);
        assert!(manager.is_none());
        assert_eq!(drops.get(), 1);
        assert_eq!(
            coordinator.update(false, false, None::<&ScriptedSink>),
            vec![NativeCloseAction::Close]
        );
    }

    struct DropProbe(Rc<Cell<usize>>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.set(self.0.get().saturating_add(1));
        }
    }
}
