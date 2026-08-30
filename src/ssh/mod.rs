mod command;
mod error;

pub use command::{CommandSpec, SshCommandFactory};
pub use error::{classify_stderr, SshFailure, SshFailureKind};
