use crate::{
    connection::FbRect,
    model::VmId,
    ssh::InventorySnapshot,
    vnc::{ClipboardText, InputError, VncOptions},
};

use super::{PublicError, SessionId, SessionSnapshot};

pub use crate::connection::{DesktopSize, ResizeProtocolOutcome};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenOptions {
    pub vnc: VncOptions,
    pub view_only: bool,
    pub clipboard_enabled: bool,
    pub dynamic_resolution: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            vnc: VncOptions::default(),
            view_only: false,
            clipboard_enabled: false,
            dynamic_resolution: true,
        }
    }
}

pub enum InputAction {
    Key {
        down: bool,
        keysym: u32,
    },
    Pointer {
        buttons: u8,
        x: u16,
        y: u16,
    },
    ReleaseOwnedInput {
        pointer_position: Option<(u16, u16)>,
    },
    CtrlAltDelete,
    ReleaseAllKeys,
    SetViewOnly(bool),
    SendClipboard(String),
    ReceiveClipboard,
}

pub enum AppCommand {
    RefreshInventory,
    Open {
        vmid: VmId,
        options: OpenOptions,
    },
    Reconnect {
        session_id: SessionId,
    },
    Close {
        session_id: SessionId,
    },
    SendInput {
        session_id: SessionId,
        action: InputAction,
    },
    ViewportChanged {
        session_id: SessionId,
        backing_width: u32,
        backing_height: u32,
    },
    SetDynamicResolution {
        session_id: SessionId,
        enabled: bool,
    },
    RetryDynamicResolution {
        session_id: SessionId,
    },
    Shutdown,
}

pub enum AppEvent {
    CachedInventory(InventorySnapshot),
    LiveInventory(InventorySnapshot),
    SessionChanged(SessionSnapshot),
    FocusExisting {
        session_id: SessionId,
    },
    Framebuffer {
        session_id: SessionId,
        rects: Vec<FbRect>,
    },
    ClipboardReceived {
        session_id: SessionId,
        text: ClipboardText,
    },
    InputRejected {
        session_id: SessionId,
        reason: InputError,
    },
    Error(PublicError),
}

pub enum SessionTransportEvent {
    Framebuffer(Vec<FbRect>),
    DesktopSize(DesktopSize),
    ResizeOutcome(ResizeProtocolOutcome),
    Error(PublicError),
    Disconnected,
}
