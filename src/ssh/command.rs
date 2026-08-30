use std::{
    convert::Infallible,
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};

use secrecy::{ExposeSecret, SecretString};

use crate::model::{PveProfile, VmId};

use super::ProxyTicket;

const SYSTEM_SSH: &str = "/usr/bin/ssh";
const COMMON_OPTIONS: &[&str] = &[
    "BatchMode=yes",
    "ConnectTimeout=12",
    "ServerAliveInterval=15",
    "ServerAliveCountMax=3",
    "StrictHostKeyChecking=yes",
    "PasswordAuthentication=no",
    "KbdInteractiveAuthentication=no",
];

/// An immutable, shell-free process contract for a single OpenSSH invocation.
///
/// This type intentionally does not implement `Debug` or serialization because
/// its environment can contain a process-local secret.
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    env: Vec<(OsString, SecretString)>,
    pub capture_stderr: bool,
}

impl CommandSpec {
    pub fn environment_variable_count(&self) -> usize {
        self.env.len()
    }

    /// Builds an OpenSSH process directly from separate program, argument, and
    /// environment values. It removes any inherited proxy ticket before
    /// applying this spec's approved environment and never invokes a command
    /// interpreter. The standard process builder necessarily owns an internal
    /// OS-string copy of explicit environment values until spawn or drop.
    ///
    /// Task 4 consumes this crate-private execution boundary when it owns the
    /// child lifecycle.
    #[allow(dead_code)]
    pub(crate) fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command.env_remove("LC_PVE_TICKET");
        for (key, value) in &self.env {
            command.env(key, value.expose_secret());
        }
        if self.capture_stderr {
            command.stderr(Stdio::piped());
        }
        command
    }
}

/// Builds the only OpenSSH command shapes the application may execute.
pub struct SshCommandFactory {
    executable: PathBuf,
    control_socket: PathBuf,
}

impl SshCommandFactory {
    /// Creates a production factory pinned to the system OpenSSH executable.
    pub fn new(control_socket: PathBuf) -> Self {
        Self {
            executable: PathBuf::from(SYSTEM_SSH),
            control_socket,
        }
    }

    pub fn master(&self, profile: &PveProfile) -> Result<CommandSpec, Infallible> {
        let mut spec = self.base_spec();
        spec.args.extend([
            OsString::from("-M"),
            OsString::from("-N"),
            OsString::from("-o"),
            OsString::from("ControlPersist=no"),
        ]);
        self.with_control_socket(&mut spec);
        self.with_target(&mut spec, profile);
        Ok(spec)
    }

    pub fn check(&self, profile: &PveProfile) -> Result<CommandSpec, Infallible> {
        self.control_operation(profile, "check")
    }

    pub fn exit(&self, profile: &PveProfile) -> Result<CommandSpec, Infallible> {
        self.control_operation(profile, "exit")
    }

    pub fn inventory(&self, profile: &PveProfile) -> Result<CommandSpec, Infallible> {
        let mut spec = self.child_spec(profile);
        spec.args.push(OsString::from(format!(
            "pvesh get /nodes/{}/qemu --output-format json",
            profile.node.as_str()
        )));
        Ok(spec)
    }

    pub fn proxy(
        &self,
        profile: &PveProfile,
        vmid: VmId,
        ticket: &ProxyTicket,
    ) -> Result<CommandSpec, Infallible> {
        let mut spec = self.base_spec();
        spec.args.extend([
            OsString::from("-o"),
            OsString::from("SendEnv=LC_PVE_TICKET"),
        ]);
        self.with_control_socket(&mut spec);
        self.with_target(&mut spec, profile);
        spec.args.push(OsString::from(format!(
            "exec /usr/sbin/qm vncproxy {}",
            vmid.get()
        )));
        spec.env.push((
            OsString::from("LC_PVE_TICKET"),
            SecretString::from(ticket.expose_for_auth()),
        ));
        Ok(spec)
    }

    fn base_spec(&self) -> CommandSpec {
        let mut args = Vec::with_capacity(COMMON_OPTIONS.len() * 2 + 8);
        for option in COMMON_OPTIONS {
            args.push(OsString::from("-o"));
            args.push(OsString::from(option));
        }
        CommandSpec {
            program: self.executable.clone(),
            args,
            env: Vec::new(),
            capture_stderr: true,
        }
    }

    fn child_spec(&self, profile: &PveProfile) -> CommandSpec {
        let mut spec = self.base_spec();
        self.with_control_socket(&mut spec);
        self.with_target(&mut spec, profile);
        spec
    }

    fn control_operation(
        &self,
        profile: &PveProfile,
        operation: &'static str,
    ) -> Result<CommandSpec, Infallible> {
        let mut spec = self.base_spec();
        self.with_control_socket(&mut spec);
        spec.args
            .extend([OsString::from("-O"), OsString::from(operation)]);
        self.with_target(&mut spec, profile);
        Ok(spec)
    }

    fn with_control_socket(&self, spec: &mut CommandSpec) {
        spec.args.push(OsString::from("-S"));
        spec.args.push(self.control_socket.clone().into_os_string());
    }

    fn with_target(&self, spec: &mut CommandSpec, profile: &PveProfile) {
        spec.args.push(OsString::from(profile.ssh_target.as_str()));
    }
}

#[cfg(test)]
impl SshCommandFactory {
    pub(super) fn new_for_test(executable: PathBuf, control_socket: PathBuf) -> Self {
        Self {
            executable,
            control_socket,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::{NodeName, SshTarget};

    const SOCKET: &str = "/tmp/roc-test/c";

    fn fixture_profile() -> PveProfile {
        PveProfile {
            name: "Test Proxmox".to_owned(),
            ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
            node: NodeName::parse("pve2").unwrap(),
        }
    }

    #[test]
    fn command_spec_materializes_a_direct_ssh_process_for_crate_execution() {
        let spec = SshCommandFactory::new(PathBuf::from(SOCKET))
            .inventory(&fixture_profile())
            .unwrap();
        let command = spec.to_command();

        assert_eq!(command.get_program(), PathBuf::from("/usr/bin/ssh"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            spec.args
                .iter()
                .map(OsString::as_os_str)
                .collect::<Vec<_>>()
        );
        let environments = command.get_envs().collect::<Vec<_>>();
        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].0, "LC_PVE_TICKET");
        assert!(environments[0].1.is_none());
    }

    #[test]
    fn proxy_replaces_the_inherited_ticket_with_exactly_one_explicit_value() {
        let ticket = ProxyTicket::generate();
        let spec = SshCommandFactory::new(PathBuf::from(SOCKET))
            .proxy(&fixture_profile(), VmId::new(107).unwrap(), &ticket)
            .unwrap();
        let command = spec.to_command();
        let ticket_entries = command
            .get_envs()
            .filter(|(key, _)| *key == "LC_PVE_TICKET")
            .collect::<Vec<_>>();

        assert_eq!(ticket_entries.len(), 1);
        assert!(ticket_entries[0].1.is_some());
        assert_eq!(spec.environment_variable_count(), 1);
    }

    #[test]
    fn test_owned_executable_injection_does_not_change_production_construction() {
        let injected = PathBuf::from("/private/test-fixtures/fake-ssh");
        let test_spec = SshCommandFactory::new_for_test(injected.clone(), PathBuf::from(SOCKET))
            .inventory(&fixture_profile())
            .unwrap();
        let production_spec = SshCommandFactory::new(PathBuf::from(SOCKET))
            .inventory(&fixture_profile())
            .unwrap();

        assert_eq!(test_spec.program, injected);
        assert_eq!(production_spec.program, PathBuf::from("/usr/bin/ssh"));
    }
}
