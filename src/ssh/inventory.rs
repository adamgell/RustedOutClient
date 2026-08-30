use std::{io, process::Stdio, time::SystemTime};

#[cfg(test)]
use std::path::PathBuf;

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
const INVENTORY_PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const REAP_TIMEOUT: Duration = Duration::from_secs(1);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct InventoryPolicy {
    operation_timeout: Duration,
    reap_timeout: Duration,
    pipe_drain_timeout: Duration,
    #[cfg(test)]
    readiness: Option<TestReadiness>,
}

impl InventoryPolicy {
    fn production() -> Self {
        Self {
            operation_timeout: INVENTORY_PROCESS_TIMEOUT,
            reap_timeout: REAP_TIMEOUT,
            pipe_drain_timeout: PIPE_DRAIN_TIMEOUT,
            #[cfg(test)]
            readiness: None,
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TestReadiness {
    path: PathBuf,
    timeout: Duration,
}

#[cfg(test)]
#[derive(Clone)]
struct TestInventoryPolicy(InventoryPolicy);

#[cfg(test)]
impl TestInventoryPolicy {
    fn generous() -> Self {
        Self(InventoryPolicy {
            operation_timeout: Duration::from_secs(60),
            reap_timeout: Duration::from_secs(30),
            pipe_drain_timeout: Duration::from_secs(30),
            readiness: None,
        })
    }

    fn short_after_ready(path: PathBuf) -> Self {
        Self(InventoryPolicy {
            operation_timeout: Duration::from_secs(2),
            reap_timeout: Duration::from_secs(30),
            pipe_drain_timeout: Duration::from_secs(30),
            readiness: Some(TestReadiness {
                path,
                timeout: Duration::from_secs(30),
            }),
        })
    }
}

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

#[derive(Clone, Copy, Default)]
struct CleanupFaults {
    #[cfg(test)]
    fault: Option<CleanupFault>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupFault {
    Kill,
    Wait,
    Drain,
}

impl CleanupFaults {
    fn kill_fails(self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(CleanupFault::Kill)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn wait_fails(self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(CleanupFault::Wait)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn drain_fails(self) -> bool {
        #[cfg(test)]
        {
            self.fault == Some(CleanupFault::Drain)
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

#[cfg(test)]
impl From<CleanupFault> for CleanupFaults {
    fn from(fault: CleanupFault) -> Self {
        Self { fault: Some(fault) }
    }
}

impl InventoryClient {
    pub async fn fetch(
        factory: &SshCommandFactory,
        profile: &PveProfile,
    ) -> Result<InventorySnapshot, InventoryError> {
        Self::fetch_inner(
            factory,
            profile,
            CleanupFaults::default(),
            &InventoryPolicy::production(),
        )
        .await
    }

    #[cfg(test)]
    async fn fetch_with_test_policy(
        factory: &SshCommandFactory,
        profile: &PveProfile,
        policy: TestInventoryPolicy,
    ) -> Result<InventorySnapshot, InventoryError> {
        Self::fetch_inner(factory, profile, CleanupFaults::default(), &policy.0).await
    }

    #[cfg(test)]
    async fn fetch_with_cleanup_fault(
        factory: &SshCommandFactory,
        profile: &PveProfile,
        fault: CleanupFault,
        policy: TestInventoryPolicy,
    ) -> Result<InventorySnapshot, InventoryError> {
        Self::fetch_inner(factory, profile, fault.into(), &policy.0).await
    }

    async fn fetch_inner(
        factory: &SshCommandFactory,
        profile: &PveProfile,
        cleanup_faults: CleanupFaults,
        policy: &InventoryPolicy,
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
        if !wait_for_readiness(policy).await {
            let cleanup =
                cleanup_inventory_child(&mut child, stderr_task, cleanup_faults, policy).await;
            stdout_task.abort();
            return compose_process_result(Err(InventoryError::ProcessTimedOut), cleanup);
        }
        let deadline = Instant::now() + policy.operation_timeout;

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
                        let cleanup = cleanup_inventory_child(
                            &mut child,
                            stderr_task,
                            cleanup_faults,
                            policy,
                        )
                        .await;
                        return compose_process_result(Err(error), cleanup);
                    }
                };
                let status = match timeout_at(deadline, child.wait()).await {
                    Ok(Ok(status)) => status,
                    Ok(Err(error)) => {
                        let cleanup = cleanup_inventory_child(
                            &mut child,
                            stderr_task,
                            cleanup_faults,
                            policy,
                        )
                        .await;
                        return compose_process_result(Err(InventoryError::Io(error)), cleanup);
                    }
                    Err(_) => {
                        let cleanup = cleanup_inventory_child(
                            &mut child,
                            stderr_task,
                            cleanup_faults,
                            policy,
                        )
                        .await;
                        return compose_process_result(
                            Err(InventoryError::ProcessTimedOut),
                            cleanup,
                        );
                    }
                };
                let stderr = finish_stderr(stderr_task, policy.pipe_drain_timeout).await?;
                (status, stdout, stderr)
            }
            FirstCompletion::Status(status) => {
                let status = match status {
                    Ok(status) => status,
                    Err(error) => {
                        let cleanup = cleanup_inventory_child(
                            &mut child,
                            stderr_task,
                            cleanup_faults,
                            policy,
                        )
                        .await;
                        stdout_task.abort();
                        return compose_process_result(Err(InventoryError::Io(error)), cleanup);
                    }
                };
                let stdout_result =
                    finish_stdout(&mut stdout_task, policy.pipe_drain_timeout).await;
                let stderr_result = finish_stderr(stderr_task, policy.pipe_drain_timeout).await;
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
                let cleanup =
                    cleanup_inventory_child(&mut child, stderr_task, cleanup_faults, policy).await;
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
    drain_timeout: Duration,
) -> Result<Vec<u8>, InventoryError> {
    match timeout(drain_timeout, &mut *task).await {
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
    drain_timeout: Duration,
) -> Result<Vec<u8>, InventoryError> {
    match task {
        Some(ref mut task) => match timeout(drain_timeout, &mut *task).await {
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
    cleanup_faults: CleanupFaults,
    policy: &InventoryPolicy,
) -> Result<(), InventoryError> {
    let kill_failed = child.start_kill().is_err() || cleanup_faults.kill_fails();
    let reap_failed = !matches!(timeout(policy.reap_timeout, child.wait()).await, Ok(Ok(_)))
        || cleanup_faults.wait_fails();
    let drain_failed = finish_stderr(stderr_task, policy.pipe_drain_timeout)
        .await
        .is_err()
        || cleanup_faults.drain_fails();
    if kill_failed || reap_failed || drain_failed {
        Err(InventoryError::OwnedChildCleanupFailed {
            operation_failed: false,
        })
    } else {
        Ok(())
    }
}

async fn wait_for_readiness(_policy: &InventoryPolicy) -> bool {
    #[cfg(test)]
    if let Some(readiness) = &_policy.readiness {
        return timeout(readiness.timeout, async {
            loop {
                if tokio::fs::metadata(&readiness.path).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
    }

    true
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
        process::{Command, Stdio},
        time::Duration,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{tempdir, TempDir};
    use tokio::time::{sleep, timeout};

    use super::{
        parse_inventory, CleanupFault, InventoryClient, InventoryError, TestInventoryPolicy,
        VmStatus,
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
        timeout(Duration::from_secs(30), async {
            while !path.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake SSH did not create its state file");
    }

    fn helper_pid(path: &Path) -> u32 {
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    fn assert_exact_pid_is_gone(pid: u32) {
        let status = Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "synthetic helper PID {pid} is still alive"
        );
    }

    fn write_inventory_payload(socket: &Path, size: usize) {
        const PREFIX: &[u8] =
            br#"[{"vmid":107,"name":"LABZ1-CM01","status":"running","template":0,"padding":""#;
        const SUFFIX: &[u8] = br#""}]"#;
        assert!(size >= PREFIX.len() + SUFFIX.len());
        let mut payload = Vec::with_capacity(size);
        payload.extend_from_slice(PREFIX);
        payload.resize(size - SUFFIX.len(), b'x');
        payload.extend_from_slice(SUFFIX);
        assert_eq!(payload.len(), size);
        fs::write(socket.with_extension("inventory_payload"), payload).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_uses_owned_master_socket_and_returns_sorted_openable_inventory() {
        let _process_guard = crate::ssh::process_test_guard().await;
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

        let snapshot = InventoryClient::fetch_with_test_policy(
            &SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned()),
            &fixture_profile(),
            TestInventoryPolicy::generous(),
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
        let _process_guard = crate::ssh::process_test_guard().await;
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

            let result = InventoryClient::fetch_with_test_policy(
                &factory,
                &fixture_profile(),
                TestInventoryPolicy::generous(),
            )
            .await;

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
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        write_inventory_payload(runtime.control_socket(), 4 * 1024 * 1024 + 8 * 1024);
        let factory =
            SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

        let result = InventoryClient::fetch_with_test_policy(
            &factory,
            &fixture_profile(),
            TestInventoryPolicy::generous(),
        )
        .await;

        assert!(matches!(result, Err(InventoryError::StdoutTooLarge)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_accepts_exactly_four_mib_and_rejects_four_mib_plus_one() {
        let _process_guard = crate::ssh::process_test_guard().await;
        for (size, should_succeed) in [(4 * 1024 * 1024, true), (4 * 1024 * 1024 + 1, false)] {
            let runtime = RuntimeDir::create().unwrap();
            let (_fixture_directory, executable) = fake_ssh();
            write_inventory_payload(runtime.control_socket(), size);
            let factory =
                SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

            let result = InventoryClient::fetch_with_test_policy(
                &factory,
                &fixture_profile(),
                TestInventoryPolicy::generous(),
            )
            .await;

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
        let _process_guard = crate::ssh::process_test_guard().await;
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
            Duration::from_secs(30),
            InventoryClient::fetch_with_test_policy(
                &factory,
                &fixture_profile(),
                TestInventoryPolicy::generous(),
            ),
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
        let _process_guard = crate::ssh::process_test_guard().await;
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
            Duration::from_secs(30),
            InventoryClient::fetch_with_test_policy(
                &factory,
                &fixture_profile(),
                TestInventoryPolicy::short_after_ready(
                    runtime.control_socket().with_extension("inventory.pid"),
                ),
            ),
        )
        .await;

        assert!(
            result.is_ok(),
            "inventory exceeded its owned-child deadline"
        );
        assert!(result.unwrap().is_err());
        let inventory_pid = helper_pid(&runtime.control_socket().with_extension("inventory.pid"));
        assert_exact_pid_is_gone(inventory_pid);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_faults_run_inventory_kill_wait_and_drain_orchestration() {
        let _process_guard = crate::ssh::process_test_guard().await;
        for fault in [CleanupFault::Kill, CleanupFault::Wait, CleanupFault::Drain] {
            let runtime = RuntimeDir::create().unwrap();
            let (_fixture_directory, executable) = fake_ssh();
            fs::write(
                runtime.control_socket().with_extension("hang_inventory"),
                b"synthetic fixture control\n",
            )
            .unwrap();
            let factory =
                SshCommandFactory::new_for_test(executable, runtime.control_socket().to_owned());

            let result = InventoryClient::fetch_with_cleanup_fault(
                &factory,
                &fixture_profile(),
                fault,
                TestInventoryPolicy::short_after_ready(
                    runtime.control_socket().with_extension("inventory.pid"),
                ),
            )
            .await;

            assert!(matches!(
                result,
                Err(InventoryError::OwnedChildCleanupFailed {
                    operation_failed: true,
                })
            ));
            let pid = helper_pid(&runtime.control_socket().with_extension("inventory.pid"));
            assert_exact_pid_is_gone(pid);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn short_timeout_starts_after_exact_inventory_helper_readiness() {
        let _process_guard = crate::ssh::process_test_guard().await;
        let runtime = RuntimeDir::create().unwrap();
        let (_fixture_directory, executable) = fake_ssh();
        fs::write(
            runtime
                .control_socket()
                .with_extension("hold_before_inventory_ready"),
            b"synthetic fixture control\n",
        )
        .unwrap();
        let socket = runtime.control_socket().to_owned();
        let factory = SshCommandFactory::new_for_test(executable, socket.clone());
        let mut fetch = tokio::spawn(async move {
            InventoryClient::fetch_with_test_policy(
                &factory,
                &fixture_profile(),
                TestInventoryPolicy::short_after_ready(socket.with_extension("inventory.pid")),
            )
            .await
        });
        wait_for(&runtime.control_socket().with_extension("inventory.spawned")).await;

        assert!(
            timeout(Duration::from_secs(3), &mut fetch).await.is_err(),
            "operation timeout started before exact helper readiness"
        );
        fs::write(
            runtime
                .control_socket()
                .with_extension("allow_inventory_ready"),
            b"synthetic fixture control\n",
        )
        .unwrap();

        let snapshot = timeout(Duration::from_secs(30), fetch)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.vms[0].vmid.get(), 107);
        let pid = helper_pid(&runtime.control_socket().with_extension("inventory.pid"));
        assert_exact_pid_is_gone(pid);
    }
}
