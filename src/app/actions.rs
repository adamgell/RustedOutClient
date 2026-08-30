use thiserror::Error;

use crate::{
    model::{ScaleMode, VmId},
    session::{AppCommand, InputAction, ResizeStatus, SessionManager, SessionPhase},
};

use super::state::{AppState, ClipboardStatus, QueueStatus, SessionTabState, UiEffect};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ActionAvailability {
    pub open: bool,
    pub reconnect: bool,
    pub close: bool,
    pub open_in_tigervnc: bool,
    pub ctrl_alt_delete: bool,
    pub release_all_keys: bool,
    pub view_only: bool,
    pub dynamic_resolution: bool,
    pub retry_dynamic_resolution: bool,
    pub fit_to_window: bool,
    pub one_to_one: bool,
    pub fullscreen: bool,
    pub send_clipboard: bool,
    pub receive_clipboard: bool,
    pub diagnostics: bool,
    pub keyboard: bool,
    pub pointer: bool,
}

impl ActionAvailability {
    pub(crate) fn from_state(state: &AppState) -> Self {
        let open = state
            .selected_inventory_item()
            .is_some_and(|item| item.status == crate::ssh::VmStatus::Running);
        let Some(tab) = state.selected_session() else {
            return Self {
                open,
                diagnostics: true,
                ..Self::default()
            };
        };
        let live = !matches!(
            tab.snapshot.phase,
            SessionPhase::Disconnecting | SessionPhase::Disconnected
        );
        let ready = tab.snapshot.phase == SessionPhase::Ready && tab.last_error.is_none();
        let writable = ready && !tab.snapshot.view_only;
        let dynamic_resolution = state.selected_resize_is_actionable();
        let retry_dynamic_resolution = dynamic_resolution
            && matches!(
                tab.snapshot.resize_status,
                ResizeStatus::Rejected | ResizeStatus::Unsupported | ResizeStatus::TimedOut
            );
        Self {
            open,
            reconnect: live,
            close: live,
            open_in_tigervnc: false,
            ctrl_alt_delete: writable,
            release_all_keys: live,
            view_only: live,
            dynamic_resolution,
            retry_dynamic_resolution,
            fit_to_window: true,
            one_to_one: true,
            fullscreen: true,
            send_clipboard: writable && tab.snapshot.clipboard_enabled,
            receive_clipboard: writable && tab.snapshot.clipboard_enabled,
            diagnostics: true,
            keyboard: writable,
            pointer: writable,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandQueueError {
    Full,
    Disconnected,
}

pub trait AppCommandSink {
    fn try_send(&self, command: AppCommand) -> Result<(), CommandQueueError>;
}

impl AppCommandSink for SessionManager {
    fn try_send(&self, command: AppCommand) -> Result<(), CommandQueueError> {
        SessionManager::try_send(self, command).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => CommandQueueError::Full,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => CommandQueueError::Disconnected,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("host clipboard operation failed")]
pub struct ClipboardAdapterError;

pub trait ClipboardAdapter {
    fn read_text(&mut self) -> Result<Option<String>, ClipboardAdapterError>;
    fn write_text(&mut self, text: String) -> Result<(), ClipboardAdapterError>;
}

#[derive(Default)]
pub struct SystemClipboard;

impl ClipboardAdapter for SystemClipboard {
    fn read_text(&mut self) -> Result<Option<String>, ClipboardAdapterError> {
        let mut clipboard = arboard::Clipboard::new().map_err(|_| ClipboardAdapterError)?;
        match clipboard.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(_) => Err(ClipboardAdapterError),
        }
    }

    fn write_text(&mut self, text: String) -> Result<(), ClipboardAdapterError> {
        let mut clipboard = arboard::Clipboard::new().map_err(|_| ClipboardAdapterError)?;
        clipboard.set_text(text).map_err(|_| ClipboardAdapterError)
    }
}

pub enum UiAction {
    RefreshInventory,
    Open,
    Reconnect,
    Close,
    OpenInTigerVnc,
    CtrlAltDelete,
    ReleaseAllKeys,
    SetViewOnly(bool),
    SetDynamicResolution(bool),
    RetryDynamicResolution,
    FitToWindow,
    OneToOne,
    Fullscreen,
    SendClipboard,
    ReceiveClipboard,
    Diagnostics,
    CopyDiagnostics,
    ViewportChanged {
        backing_width: u32,
        backing_height: u32,
    },
    Key {
        down: bool,
        keysym: u32,
    },
    Pointer {
        buttons: u8,
        x: u16,
        y: u16,
    },
    FocusLost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    Sent,
    AppliedLocally,
    Busy,
    Disconnected,
    NotAvailable,
    ClipboardUnavailable,
}

pub fn dispatch_action<S, C>(
    state: &mut AppState,
    sink: &S,
    clipboard: &mut C,
    action: UiAction,
) -> DispatchOutcome
where
    S: AppCommandSink + ?Sized,
    C: ClipboardAdapter + ?Sized,
{
    let availability = state.action_availability();
    let selected_session = state.selected_session_id();
    match action {
        UiAction::RefreshInventory => dispatch(state, sink, AppCommand::RefreshInventory),
        UiAction::Open => {
            if !availability.open {
                return DispatchOutcome::NotAvailable;
            }
            let Some(vmid) = state.selected_inventory() else {
                return DispatchOutcome::NotAvailable;
            };
            if let Some(existing) = active_tab_for_vmid(state, vmid) {
                state.select_session(Some(existing));
                return DispatchOutcome::AppliedLocally;
            }
            dispatch(
                state,
                sink,
                AppCommand::Open {
                    vmid,
                    options: state.open_options(vmid),
                },
            )
        }
        UiAction::Reconnect => {
            selected_command(state, sink, availability.reconnect, |session_id| {
                AppCommand::Reconnect { session_id }
            })
        }
        UiAction::Close => selected_command(state, sink, availability.close, |session_id| {
            AppCommand::Close { session_id }
        }),
        UiAction::OpenInTigerVnc => DispatchOutcome::NotAvailable,
        UiAction::CtrlAltDelete => selected_input(
            state,
            sink,
            availability.ctrl_alt_delete,
            InputAction::CtrlAltDelete,
        ),
        UiAction::ReleaseAllKeys => selected_input(
            state,
            sink,
            availability.release_all_keys,
            InputAction::ReleaseAllKeys,
        ),
        UiAction::SetViewOnly(enabled) => selected_input(
            state,
            sink,
            availability.view_only,
            InputAction::SetViewOnly(enabled),
        ),
        UiAction::SetDynamicResolution(enabled) => {
            selected_command(state, sink, availability.dynamic_resolution, |session_id| {
                AppCommand::SetDynamicResolution {
                    session_id,
                    enabled,
                }
            })
        }
        UiAction::RetryDynamicResolution => selected_command(
            state,
            sink,
            availability.retry_dynamic_resolution,
            |session_id| AppCommand::RetryDynamicResolution { session_id },
        ),
        UiAction::FitToWindow => {
            if !availability.fit_to_window {
                return DispatchOutcome::NotAvailable;
            }
            state.set_scale_mode(ScaleMode::Fit);
            DispatchOutcome::AppliedLocally
        }
        UiAction::OneToOne => {
            if !availability.one_to_one {
                return DispatchOutcome::NotAvailable;
            }
            state.set_scale_mode(ScaleMode::OneToOne);
            DispatchOutcome::AppliedLocally
        }
        UiAction::Fullscreen => {
            if !availability.fullscreen {
                return DispatchOutcome::NotAvailable;
            }
            state.toggle_fullscreen();
            DispatchOutcome::AppliedLocally
        }
        UiAction::SendClipboard => {
            if !availability.send_clipboard {
                return DispatchOutcome::NotAvailable;
            }
            let Some(session_id) = selected_session else {
                return DispatchOutcome::NotAvailable;
            };
            let text = match clipboard.read_text() {
                Ok(Some(text)) => text,
                Ok(None) | Err(_) => {
                    state.set_clipboard_status(session_id, ClipboardStatus::Unavailable);
                    return DispatchOutcome::ClipboardUnavailable;
                }
            };
            let outcome = dispatch(
                state,
                sink,
                AppCommand::SendInput {
                    session_id,
                    action: InputAction::SendClipboard(text),
                },
            );
            if outcome == DispatchOutcome::Sent {
                state.set_clipboard_status(session_id, ClipboardStatus::Sent);
            }
            outcome
        }
        UiAction::ReceiveClipboard => selected_input(
            state,
            sink,
            availability.receive_clipboard,
            InputAction::ReceiveClipboard,
        ),
        UiAction::Diagnostics => {
            state.toggle_diagnostics();
            DispatchOutcome::AppliedLocally
        }
        UiAction::CopyDiagnostics => {
            if !availability.diagnostics {
                return DispatchOutcome::NotAvailable;
            }
            if clipboard.write_text(state.diagnostics_summary()).is_ok() {
                DispatchOutcome::AppliedLocally
            } else {
                DispatchOutcome::ClipboardUnavailable
            }
        }
        UiAction::ViewportChanged {
            backing_width,
            backing_height,
        } => {
            let Some(session_id) = selected_session else {
                return DispatchOutcome::NotAvailable;
            };
            if state
                .set_viewport(session_id, backing_width, backing_height)
                .is_err()
            {
                return DispatchOutcome::NotAvailable;
            }
            dispatch(
                state,
                sink,
                AppCommand::ViewportChanged {
                    session_id,
                    backing_width,
                    backing_height,
                },
            )
        }
        UiAction::Key { down, keysym } => selected_input(
            state,
            sink,
            availability.keyboard,
            InputAction::Key { down, keysym },
        ),
        UiAction::Pointer { buttons, x, y } => selected_input(
            state,
            sink,
            availability.pointer,
            InputAction::Pointer { buttons, x, y },
        ),
        UiAction::FocusLost => selected_input(
            state,
            sink,
            availability.release_all_keys,
            InputAction::FocusLost,
        ),
    }
}

fn active_tab_for_vmid(state: &AppState, vmid: VmId) -> Option<crate::session::SessionId> {
    state.tabs().iter().find_map(|tab| {
        (tab.snapshot.vmid == vmid && tab.snapshot.phase != SessionPhase::Disconnected)
            .then_some(tab.snapshot.session_id)
    })
}

fn selected_input<S>(
    state: &mut AppState,
    sink: &S,
    available: bool,
    action: InputAction,
) -> DispatchOutcome
where
    S: AppCommandSink + ?Sized,
{
    selected_command(state, sink, available, |session_id| AppCommand::SendInput {
        session_id,
        action,
    })
}

fn selected_command<S>(
    state: &mut AppState,
    sink: &S,
    available: bool,
    command: impl FnOnce(crate::session::SessionId) -> AppCommand,
) -> DispatchOutcome
where
    S: AppCommandSink + ?Sized,
{
    if !available {
        return DispatchOutcome::NotAvailable;
    }
    let Some(session_id) = state.selected_session_id() else {
        return DispatchOutcome::NotAvailable;
    };
    dispatch(state, sink, command(session_id))
}

fn dispatch<S>(state: &mut AppState, sink: &S, command: AppCommand) -> DispatchOutcome
where
    S: AppCommandSink + ?Sized,
{
    match sink.try_send(command) {
        Ok(()) => {
            state.set_queue_status(QueueStatus::Ready);
            DispatchOutcome::Sent
        }
        Err(CommandQueueError::Full) => {
            state.set_queue_status(QueueStatus::Busy);
            DispatchOutcome::Busy
        }
        Err(CommandQueueError::Disconnected) => {
            state.set_queue_status(QueueStatus::Disconnected);
            DispatchOutcome::Disconnected
        }
    }
}

pub fn apply_ui_effects<C>(state: &mut AppState, clipboard: &mut C, effects: Vec<UiEffect>)
where
    C: ClipboardAdapter + ?Sized,
{
    for effect in effects {
        match effect {
            UiEffect::WriteHostClipboard { session_id, text } => {
                let status = if clipboard.write_text(text.into_string()).is_ok() {
                    ClipboardStatus::Received
                } else {
                    ClipboardStatus::Unavailable
                };
                state.set_clipboard_status(session_id, status);
            }
        }
    }
}

pub(crate) fn selected_tab(state: &AppState) -> Option<&SessionTabState> {
    state.selected_session()
}
