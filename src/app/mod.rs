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
    AppState, AppStateError, BackingViewport, ClipboardStatus, FramebufferImage, InventoryRow,
    QueueStatus, SessionTabState, SetupState, UiEffect,
};

struct DisconnectedSink;

impl AppCommandSink for DisconnectedSink {
    fn try_send(&self, _command: AppCommand) -> Result<(), CommandQueueError> {
        Err(CommandQueueError::Disconnected)
    }
}

pub struct RustedOutClient {
    state: AppState,
    manager: Option<SessionManager>,
    clipboard: Box<dyn ClipboardAdapter>,
    view: view::ViewResources,
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
        }
    }

    fn drain_events(&mut self) {
        let Some(manager) = self.manager.as_mut() else {
            return;
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
        if disconnected {
            self.manager = None;
        }
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
        self.drain_events();
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

        if ctx.input(|input| input.viewport().close_requested()) {
            if let Some(manager) = &self.manager {
                let _ = manager.try_send(AppCommand::Shutdown);
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
