use rustedoutclient::session::{PublicError, PublicErrorKind};

fn main() {
    let _ = PublicError::new(PublicErrorKind::Cleanup).with_cleanup_failure();
}
