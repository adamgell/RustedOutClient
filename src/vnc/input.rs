use std::collections::BTreeSet;

use thiserror::Error;

use crate::connection::VncConnection;

pub const CLIPBOARD_TEXT_LIMIT: usize = 1_048_576;

const CONTROL_L: u32 = 0xFFE3;
const ALT_L: u32 = 0xFFE9;
const DELETE: u32 = 0xFFFF;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum InputError {
    #[error("session is not ready for input")]
    NotReady,
    #[error("session is view-only")]
    ViewOnly,
    #[error("clipboard actions are disabled")]
    ClipboardDisabled,
    #[error("clipboard text exceeds the protocol limit")]
    ClipboardTooLarge,
    #[error("remote clipboard text is not valid UTF-8")]
    InvalidClipboardText,
    #[error("bounded VNC command queue is unavailable")]
    QueueUnavailable,
}

/// Bounded UTF-8 text received from the remote RFB peer.
///
/// This type intentionally implements neither `Debug` nor `Display` so its
/// sensitive ephemeral content cannot be included accidentally in diagnostics.
pub struct ClipboardText(String);

impl ClipboardText {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<Vec<u8>> for ClipboardText {
    type Error = InputError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        if bytes.len() > CLIPBOARD_TEXT_LIMIT {
            return Err(InputError::ClipboardTooLarge);
        }
        String::from_utf8(bytes)
            .map(Self)
            .map_err(|_| InputError::InvalidClipboardText)
    }
}

impl TryFrom<String> for ClipboardText {
    type Error = InputError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.len() > CLIPBOARD_TEXT_LIMIT {
            return Err(InputError::ClipboardTooLarge);
        }
        Ok(Self(text))
    }
}

/// The narrow low-level boundary owned by an [`InputController`].
pub trait InputSink {
    fn key(&mut self, down: bool, keysym: u32) -> Result<(), InputError>;
    fn pointer(&mut self, buttons: u8, x: u16, y: u16) -> Result<(), InputError>;
    fn send_clipboard(&mut self, text: String) -> Result<(), InputError>;
}

impl InputSink for VncConnection {
    fn key(&mut self, down: bool, keysym: u32) -> Result<(), InputError> {
        self.send_key(down, keysym)
            .map_err(|_| InputError::QueueUnavailable)
    }

    fn pointer(&mut self, buttons: u8, x: u16, y: u16) -> Result<(), InputError> {
        self.send_pointer(buttons, x, y)
            .map_err(|_| InputError::QueueUnavailable)
    }

    fn send_clipboard(&mut self, text: String) -> Result<(), InputError> {
        VncConnection::send_clipboard(self, text).map_err(|_| InputError::QueueUnavailable)
    }
}

/// Enforces all readiness, view-only, tracked-key, secure-attention, and
/// clipboard policy before a command can reach the VNC queue.
pub struct InputController<S> {
    sink: S,
    pressed: BTreeSet<u32>,
    ready: bool,
    view_only: bool,
    clipboard_enabled: bool,
    pending_clipboard: Option<ClipboardText>,
}

impl<S> InputController<S>
where
    S: InputSink,
{
    pub fn new(sink: S, view_only: bool, clipboard_enabled: bool) -> Self {
        Self {
            sink,
            pressed: BTreeSet::new(),
            ready: false,
            view_only,
            clipboard_enabled,
            pending_clipboard: None,
        }
    }

    pub fn mark_ready(&mut self) {
        self.ready = true;
    }

    pub fn key(&mut self, down: bool, keysym: u32) -> Result<(), InputError> {
        self.require_interactive()?;
        if down {
            self.pressed.insert(keysym);
            self.sink.key(true, keysym)
        } else {
            let result = self.sink.key(false, keysym);
            self.pressed.remove(&keysym);
            result
        }
    }

    pub fn pointer(&mut self, buttons: u8, x: u16, y: u16) -> Result<(), InputError> {
        self.require_interactive()?;
        self.sink.pointer(buttons, x, y)
    }

    pub fn ctrl_alt_delete(&mut self) -> Result<(), InputError> {
        self.require_interactive()?;
        for (down, keysym) in [
            (true, CONTROL_L),
            (true, ALT_L),
            (true, DELETE),
            (false, DELETE),
            (false, ALT_L),
            (false, CONTROL_L),
        ] {
            if let Err(primary) = self.key(down, keysym) {
                let _ = self.release_all_keys();
                return Err(primary);
            }
        }
        Ok(())
    }

    pub fn release_all_keys(&mut self) -> Result<(), InputError> {
        let keys = self.pressed.iter().rev().copied().collect::<Vec<_>>();
        let mut first_error = None;
        for keysym in keys {
            if let Err(error) = self.sink.key(false, keysym) {
                first_error.get_or_insert(error);
            }
        }
        self.pressed.clear();
        first_error.map_or(Ok(()), Err)
    }

    pub fn focus_lost(&mut self) -> Result<(), InputError> {
        self.release_all_keys()
    }

    pub fn set_view_only(&mut self, enabled: bool) -> Result<(), InputError> {
        if !enabled {
            self.view_only = false;
            return Ok(());
        }
        let result = self.release_all_keys();
        self.view_only = true;
        result
    }

    pub fn send_clipboard(&mut self, text: String) -> Result<(), InputError> {
        self.require_clipboard()?;
        if text.len() > CLIPBOARD_TEXT_LIMIT {
            return Err(InputError::ClipboardTooLarge);
        }
        self.sink.send_clipboard(text)
    }

    pub fn receive_clipboard(&mut self) -> Result<Option<ClipboardText>, InputError> {
        self.require_clipboard()?;
        Ok(self.pending_clipboard.take())
    }

    pub fn buffer_remote_clipboard(&mut self, text: ClipboardText) {
        if self.clipboard_enabled {
            self.pending_clipboard = Some(text);
        }
    }

    pub fn clear_session(&mut self) -> Result<(), InputError> {
        let result = self.release_all_keys();
        self.pending_clipboard = None;
        self.ready = false;
        result
    }

    fn require_interactive(&self) -> Result<(), InputError> {
        if !self.ready {
            return Err(InputError::NotReady);
        }
        if self.view_only {
            return Err(InputError::ViewOnly);
        }
        Ok(())
    }

    fn require_clipboard(&self) -> Result<(), InputError> {
        self.require_interactive()?;
        if !self.clipboard_enabled {
            return Err(InputError::ClipboardDisabled);
        }
        Ok(())
    }
}

impl InputController<VncConnection> {
    pub(crate) fn connection(&self) -> &VncConnection {
        &self.sink
    }
}
