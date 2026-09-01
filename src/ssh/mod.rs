mod command;
mod error;
mod inventory;
mod master;
mod proxy;
mod stream;

pub use command::{CommandSpec, SshCommandFactory};
pub use error::{classify_stderr, SshFailure, SshFailureKind};
pub(crate) use inventory::normalize_inventory_snapshot;
pub use inventory::{
    InventoryClient, InventoryError, InventorySelectionError, InventorySnapshot, VmInventoryItem,
    VmStatus,
};
pub use master::{SshMaster, SshMasterError, VerifiedSshMaster};
pub use proxy::{ProxyOpenError, ProxyTicket, TrustedSshProxy};
pub use stream::{
    ProxyCleanupFailure, ProxyCleanupStage, ProxyIoFailure, ProxyIoStage, ProxyReapState,
    ProxyStream, ProxyStreamError,
};

#[cfg(test)]
pub(crate) async fn process_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    use std::sync::OnceLock;

    static PROCESS_TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    PROCESS_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}
