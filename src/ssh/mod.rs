mod command;
mod error;
mod inventory;
mod master;

pub use command::{CommandSpec, SshCommandFactory};
pub use error::{classify_stderr, SshFailure, SshFailureKind};
pub use inventory::{
    InventoryClient, InventoryError, InventorySelectionError, InventorySnapshot, VmInventoryItem,
    VmStatus,
};
pub use master::{SshMaster, SshMasterError};
