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
    fn update<S>(&mut self, close_requested: bool, manager: Option<&S>) -> Vec<NativeCloseAction>
    where
        S: AppCommandSink + ?Sized,
    {
        if close_requested && !self.close_started && manager.is_some() {
            self.close_started = true;
            self.shutdown = ShutdownEnqueueState::Pending;
        }

        if self.close_started
            && !self.manager_completed
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

impl eframe::App for RustedOutClient {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let manager_completed = self.drain_events();
        self.close
            .reconcile_manager(manager_completed, &mut self.manager);
        let actions = view::render(ctx, &mut self.state, &mut self.view);
        for action in actions {
            let was_fullscreen = self.state.fullscreen();
            let outcome = match self.manager.as_ref() {
                Some(manager) => {
                    dispatch_action(&mut self.state, manager, &mut *self.clipboard, action)
                }
                None => dispatch_action(
                    &mut self.state,
                    &DisconnectedSink,
                    &mut *self.clipboard,
                    action,
                ),
            };
            if outcome == DispatchOutcome::AppliedLocally
                && self.state.fullscreen() != was_fullscreen
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.state.fullscreen()));
            }
        }

        let close_requested = ctx.input(|input| input.viewport().close_requested());
        for action in self.close.update(close_requested, self.manager.as_ref()) {
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

    use super::{
        AppCommand, AppCommandSink, CloseCoordinator, CommandQueueError, NativeCloseAction,
    };

    struct ScriptedSink {
        outcomes: RefCell<VecDeque<Result<(), CommandQueueError>>>,
        shutdown_attempts: Cell<usize>,
    }

    impl ScriptedSink {
        fn new(outcomes: impl IntoIterator<Item = Result<(), CommandQueueError>>) -> Self {
            Self {
                outcomes: RefCell::new(outcomes.into_iter().collect()),
                shutdown_attempts: Cell::new(0),
            }
        }
    }

    impl AppCommandSink for ScriptedSink {
        fn try_send(&self, command: AppCommand) -> Result<(), CommandQueueError> {
            assert!(matches!(command, AppCommand::Shutdown));
            self.shutdown_attempts
                .set(self.shutdown_attempts.get().saturating_add(1));
            self.outcomes.borrow_mut().pop_front().unwrap_or(Ok(()))
        }
    }

    #[test]
    fn immediate_native_close_is_cancelled_and_enqueues_shutdown_once() {
        let sink = ScriptedSink::new([Ok(())]);
        let mut coordinator = CloseCoordinator::default();

        assert_eq!(
            coordinator.update(true, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        assert_eq!(coordinator.update(false, Some(&sink)), Vec::new());
        assert_eq!(sink.shutdown_attempts.get(), 1);
    }

    #[test]
    fn full_shutdown_queue_retains_one_intent_and_retries_on_a_later_frame() {
        let sink = ScriptedSink::new([Err(CommandQueueError::Full), Ok(())]);
        let mut coordinator = CloseCoordinator::default();

        assert_eq!(
            coordinator.update(true, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        assert_eq!(sink.shutdown_attempts.get(), 1);
        assert_eq!(coordinator.update(false, Some(&sink)), Vec::new());
        assert_eq!(sink.shutdown_attempts.get(), 2);
        assert_eq!(coordinator.update(false, Some(&sink)), Vec::new());
        assert_eq!(sink.shutdown_attempts.get(), 2);
    }

    #[test]
    fn duplicate_close_requests_are_each_cancelled_without_duplicate_shutdown() {
        let sink = ScriptedSink::new([Ok(())]);
        let mut coordinator = CloseCoordinator::default();

        for _ in 0..2 {
            assert_eq!(
                coordinator.update(true, Some(&sink)),
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
            coordinator.update(true, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        coordinator.reconcile_manager(false, &mut manager);
        assert!(manager.is_some());
        assert_eq!(coordinator.update(false, Some(&sink)), Vec::new());

        coordinator.reconcile_manager(true, &mut manager);
        assert!(manager.is_none());
        assert_eq!(
            coordinator.update(false, None::<&ScriptedSink>),
            vec![NativeCloseAction::Close]
        );
        assert_eq!(coordinator.update(false, None::<&ScriptedSink>), Vec::new());
    }

    #[test]
    fn disconnected_command_sender_does_not_release_manager_early() {
        let sink = ScriptedSink::new([Err(CommandQueueError::Disconnected)]);
        let mut coordinator = CloseCoordinator::default();
        let drops = Rc::new(Cell::new(0));
        let mut manager = Some(DropProbe(Rc::clone(&drops)));

        assert_eq!(
            coordinator.update(true, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );
        coordinator.reconcile_manager(false, &mut manager);
        assert!(manager.is_some());
        assert_eq!(drops.get(), 0);
        assert_eq!(
            coordinator.update(true, Some(&sink)),
            vec![NativeCloseAction::CancelClose]
        );

        coordinator.reconcile_manager(true, &mut manager);
        assert!(manager.is_none());
        assert_eq!(drops.get(), 1);
        assert_eq!(
            coordinator.update(false, None::<&ScriptedSink>),
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
