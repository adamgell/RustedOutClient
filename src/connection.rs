use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use tokio::sync::oneshot;

use crate::vnc::{ClipboardText, RfbError};

pub const VNC_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopSize {
    pub width: u16,
    pub height: u16,
}

impl DesktopSize {
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizeProtocolOutcome {
    Forwarded(DesktopSize),
    Rejected,
    Unsupported,
    ServerUnsupported,
}

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
    DesktopSize(DesktopSize),
    ResizeOutcome(ResizeProtocolOutcome),
    FramebufferRects(Vec<FbRect>),
    DesktopName(String),
    Error(RfbError),
    Disconnected,
}

/// Commands sent from the UI to the VNC task.
pub enum VncCommand {
    KeyEvent { down: bool, keysym: u32 },
    PointerEvent { buttons: u8, x: u16, y: u16 },
    SetClipboard(String),
    SetDesktopSize(DesktopSize),
    GracefulDisconnect(CloseBarrier),
    Disconnect,
}

/// Opaque acknowledgement carried only by the session's ordered command queue.
pub struct CloseBarrier {
    acknowledged: oneshot::Sender<()>,
}

impl CloseBarrier {
    pub(crate) fn acknowledge(self) {
        let _ = self.acknowledged.send(());
    }
}

/// The session's sole retained remote clipboard value.
#[derive(Clone, Default)]
pub(crate) struct ClipboardSlot(Arc<Mutex<Option<ClipboardText>>>);

impl ClipboardSlot {
    pub(crate) fn replace(&self, text: ClipboardText) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(text);
    }

    pub(crate) fn take(&self) -> Option<ClipboardText> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    pub(crate) fn clear(&self) {
        drop(self.take());
    }

    #[cfg(test)]
    pub(crate) fn retained_count(&self) -> usize {
        usize::from(
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
        )
    }
}

/// UI-owned ends of one bounded VNC session's queues.
pub struct VncConnection {
    pub event_rx: Receiver<VncEvent>,
    pub command_tx: Sender<VncCommand>,
    pub(crate) clipboard: ClipboardSlot,
}

/// Runtime-owned ends passed to [`crate::vnc::VncClient::run`].
pub struct VncSessionChannels {
    pub event_tx: Sender<VncEvent>,
    pub command_rx: Receiver<VncCommand>,
    pub(crate) clipboard: ClipboardSlot,
}

/// Creates the only supported command/event queue shape.
pub fn bounded_vnc_channels() -> (VncConnection, VncSessionChannels) {
    let (event_tx, event_rx) = crossbeam_channel::bounded(VNC_QUEUE_CAPACITY);
    let (command_tx, command_rx) = crossbeam_channel::bounded(VNC_QUEUE_CAPACITY);
    let clipboard = ClipboardSlot::default();
    (
        VncConnection {
            event_rx,
            command_tx,
            clipboard: clipboard.clone(),
        },
        VncSessionChannels {
            event_tx,
            command_rx,
            clipboard,
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

    pub(crate) fn request_desktop_size(
        &self,
        size: DesktopSize,
    ) -> Result<(), TrySendError<VncCommand>> {
        self.command_tx.try_send(VncCommand::SetDesktopSize(size))
    }

    pub fn disconnect(&self) -> Result<(), TrySendError<VncCommand>> {
        self.command_tx.try_send(VncCommand::Disconnect)
    }

    pub(crate) fn begin_graceful_close(
        &self,
    ) -> Result<oneshot::Receiver<()>, TrySendError<VncCommand>> {
        let (acknowledged, receiver) = oneshot::channel();
        self.command_tx
            .try_send(VncCommand::GracefulDisconnect(CloseBarrier {
                acknowledged,
            }))?;
        Ok(receiver)
    }
}
