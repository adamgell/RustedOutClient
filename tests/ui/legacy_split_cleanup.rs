use rustedoutclient::{
    app::UiAction,
    session::{InputAction, SessionId},
};

fn main() {
    let session_id = SessionId::new();
    let _ = InputAction::ReleasePointer { x: 1, y: 2 };
    let _ = InputAction::FocusLost;
    let _ = UiAction::ReleasePointer {
        session_id,
        x: 1,
        y: 2,
    };
    let _ = UiAction::FocusLost { session_id };
}
