use std::{
    convert::Infallible,
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};

use secrecy::{ExposeSecret, SecretString};

use crate::model::{PveProfile, VmId};

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
    pub env: Vec<(OsString, SecretString)>,
    pub capture_stderr: bool,
}

impl CommandSpec {
    /// Builds an OpenSSH process directly from separate program, argument, and
    /// environment values. It never invokes a command interpreter.
    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
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

    /// Creates a factory with a test-owned executable for integration tests.
    ///
    /// This is not wired to configuration, CLI flags, environment variables,
    /// PATH lookup, or user input. Production construction always uses
    /// `/usr/bin/ssh` through [`Self::new`].
    #[doc(hidden)]
    pub fn new_for_test(executable: PathBuf, control_socket: PathBuf) -> Self {
        Self {
            executable,
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

    pub fn proxy(&self, profile: &PveProfile, vmid: VmId) -> Result<CommandSpec, Infallible> {
        let mut spec = self.child_spec(profile);
        spec.args.push(OsString::from(format!(
            "exec /usr/sbin/qm vncproxy {}",
            vmid.get()
        )));
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
