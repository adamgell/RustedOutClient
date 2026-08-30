use crate::{
    connection::{FbRect, VncCommand},
    model::VmId,
    ssh::InventorySnapshot,
    vnc::VncOptions,
};

use super::{PublicError, SessionId, SessionSnapshot};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenOptions {
    pub vnc: VncOptions,
}

pub enum InputAction {
    Forward(VncCommand),
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
    Error(PublicError),
}

pub enum SessionTransportEvent {
    Framebuffer(Vec<FbRect>),
    Error(PublicError),
    Disconnected,
}
