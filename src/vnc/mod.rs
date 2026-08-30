mod client;
pub mod encoding;
mod framebuffer;
mod limits;
pub mod messages;
mod security;
mod wire;

pub use client::{read_server_init, ServerInit, VncClient, VncOptions};
pub use framebuffer::{CheckedRect, Framebuffer};
pub use limits::{validate_framebuffer_layout, FramebufferLayout, ProtocolLimits};
pub use security::{negotiate_security_type, negotiate_version, read_security_result, RfbVersion};
pub use wire::{RfbError, RfbErrorKind, RfbPhase, RfbReader};
