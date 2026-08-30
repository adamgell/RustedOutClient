use rustedoutclient::{
    connection::VncCommand,
    session::InputAction,
};

fn main() {
    let _ = InputAction::Forward(VncCommand::Disconnect);
}
