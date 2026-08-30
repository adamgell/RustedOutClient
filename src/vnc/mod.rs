mod client;
pub mod encoding;
mod framebuffer;
mod input;
mod limits;
pub mod messages;
mod security;
mod wire;

pub use client::{
    encode_set_desktop_size, encode_set_encodings, normalize_resize_request,
    parse_extended_desktop_size, read_server_init, ExtendedDesktopSize, ServerInit, VncClient,
    VncOptions,
};
pub use framebuffer::{CheckedRect, Framebuffer};
pub use input::{ClipboardText, InputController, InputError, InputSink, CLIPBOARD_TEXT_LIMIT};
pub use limits::{validate_framebuffer_layout, FramebufferLayout, ProtocolLimits};
pub use security::{negotiate_security_type, negotiate_version, read_security_result, RfbVersion};
pub use wire::{RfbError, RfbErrorKind, RfbPhase, RfbReader};
