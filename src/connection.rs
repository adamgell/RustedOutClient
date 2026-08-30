use crossbeam_channel::{Receiver, Sender, TrySendError};

use crate::vnc::RfbError;

pub const VNC_QUEUE_CAPACITY: usize = 256;

/// A single changed rectangle: tightly-packed `w*h*4` RGBA bytes at `(x, y)`.
pub struct FbRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// Messages sent from the VNC task to the UI.
pub enum VncEvent {
    DesktopSize(u32, u32),
    FramebufferRects(Vec<FbRect>),
    DesktopName(String),
    ClipboardText(String),
    Error(RfbError),
    Disconnected,
}

/// Commands sent from the UI to the VNC task.
pub enum VncCommand {
    KeyEvent { down: bool, keysym: u32 },
    PointerEvent { buttons: u8, x: u16, y: u16 },
    SetClipboard(String),
    Disconnect,
}

/// UI-owned ends of one bounded VNC session's queues.
pub struct VncConnection {
    pub event_rx: Receiver<VncEvent>,
    pub command_tx: Sender<VncCommand>,
}

/// Runtime-owned ends passed to [`crate::vnc::VncClient::run`].
pub struct VncSessionChannels {
    pub event_tx: Sender<VncEvent>,
    pub command_rx: Receiver<VncCommand>,
}

/// Creates the only supported command/event queue shape.
pub fn bounded_vnc_channels() -> (VncConnection, VncSessionChannels) {
    let (event_tx, event_rx) = crossbeam_channel::bounded(VNC_QUEUE_CAPACITY);
    let (command_tx, command_rx) = crossbeam_channel::bounded(VNC_QUEUE_CAPACITY);
    (
        VncConnection {
            event_rx,
            command_tx,
        },
        VncSessionChannels {
            event_tx,
            command_rx,
        },
    )
}

impl VncConnection {
    pub fn send_key(&self, down: bool, keysym: u32) -> Result<(), TrySendError<VncCommand>> {
        self.command_tx
            .try_send(VncCommand::KeyEvent { down, keysym })
    }

    pub fn send_pointer(
        &self,
        buttons: u8,
        x: u16,
        y: u16,
    ) -> Result<(), TrySendError<VncCommand>> {
        self.command_tx
            .try_send(VncCommand::PointerEvent { buttons, x, y })
    }

    pub fn send_clipboard(&self, text: String) -> Result<(), TrySendError<VncCommand>> {
        self.command_tx.try_send(VncCommand::SetClipboard(text))
    }

    pub fn disconnect(&self) -> Result<(), TrySendError<VncCommand>> {
        self.command_tx.try_send(VncCommand::Disconnect)
    }
}
