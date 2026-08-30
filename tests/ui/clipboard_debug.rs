use std::fmt::Debug;

use rustedoutclient::{
    connection::{VncCommand, VncEvent},
    session::{AppCommand, AppEvent, InputAction},
    vnc::ClipboardText,
};

fn requires_debug<T: Debug>() {}

fn main() {
    requires_debug::<ClipboardText>();
    requires_debug::<VncCommand>();
    requires_debug::<VncEvent>();
    requires_debug::<InputAction>();
    requires_debug::<AppCommand>();
    requires_debug::<AppEvent>();
}
