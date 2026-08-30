use std::fmt;

#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use rand::{distributions::Alphanumeric, rngs::OsRng, Rng};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use crate::model::VmId;

use super::{
    InventoryError, ProxyStream, ProxyStreamError, SshMasterError, VerifiedSshMaster,
    VmInventoryItem, VmStatus,
};

const TICKET_LENGTH: usize = 8;

#[cfg(test)]
static PROXY_TICKET_GENERATIONS: AtomicUsize = AtomicUsize::new(0);

/// An eight-byte, OS-CSPRNG-generated Proxmox VNC proxy credential.
///
/// It intentionally implements neither `Clone`, `Display`, nor serialization.
pub struct ProxyTicket {
    value: SecretString,
    // Field order is intentional: the secret is dropped before the test signal fires.
    #[cfg(test)]
    _drop_signal: TestTicketDropSignal,
}

#[cfg(test)]
struct TestTicketDropSignal(Option<tokio::sync::oneshot::Sender<()>>);

#[cfg(test)]
impl Drop for TestTicketDropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl ProxyTicket {
    pub fn generate() -> Self {
        let value: String = OsRng
            .sample_iter(&Alphanumeric)
            .take(TICKET_LENGTH)
            .map(char::from)
            .collect();
        Self {
            value: SecretString::from(value),
            #[cfg(test)]
            _drop_signal: TestTicketDropSignal(None),
        }
    }

    pub fn auth_len(&self) -> usize {
        self.value.expose_secret().len()
    }

    pub fn auth_is_ascii_alphanumeric(&self) -> bool {
        self.value
            .expose_secret()
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
    }

    pub(crate) fn expose_for_auth(&self) -> &str {
        self.value.expose_secret()
    }

    #[cfg(test)]
    pub(crate) fn for_auth_test_with_drop_signal(
        value: &str,
    ) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (
            Self {
                value: SecretString::from(value),
                _drop_signal: TestTicketDropSignal(Some(sender)),
            },
            receiver,
        )
    }

    fn generate_for_proxy() -> Self {
        #[cfg(test)]
        PROXY_TICKET_GENERATIONS.fetch_add(1, Ordering::SeqCst);
        Self::generate()
    }

    #[cfg(test)]
    pub(crate) fn test_generation_count() -> usize {
        PROXY_TICKET_GENERATIONS.load(Ordering::SeqCst)
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
    #[error("the selected VM was not present in verified live inventory")]
    VmNotFound,
    #[error("the selected VM is not running")]
    VmNotRunning,
}

fn validate_openable(item: &VmInventoryItem) -> Result<(), ProxyOpenError> {
    if item.status != VmStatus::Running {
        return Err(ProxyOpenError::VmNotRunning);
    }
    Ok(())
}

/// Trusted transport input for the VNC authentication boundary.
pub struct TrustedSshProxy {
    stream: ProxyStream,
    ticket: ProxyTicket,
}

enum ProxySetup {
    Production,
    #[cfg(test)]
    MissingStdout(PathBuf),
}

impl TrustedSshProxy {
    pub async fn connect(
        master: &mut VerifiedSshMaster<'_>,
        vmid: VmId,
    ) -> Result<Self, ProxyOpenError> {
        Self::connect_inner(master, vmid, ProxySetup::Production).await
    }

    #[cfg(test)]
    async fn connect_with_missing_stdout_for_test(
        master: &mut VerifiedSshMaster<'_>,
        vmid: VmId,
        startup_gate: PathBuf,
    ) -> Result<Self, ProxyOpenError> {
        Self::connect_inner(master, vmid, ProxySetup::MissingStdout(startup_gate)).await
    }

    async fn connect_inner(
        master: &mut VerifiedSshMaster<'_>,
        vmid: VmId,
        setup: ProxySetup,
    ) -> Result<Self, ProxyOpenError> {
        master.recheck().await?;
        let inventory = master.fetch_inventory().await?;
        let item = inventory
            .vms
            .iter()
            .find(|item| item.vmid == vmid)
            .ok_or(ProxyOpenError::VmNotFound)?;
        validate_openable(item)?;

        let ticket = ProxyTicket::generate_for_proxy();
        let spec = master.proxy_spec(vmid, &ticket);
        let stream = match setup {
            ProxySetup::Production => ProxyStream::spawn(spec).await?,
            #[cfg(test)]
            ProxySetup::MissingStdout(startup_gate) => {
                ProxyStream::spawn_with_missing_stdout_for_test(spec, startup_gate).await?
            }
        };
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
        collections::BTreeSet,
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

    use super::{ProxyOpenError, ProxyTicket, TrustedSshProxy};
    use crate::{
        model::{NodeName, PveProfile, SshTarget, VmId},
        runtime::RuntimeDir,
        ssh::{ProxyIoStage, SshCommandFactory, SshMaster},
    };

    struct ParentTicketEnvironment(Option<std::ffi::OsString>);

    impl ParentTicketEnvironment {
        fn install_synthetic_sentinel() -> Self {
            let previous = std::env::var_os("LC_PVE_TICKET");
            std::env::set_var("LC_PVE_TICKET", "SENTINEL");
            Self(previous)
        }
    }

    impl Drop for ParentTicketEnvironment {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                std::env::set_var("LC_PVE_TICKET", previous);
            } else {
                std::env::remove_var("LC_PVE_TICKET");
            }
        }
    }

    fn fixture_profile() -> PveProfile {
        PveProfile {
            name: "Synthetic Proxmox".to_owned(),
            ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
            node: NodeName::parse("pve2").unwrap(),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn post_spawn_proxy_setup_failure_reaps_the_exact_owned_child() {
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
        let startup_gate = runtime
            .control_socket()
            .with_extension("allow_proxy_setup_failure");
        let pid_path = runtime.control_socket().with_extension("proxy.pid");

        let connect = TrustedSshProxy::connect_with_missing_stdout_for_test(
            &mut verified,
            VmId::new(107).unwrap(),
            startup_gate.clone(),
        );
        let release_fault = async {
            wait_for(&pid_path).await;
            let pid = helper_pid(&pid_path);
            fs::write(&startup_gate, b"synthetic fixture control\n").unwrap();
            pid
        };
        let (result, proxy_pid) = tokio::join!(connect, release_fault);
        let error = match result {
            Err(ProxyOpenError::Stream(error)) => error,
            Err(error) => panic!("unexpected proxy setup error: {error}"),
            Ok(_) => panic!("post-spawn proxy setup unexpectedly succeeded"),
        };

        assert_eq!(
            error.io_failure().unwrap().stage(),
            ProxyIoStage::SetupStdout
        );
        assert_exact_pid_is_gone(proxy_pid).await;
        master.close().await.unwrap();
    }

    fn assert_exact_regular_artifacts_exclude_ticket(
        directory: &Path,
        expected_names: &[&str],
        ticket: &ProxyTicket,
    ) {
        let mut observed = BTreeSet::new();
        for entry in fs::read_dir(directory).expect("runtime directory must be readable") {
            let entry = entry.expect("every runtime directory entry must be readable");
            let metadata = fs::symlink_metadata(entry.path())
                .expect("every runtime artifact must have readable metadata");
            assert!(
                metadata.file_type().is_file(),
                "runtime artifacts must all be regular files"
            );
            let name = entry
                .file_name()
                .into_string()
                .expect("runtime artifact names must be UTF-8");
            assert!(
                observed.insert(name),
                "runtime artifact names must be unique"
            );
            let bytes = fs::read(entry.path()).expect("every runtime artifact must be readable");
            assert!(!bytes
                .windows(ticket.expose_for_auth().len())
                .any(|window| window == ticket.expose_for_auth().as_bytes()));
        }
        let expected = expected_names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(observed, expected);
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
        let _parent_environment = ParentTicketEnvironment::install_synthetic_sentinel();
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;

        let mut verified = master.verify().await.unwrap();
        let proxy = TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap())
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
        master.close().await.unwrap();
        assert_exact_regular_artifacts_exclude_ticket(
            runtime.path(),
            &[
                "c.argv",
                "c.check.pid",
                "c.exit.pid",
                "c.inventory.pid",
                "c.proxy.env-valid",
                "c.proxy.pid",
            ],
            &ticket,
        );
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
        let proxy = TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap())
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
        let proxy = TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap())
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
        let proxy = TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap())
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
    async fn connect_refetches_and_rejects_stopped_template_and_missing_vm_before_ticket_or_spawn()
    {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let mut verified = master.verify().await.unwrap();

        let payload = runtime.control_socket().with_extension("inventory_payload");
        let generation_count = ProxyTicket::test_generation_count();

        fs::write(
            &payload,
            br#"[{"vmid":107,"name":"SYNTHETIC-107","status":"running","template":0}]"#,
        )
        .unwrap();
        let _caller_snapshot = verified.fetch_inventory().await.unwrap();
        fs::write(
            &payload,
            br#"[{"vmid":107,"name":"SYNTHETIC-107","status":"stopped","template":0}]"#,
        )
        .unwrap();
        assert!(matches!(
            TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap()).await,
            Err(ProxyOpenError::VmNotRunning)
        ));

        fs::write(
            &payload,
            br#"[{"vmid":107,"name":"SYNTHETIC-107","status":"running","template":1}]"#,
        )
        .unwrap();
        assert!(matches!(
            TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap()).await,
            Err(ProxyOpenError::VmNotFound)
        ));

        fs::write(
            &payload,
            br#"[{"vmid":205,"name":"SYNTHETIC-205","status":"running","template":0}]"#,
        )
        .unwrap();
        assert!(matches!(
            TrustedSshProxy::connect(&mut verified, VmId::new(999).unwrap()).await,
            Err(ProxyOpenError::VmNotFound)
        ));

        assert_eq!(ProxyTicket::test_generation_count(), generation_count);

        let argv = fs::read_to_string(runtime.control_socket().with_extension("argv")).unwrap();
        assert!(!argv.contains("exec /usr/sbin/qm vncproxy"));
        assert!(!runtime
            .control_socket()
            .with_extension("proxy.pid")
            .exists());

        master.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connect_rechecks_master_before_inventory_ticket_and_proxy_spawn() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());
        let mut master = SshMaster::start(factory, fixture_profile()).await.unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;
        let mut verified = master.verify().await.unwrap();
        fs::remove_file(runtime.control_socket().with_extension("state")).unwrap();
        let generation_count = ProxyTicket::test_generation_count();

        assert!(matches!(
            TrustedSshProxy::connect(&mut verified, VmId::new(107).unwrap()).await,
            Err(ProxyOpenError::Master(_))
        ));
        assert_eq!(ProxyTicket::test_generation_count(), generation_count);

        let argv = fs::read_to_string(runtime.control_socket().with_extension("argv")).unwrap();
        assert!(!argv.contains("pvesh get"));
        assert!(!argv.contains("exec /usr/sbin/qm vncproxy"));
        assert!(!runtime
            .control_socket()
            .with_extension("proxy.pid")
            .exists());

        master.close().await.unwrap();
    }
}
