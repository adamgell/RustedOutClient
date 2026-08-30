use std::fmt::Debug;

use rustedoutclient::{
    session::InputAction,
    vnc::ClipboardText,
};

fn requires_debug<T: Debug>() {}

fn main() {
    requires_debug::<ClipboardText>();
    requires_debug::<InputAction>();
}
