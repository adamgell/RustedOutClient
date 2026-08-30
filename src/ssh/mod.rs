mod command;
mod error;
mod inventory;
mod master;

pub use command::{CommandSpec, SshCommandFactory};
pub use error::{classify_stderr, SshFailure, SshFailureKind};
pub(crate) use inventory::normalize_inventory_snapshot;
pub use inventory::{
    InventoryClient, InventoryError, InventorySelectionError, InventorySnapshot, VmInventoryItem,
    VmStatus,
};
pub use master::{SshMaster, SshMasterError};
