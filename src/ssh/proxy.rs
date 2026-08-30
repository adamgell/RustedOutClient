use std::{
    fmt,
    time::{Duration, Instant},
};

use rand::{distributions::Alphanumeric, rngs::OsRng, Rng};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use crate::model::VmId;

use super::{
    InventoryError, InventorySnapshot, ProxyStream, ProxyStreamError, SshMasterError,
    VerifiedSshMaster, VmInventoryItem, VmStatus,
};

const TICKET_LENGTH: usize = 8;
const LIVE_INVENTORY_MAX_AGE: Duration = Duration::from_secs(30);

/// An eight-byte, OS-CSPRNG-generated Proxmox VNC proxy credential.
///
/// It intentionally implements neither `Clone`, `Display`, nor serialization.
pub struct ProxyTicket(SecretString);

impl ProxyTicket {
    pub fn generate() -> Self {
        let value: String = OsRng
            .sample_iter(&Alphanumeric)
            .take(TICKET_LENGTH)
            .map(char::from)
            .collect();
        Self(SecretString::from(value))
    }

    pub fn auth_len(&self) -> usize {
        self.0.expose_secret().len()
    }

    pub fn auth_is_ascii_alphanumeric(&self) -> bool {
        self.0
            .expose_secret()
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    }

    pub(crate) fn expose_for_auth(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for ProxyTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProxyTicket([REDACTED])")
    }
}

#[derive(Debug, Error)]
pub enum ProxyOpenError {
    #[error(transparent)]
    Master(#[from] SshMasterError),
    #[error(transparent)]
    Inventory(#[from] InventoryError),
    #[error(transparent)]
    Stream(#[from] ProxyStreamError),
    #[error("live inventory is no longer fresh")]
    InventoryStale,
    #[error("the selected VM was not present in verified live inventory")]
    VmNotFound,
    #[error("the selected VM is not running")]
    VmNotRunning,
    #[error("templates cannot be opened as VNC sessions")]
    VmTemplate,
    #[error("the live inventory belongs to a different owned SSH master")]
    WrongMaster,
}

/// Fresh inventory fetched through one verified, owned master.
///
/// There is no public constructor. Cached and caller-created snapshots cannot
/// be converted into this proof.
pub struct VerifiedInventory {
    owner_id: u64,
    fetched_at: Instant,
    snapshot: InventorySnapshot,
}

impl VerifiedInventory {
    pub(super) fn from_live_fetch(owner_id: u64, snapshot: InventorySnapshot) -> Self {
        Self {
            owner_id,
            fetched_at: Instant::now(),
            snapshot,
        }
    }

    pub fn snapshot(&self) -> &InventorySnapshot {
        &self.snapshot
    }

    pub fn running_vm(&self, vmid: VmId) -> Result<VerifiedRunningVm, ProxyOpenError> {
        if self.fetched_at.elapsed() > LIVE_INVENTORY_MAX_AGE || self.snapshot.stale {
            return Err(ProxyOpenError::InventoryStale);
        }
        let item = self
            .snapshot
            .vms
            .iter()
            .find(|item| item.vmid == vmid)
            .ok_or(ProxyOpenError::VmNotFound)?;
        validate_openable(item)?;
        Ok(VerifiedRunningVm {
            owner_id: self.owner_id,
            vmid,
            validated_at: self.fetched_at,
        })
    }

    #[cfg(test)]
    fn for_test(owner_id: u64, fetched_at: Instant, vms: Vec<VmInventoryItem>) -> Self {
        Self {
            owner_id,
            fetched_at,
            snapshot: InventorySnapshot {
                observed_at_unix_ms: 1,
                stale: false,
                vms,
            },
        }
    }
}

fn validate_openable(item: &VmInventoryItem) -> Result<(), ProxyOpenError> {
    if item.template {
        return Err(ProxyOpenError::VmTemplate);
    }
    if item.status != VmStatus::Running {
        return Err(ProxyOpenError::VmNotRunning);
    }
    Ok(())
}

/// Opaque proof that one VM was running in fresh live inventory.
pub struct VerifiedRunningVm {
    owner_id: u64,
    vmid: VmId,
    validated_at: Instant,
}

/// Trusted transport input for the VNC authentication boundary.
pub struct TrustedSshProxy {
    stream: ProxyStream,
    ticket: ProxyTicket,
}

impl TrustedSshProxy {
    pub async fn connect(
        master: &mut VerifiedSshMaster<'_>,
        vm: VerifiedRunningVm,
    ) -> Result<Self, ProxyOpenError> {
        if master.owner_id() != vm.owner_id {
            return Err(ProxyOpenError::WrongMaster);
        }
        if vm.validated_at.elapsed() > LIVE_INVENTORY_MAX_AGE {
            return Err(ProxyOpenError::InventoryStale);
        }

        master.recheck().await?;
        let ticket = ProxyTicket::generate();
        let spec = master.proxy_spec(vm.vmid, &ticket);
        let stream = ProxyStream::spawn(spec)?;
        Ok(Self { stream, ticket })
    }

    /// Transfers the verified byte stream and still-redacted ticket to the
    /// native VNC authentication boundary.
    pub fn into_parts(self) -> (ProxyStream, ProxyTicket) {
        (self.stream, self.ticket)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{tempdir, TempDir};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        time::{sleep, timeout},
    };

    use super::{ProxyOpenError, ProxyTicket, TrustedSshProxy, VerifiedInventory};
    use crate::{
        model::{NodeName, PveProfile, SshTarget, VmId},
        runtime::RuntimeDir,
        ssh::{SshCommandFactory, SshMaster, VmInventoryItem, VmStatus},
    };

    fn fixture_profile() -> PveProfile {
        PveProfile {
            name: "Synthetic Proxmox".to_owned(),
            ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
            node: NodeName::parse("pve2").unwrap(),
        }
    }

    fn vm(vmid: u32, status: VmStatus, template: bool) -> VmInventoryItem {
        VmInventoryItem {
            vmid: VmId::new(vmid).unwrap(),
            name: format!("SYNTHETIC-{vmid}"),
            node: NodeName::parse("pve2").unwrap(),
            status,
            template,
        }
    }

    #[cfg(unix)]
    fn fake_ssh() -> (TempDir, PathBuf) {
        let directory = tempdir().unwrap();
        let executable = directory.path().join("fake_ssh.sh");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_ssh.sh"),
            &executable,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        (directory, executable)
    }

    async fn wait_for(path: &Path) {
        timeout(Duration::from_secs(30), async {
            while !path.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synthetic helper did not become ready");
    }

    fn helper_pid(path: &Path) -> u32 {
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    fn exact_pid_is_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    async fn assert_exact_pid_is_gone(pid: u32) {
        timeout(Duration::from_secs(30), async {
            while exact_pid_is_alive(pid) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned synthetic proxy child was not reaped");
    }

    #[test]
    fn generated_ticket_bytes_are_unique_without_rendering_secret_values() {
        let mut values = Vec::with_capacity(10_000);
        let mut unique = 0_usize;
        for _ in 0..10_000 {
            let ticket = ProxyTicket::generate();
            let value = ticket.expose_for_auth();
            assert_eq!(value.len(), 8);
            assert!(value.bytes().all(|byte| byte.is_ascii_alphanumeric()));
            if values
                .iter()
                .all(|existing: &ProxyTicket| existing.expose_for_auth() != value)
            {
                unique += 1;
            }
            values.push(ticket);
        }

        assert!(unique >= 9_990, "unexpected ticket collision rate");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxy_stream_moves_native_bytes_without_ticket_files_or_argv_exposure() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;

        let mut verified = master.verify().await.unwrap();
        let inventory = verified.fetch_inventory().await.unwrap();
        let running = inventory.running_vm(VmId::new(107).unwrap()).unwrap();
        let proxy = TrustedSshProxy::connect(&mut verified, running)
            .await
            .unwrap();
        let (mut stream, ticket) = proxy.into_parts();
        wait_for(&runtime.control_socket().with_extension("proxy.pid")).await;
        let pid = helper_pid(&runtime.control_socket().with_extension("proxy.pid"));

        let mut banner = [0_u8; 12];
        stream.read_exact(&mut banner).await.unwrap();
        assert_eq!(&banner, b"RFB 003.008\n");
        stream.write_all(b"x").await.unwrap();
        stream.close().await.unwrap();
        stream.close().await.unwrap();
        assert_exact_pid_is_gone(pid).await;

        let argv = fs::read(runtime.control_socket().with_extension("argv")).unwrap();
        assert!(!argv
            .windows(ticket.expose_for_auth().len())
            .any(|window| { window == ticket.expose_for_auth().as_bytes() }));
        let recorded_argv = String::from_utf8_lossy(&argv);
        assert_eq!(recorded_argv.matches("LC_PVE_TICKET").count(), 1);
        assert!(recorded_argv
            .lines()
            .any(|line| line == "SendEnv=LC_PVE_TICKET"));
        assert!(runtime
            .control_socket()
            .with_extension("proxy.env-valid")
            .exists());
        for entry in fs::read_dir(runtime.path()).unwrap() {
            let bytes = fs::read(entry.unwrap().path()).unwrap_or_default();
            assert!(!bytes
                .windows(ticket.expose_for_auth().len())
                .any(|window| { window == ticket.expose_for_auth().as_bytes() }));
        }

        master.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_stderr_is_drained_concurrently_without_entering_public_errors() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime
                .control_socket()
                .with_extension("proxy_large_stderr"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let mut verified = master.verify().await.unwrap();
        let inventory = verified.fetch_inventory().await.unwrap();
        let running = inventory.running_vm(VmId::new(107).unwrap()).unwrap();
        let proxy = TrustedSshProxy::connect(&mut verified, running)
            .await
            .unwrap();
        let (mut stream, _ticket) = proxy.into_parts();

        let mut banner = [0_u8; 12];
        timeout(Duration::from_secs(30), stream.read_exact(&mut banner))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&banner, b"RFB 003.008\n");
        stream.write_all(b"x").await.unwrap();
        timeout(Duration::from_secs(30), stream.close())
            .await
            .unwrap()
            .unwrap();

        master.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn close_waits_three_seconds_kills_exact_owned_child_and_is_idempotent() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("hang_proxy"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let mut verified = master.verify().await.unwrap();
        let inventory = verified.fetch_inventory().await.unwrap();
        let running = inventory.running_vm(VmId::new(107).unwrap()).unwrap();
        let proxy = TrustedSshProxy::connect(&mut verified, running)
            .await
            .unwrap();
        let (mut stream, _ticket) = proxy.into_parts();
        wait_for(&runtime.control_socket().with_extension("proxy.pid")).await;
        let pid = helper_pid(&runtime.control_socket().with_extension("proxy.pid"));
        stream.write_all(b"x").await.unwrap();

        let started = Instant::now();
        stream.close().await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(3),
            "close returned before grace bound"
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "close exceeded finite bound"
        );
        stream.close().await.unwrap();
        assert_exact_pid_is_gone(pid).await;

        master.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_drops_stream_and_cleanup_owner_reaps_exact_child() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let mut verified = master.verify().await.unwrap();
        let inventory = verified.fetch_inventory().await.unwrap();
        let running = inventory.running_vm(VmId::new(107).unwrap()).unwrap();
        let proxy = TrustedSshProxy::connect(&mut verified, running)
            .await
            .unwrap();
        let (stream, _ticket) = proxy.into_parts();
        wait_for(&runtime.control_socket().with_extension("proxy.pid")).await;
        let pid = helper_pid(&runtime.control_socket().with_extension("proxy.pid"));

        let owner = tokio::spawn(async move {
            let _stream = stream;
            std::future::pending::<()>().await;
        });
        owner.abort();
        let _ = owner.await;
        assert_exact_pid_is_gone(pid).await;

        master.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_stopped_template_arbitrary_and_wrong_master_proofs_never_spawn_proxy() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let mut verified = master.verify().await.unwrap();

        let stale = VerifiedInventory::for_test(
            verified.owner_id(),
            Instant::now() - Duration::from_secs(31),
            vec![vm(107, VmStatus::Running, false)],
        );
        assert!(matches!(
            stale.running_vm(VmId::new(107).unwrap()),
            Err(ProxyOpenError::InventoryStale)
        ));
        let stopped = VerifiedInventory::for_test(
            verified.owner_id(),
            Instant::now(),
            vec![vm(107, VmStatus::Stopped, false)],
        );
        assert!(matches!(
            stopped.running_vm(VmId::new(107).unwrap()),
            Err(ProxyOpenError::VmNotRunning)
        ));
        let template = VerifiedInventory::for_test(
            verified.owner_id(),
            Instant::now(),
            vec![vm(107, VmStatus::Running, true)],
        );
        assert!(matches!(
            template.running_vm(VmId::new(107).unwrap()),
            Err(ProxyOpenError::VmTemplate)
        ));
        let live = VerifiedInventory::for_test(
            verified.owner_id(),
            Instant::now(),
            vec![vm(107, VmStatus::Running, false)],
        );
        assert!(matches!(
            live.running_vm(VmId::new(999).unwrap()),
            Err(ProxyOpenError::VmNotFound)
        ));
        let wrong_master = VerifiedInventory::for_test(
            verified.owner_id().wrapping_add(1),
            Instant::now(),
            vec![vm(107, VmStatus::Running, false)],
        );
        let running = wrong_master.running_vm(VmId::new(107).unwrap()).unwrap();
        assert!(matches!(
            TrustedSshProxy::connect(&mut verified, running).await,
            Err(ProxyOpenError::WrongMaster)
        ));

        let argv = fs::read_to_string(runtime.control_socket().with_extension("argv")).unwrap();
        assert!(!argv.contains("qm vncproxy"));
        assert!(!runtime
            .control_socket()
            .with_extension("proxy.pid")
            .exists());

        master.close().await.unwrap();
    }
}
