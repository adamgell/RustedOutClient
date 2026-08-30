use crate::{
    connection::FbRect,
    model::VmId,
    ssh::InventorySnapshot,
    vnc::{ClipboardText, InputError, VncOptions},
};

use super::{PublicError, SessionId, SessionSnapshot};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenOptions {
    pub vnc: VncOptions,
    pub view_only: bool,
    pub clipboard_enabled: bool,
}

pub enum InputAction {
    Key { down: bool, keysym: u32 },
    Pointer { buttons: u8, x: u16, y: u16 },
    CtrlAltDelete,
    ReleaseAllKeys,
    FocusLost,
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
    Error(PublicError),
    Disconnected,
}
