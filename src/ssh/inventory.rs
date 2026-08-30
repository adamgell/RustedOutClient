use std::{io, process::Stdio, time::SystemTime};

use serde::{de, Deserialize, Deserializer, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
    task::JoinHandle,
    time::{sleep_until, timeout, timeout_at, Duration, Instant},
};

use crate::{
    model::{NodeName, PveProfile, VmId},
    ssh::{master::capture_bounded, SshCommandFactory, SshFailure},
};

use super::classify_stderr;

const MAX_INVENTORY_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_CAPTURED_STDERR_BYTES: usize = 65_536;
#[cfg(not(test))]
const INVENTORY_PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const INVENTORY_PROCESS_TIMEOUT: Duration = Duration::from_secs(2);
const REAP_TIMEOUT: Duration = Duration::from_secs(1);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmStatus {
    Running,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmInventoryItem {
    pub vmid: VmId,
    pub name: String,
    pub node: NodeName,
    pub status: VmStatus,
    pub template: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventorySnapshot {
    pub observed_at_unix_ms: u64,
    pub stale: bool,
    pub vms: Vec<VmInventoryItem>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum InventorySelectionError {
    #[error("no VM matches the selector")]
    NotFound,
    #[error("more than one VM has that name")]
    Ambiguous,
}

#[derive(Debug, Error)]
pub enum InventoryError {
    #[error("could not run SSH inventory: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Ssh(#[from] SshFailure),
    #[error("SSH inventory exceeded the 4 MiB limit")]
    StdoutTooLarge,
    #[error("SSH inventory contained a malformed record")]
    MalformedInventory,
    #[error("system time is before the Unix epoch")]
    InvalidSystemTime,
    #[error("SSH inventory process timed out")]
    ProcessTimedOut,
    #[error("owned SSH inventory child cleanup failed (operation failed: {operation_failed})")]
    OwnedChildCleanupFailed { operation_failed: bool },
}

pub struct InventoryClient;

impl InventoryClient {
    pub async fn fetch(
        factory: &SshCommandFactory,
        profile: &PveProfile,
    ) -> Result<InventorySnapshot, InventoryError> {
        let spec = factory.inventory(profile).unwrap();
        let mut command = tokio::process::Command::from(spec.to_command());
        command.stdout(Stdio::piped());
        command.kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("SSH inventory stdout pipe was not available"))?;
        let stderr_task = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(capture_bounded(stderr, MAX_CAPTURED_STDERR_BYTES)));
        let mut stdout_task = tokio::spawn(read_capped(stdout, MAX_INVENTORY_STDOUT_BYTES));
        let deadline = Instant::now() + INVENTORY_PROCESS_TIMEOUT;

        enum FirstCompletion {
            Stdout(Result<Result<Vec<u8>, InventoryError>, tokio::task::JoinError>),
            Status(io::Result<std::process::ExitStatus>),
            TimedOut,
        }

        let first = tokio::select! {
            stdout = &mut stdout_task => FirstCompletion::Stdout(stdout),
            status = child.wait() => FirstCompletion::Status(status),
            _ = sleep_until(deadline) => FirstCompletion::TimedOut,
        };

        let (status, stdout, stderr) = match first {
            FirstCompletion::Stdout(stdout) => {
                let stdout = match flatten_stdout(stdout) {
                    Ok(stdout) => stdout,
                    Err(error) => {
                        let cleanup = cleanup_inventory_child(&mut child, stderr_task).await;
                        return compose_process_result(Err(error), cleanup);
                    }
                };
                let status = match timeout_at(deadline, child.wait()).await {
                    Ok(Ok(status)) => status,
                    Ok(Err(error)) => {
                        let cleanup = cleanup_inventory_child(&mut child, stderr_task).await;
                        return compose_process_result(Err(InventoryError::Io(error)), cleanup);
                    }
                    Err(_) => {
                        let cleanup = cleanup_inventory_child(&mut child, stderr_task).await;
                        return compose_process_result(
                            Err(InventoryError::ProcessTimedOut),
                            cleanup,
                        );
                    }
                };
                let stderr = finish_stderr(stderr_task).await?;
                (status, stdout, stderr)
            }
            FirstCompletion::Status(status) => {
                let status = match status {
                    Ok(status) => status,
                    Err(error) => {
                        let cleanup = cleanup_inventory_child(&mut child, stderr_task).await;
                        stdout_task.abort();
                        return compose_process_result(Err(InventoryError::Io(error)), cleanup);
                    }
                };
                let stdout_result = finish_stdout(&mut stdout_task).await;
                let stderr_result = finish_stderr(stderr_task).await;
                let stderr = match stderr_result {
                    Ok(stderr) => stderr,
                    Err(cleanup) => {
                        return match stdout_result {
                            Ok(_) => Err(cleanup),
                            Err(operation) => compose_process_result::<InventorySnapshot>(
                                Err(operation),
                                Err(cleanup),
                            ),
                        };
                    }
                };
                (status, stdout_result?, stderr)
            }
            FirstCompletion::TimedOut => {
                let cleanup = cleanup_inventory_child(&mut child, stderr_task).await;
                stdout_task.abort();
                return compose_process_result(Err(InventoryError::ProcessTimedOut), cleanup);
            }
        };
        if !status.success() {
            return Err(classify_stderr(&stderr).into());
        }

        let observed_at_unix_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| InventoryError::InvalidSystemTime)?
            .as_millis()
            .try_into()
            .map_err(|_| InventoryError::InvalidSystemTime)?;
        parse_inventory(&stdout, observed_at_unix_ms, &profile.node)
    }
}

impl InventorySnapshot {
    pub fn new(observed_at_unix_ms: u64, stale: bool, mut vms: Vec<VmInventoryItem>) -> Self {
        vms.retain(|item| !item.template);
        vms.sort_by_key(|item| item.vmid);
        Self {
            observed_at_unix_ms,
            stale,
            vms,
        }
    }

    pub fn select(&self, selector: &str) -> Result<&VmInventoryItem, InventorySelectionError> {
        if selector.bytes().all(|byte| byte.is_ascii_digit()) {
            return self
                .vms
                .iter()
                .find(|item| item.vmid.to_string() == selector)
                .ok_or(InventorySelectionError::NotFound);
        }

        let mut matches = self
            .vms
            .iter()
            .filter(|item| item.name.eq_ignore_ascii_case(selector));
        let selected = matches.next().ok_or(InventorySelectionError::NotFound)?;
        if matches.next().is_some() {
            Err(InventorySelectionError::Ambiguous)
        } else {
            Ok(selected)
        }
    }
}

pub(crate) fn normalize_inventory_snapshot(
    mut snapshot: InventorySnapshot,
    stale: bool,
) -> Result<InventorySnapshot, InventoryError> {
    if snapshot
        .vms
        .iter()
        .any(|item| item.name.is_empty() || item.name.chars().any(char::is_control))
    {
        return Err(InventoryError::MalformedInventory);
    }
    snapshot.vms.sort_by_key(|item| item.vmid);
    if snapshot
        .vms
        .windows(2)
        .any(|items| items[0].vmid == items[1].vmid)
    {
        return Err(InventoryError::MalformedInventory);
    }
    snapshot.vms.retain(|item| !item.template);
    snapshot.stale = stale;
    Ok(snapshot)
}

#[derive(Deserialize)]
struct RawVmInventoryItem {
    vmid: u32,
    name: String,
    node: Option<String>,
    status: VmStatus,
    #[serde(default, deserialize_with = "deserialize_template")]
    template: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawTemplate {
    Boolean(bool),
    Integer(u64),
}

fn deserialize_template<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    match RawTemplate::deserialize(deserializer)? {
        RawTemplate::Boolean(value) => Ok(value),
        RawTemplate::Integer(0) => Ok(false),
        RawTemplate::Integer(1) => Ok(true),
        RawTemplate::Integer(_) => Err(de::Error::custom("template must be 0, 1, or boolean")),
    }
}

fn parse_inventory(
    json: &[u8],
    observed_at_unix_ms: u64,
    endpoint_node: &NodeName,
) -> Result<InventorySnapshot, InventoryError> {
    let records: Vec<RawVmInventoryItem> =
        serde_json::from_slice(json).map_err(|_| InventoryError::MalformedInventory)?;
    let mut vms = Vec::with_capacity(records.len());
    for record in records {
        if record.name.is_empty() || record.name.chars().any(char::is_control) {
            return Err(InventoryError::MalformedInventory);
        }
        let vmid = VmId::new(record.vmid).map_err(|_| InventoryError::MalformedInventory)?;
        if record
            .node
            .as_deref()
            .is_some_and(|node| node != endpoint_node.as_str())
        {
            return Err(InventoryError::MalformedInventory);
        }
        vms.push(VmInventoryItem {
            vmid,
            name: record.name,
            node: endpoint_node.clone(),
            status: record.status,
            template: record.template,
        });
    }
    vms.sort_by_key(|item| item.vmid);
    if vms.windows(2).any(|items| items[0].vmid == items[1].vmid) {
        return Err(InventoryError::MalformedInventory);
    }
    Ok(InventorySnapshot::new(observed_at_unix_ms, false, vms))
}

async fn read_capped<R>(mut reader: R, limit: usize) -> Result<Vec<u8>, InventoryError>
where
    R: AsyncRead + Unpin,
{
    let mut captured = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(captured);
        }
        if read > limit.saturating_sub(captured.len()) {
            return Err(InventoryError::StdoutTooLarge);
        }
        captured.extend_from_slice(&buffer[..read]);
    }
}

fn flatten_stdout(
    result: Result<Result<Vec<u8>, InventoryError>, tokio::task::JoinError>,
) -> Result<Vec<u8>, InventoryError> {
    result.unwrap_or(Err(InventoryError::OwnedChildCleanupFailed {
        operation_failed: true,
    }))
}

async fn finish_stdout(
    task: &mut JoinHandle<Result<Vec<u8>, InventoryError>>,
) -> Result<Vec<u8>, InventoryError> {
    match timeout(PIPE_DRAIN_TIMEOUT, &mut *task).await {
        Ok(result) => flatten_stdout(result),
        Err(_) => {
            task.abort();
            Err(InventoryError::OwnedChildCleanupFailed {
                operation_failed: false,
            })
        }
    }
}

async fn finish_stderr(
    mut task: Option<JoinHandle<io::Result<Vec<u8>>>>,
) -> Result<Vec<u8>, InventoryError> {
    match task {
        Some(ref mut task) => match timeout(PIPE_DRAIN_TIMEOUT, &mut *task).await {
            Ok(Ok(Ok(stderr))) => Ok(stderr),
            Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => {
                task.abort();
                Err(InventoryError::OwnedChildCleanupFailed {
                    operation_failed: false,
                })
            }
        },
        None => Ok(Vec::new()),
    }
}

async fn cleanup_inventory_child(
    child: &mut Child,
    stderr_task: Option<JoinHandle<io::Result<Vec<u8>>>>,
) -> Result<(), InventoryError> {
    let kill_failed = child.start_kill().is_err();
    let reap_failed = !matches!(timeout(REAP_TIMEOUT, child.wait()).await, Ok(Ok(_)));
    let drain_failed = finish_stderr(stderr_task).await.is_err();
    if kill_failed || reap_failed || drain_failed {
        Err(InventoryError::OwnedChildCleanupFailed {
            operation_failed: false,
        })
    } else {
        Ok(())
    }
}

fn compose_process_result<T>(
    operation: Result<T, InventoryError>,
    cleanup: Result<(), InventoryError>,
) -> Result<T, InventoryError> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(_), Err(_)) => Err(InventoryError::OwnedChildCleanupFailed {
            operation_failed: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{tempdir, TempDir};
    use tokio::time::{sleep, timeout};

    use super::{
        compose_process_result, parse_inventory, InventoryClient, InventoryError, VmStatus,
    };
    use crate::{
        model::{NodeName, PveProfile, SshTarget},
        runtime::RuntimeDir,
        ssh::{master::capture_bounded, SshCommandFactory, SshMaster},
    };

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
        timeout(Duration::from_secs(2), async {
            while !path.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake SSH did not create its state file");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_uses_owned_master_socket_and_returns_sorted_openable_inventory() {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        let mut master = SshMaster::start(
            SshCommandFactory::new_for_test(
                executable.clone(),
                runtime.control_socket().to_owned(),
            ),
            fixture_profile(),
        )
        .await
        .unwrap();
        wait_for(&runtime.control_socket().with_extension("state")).await;

        let snapshot = InventoryClient::fetch(
            &SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned()),
            &fixture_profile(),
        )
        .await
        .unwrap();
        master.close().await.unwrap();

        assert!(snapshot.observed_at_unix_ms > 0);
        assert!(!snapshot.stale);
        assert_eq!(
            snapshot
                .vms
                .iter()
                .map(|item| (item.vmid.get(), item.status))
                .collect::<Vec<_>>(),
            vec![(107, VmStatus::Running), (205, VmStatus::Stopped)]
        );
        assert!(snapshot.vms.iter().all(|item| !item.template));

        let argv = fs::read_to_string(runtime.control_socket().with_extension("argv")).unwrap();
        assert!(argv.contains("pvesh get /nodes/pve2/qemu --output-format json"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn optional_live_node_must_match_the_validated_endpoint_node() {
        for (marker, should_succeed) in [("matching_node", true), ("mismatched_node", false)] {
            let runtime = RuntimeDir::create().unwrap();
            let (_fixture_directory, executable) = fake_ssh();
            fs::write(
                runtime.control_socket().with_extension(marker),
                b"synthetic fixture control\n",
            )
            .unwrap();
            let factory =
                SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

            let result = InventoryClient::fetch(&factory, &fixture_profile()).await;

            if should_succeed {
                let snapshot = result.unwrap();
                assert_eq!(snapshot.vms[0].node.as_str(), "pve2");
            } else {
                assert!(matches!(result, Err(InventoryError::MalformedInventory)));
            }
        }
    }

    #[test]
    fn parser_rejects_missing_or_invalid_vmids_and_statuses() {
        for json in [
            br#"[{"name":"missing-vmid","node":"pve2","status":"running"}]"#.as_slice(),
            br#"[{"vmid":99,"name":"invalid-vmid","node":"pve2","status":"running"}]"#.as_slice(),
            br#"[{"vmid":107,"name":"missing-status","node":"pve2"}]"#.as_slice(),
            br#"[{"vmid":107,"name":"unknown-status","node":"pve2","status":"paused"}]"#.as_slice(),
        ] {
            assert!(matches!(
                parse_inventory(json, 1, &NodeName::parse("pve2").unwrap()),
                Err(InventoryError::MalformedInventory)
            ));
        }
    }

    #[tokio::test]
    async fn stderr_capture_retains_at_most_the_public_error_bound() {
        let input = vec![b'x'; 100_000];
        let captured = capture_bounded(input.as_slice(), 65_536).await.unwrap();

        assert_eq!(captured.len(), 65_536);
        assert!(captured.iter().all(|byte| *byte == b'x'));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_rejects_stdout_above_four_mib_before_json_parsing() {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("oversized"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

        let result = InventoryClient::fetch(&factory, &fixture_profile()).await;

        assert!(matches!(result, Err(InventoryError::StdoutTooLarge)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_accepts_exactly_four_mib_and_rejects_four_mib_plus_one() {
        for (marker, should_succeed) in [("exact_limit", true), ("over_limit", false)] {
            let runtime = RuntimeDir::create().unwrap();
            let (_fixture_directory, executable) = fake_ssh();
            fs::write(
                runtime.control_socket().with_extension(marker),
                b"synthetic fixture control\n",
            )
            .unwrap();
            let factory =
                SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

            let result = InventoryClient::fetch(&factory, &fixture_profile()).await;

            if should_succeed {
                assert_eq!(result.unwrap().vms[0].vmid.get(), 107);
            } else {
                assert!(matches!(result, Err(InventoryError::StdoutTooLarge)));
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_process_stderr_above_capture_limit_is_drained_and_reaped() {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("large_stderr"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

        let result = timeout(
            Duration::from_secs(4),
            InventoryClient::fetch(&factory, &fixture_profile()),
        )
        .await;

        assert_eq!(result.unwrap().unwrap().vms[0].vmid.get(), 107);
        assert!(!runtime
            .control_socket()
            .with_extension("inventory_running")
            .exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hanging_inventory_child_is_bounded_killed_and_reaped() {
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime.control_socket().with_extension("hang_inventory"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

        let result = timeout(
            Duration::from_secs(4),
            InventoryClient::fetch(&factory, &fixture_profile()),
        )
        .await;

        assert!(
            result.is_ok(),
            "inventory exceeded its owned-child deadline"
        );
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn inventory_error_composition_preserves_operation_and_cleanup_failure() {
        let result: Result<(), InventoryError> = compose_process_result(
            Err(InventoryError::StdoutTooLarge),
            Err(InventoryError::OwnedChildCleanupFailed {
                operation_failed: false,
            }),
        );

        assert!(matches!(
            result,
            Err(InventoryError::OwnedChildCleanupFailed {
                operation_failed: true,
            })
        ));
    }
}
