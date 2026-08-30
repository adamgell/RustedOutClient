use std::{error::Error, fmt};

const MAX_CAPTURED_STDERR_BYTES: usize = 65_536;

/// A non-sensitive, user-actionable SSH failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SshFailureKind {
    HostKeyUnknown,
    HostKeyChanged,
    Authentication,
    Timeout,
    Ssh,
}

/// A redacted SSH failure. Raw stderr, hostnames, fingerprints, and process
/// environment values are intentionally discarded before this value is made.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SshFailure {
    kind: SshFailureKind,
}

impl SshFailure {
    pub fn kind(&self) -> SshFailureKind {
        self.kind
    }
}

impl fmt::Display for SshFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            SshFailureKind::HostKeyUnknown => {
                "SSH host key is not trusted. Establish trust with OpenSSH outside RustedOutClient."
            }
            SshFailureKind::HostKeyChanged => "SSH host key changed. Refusing connection.",
            SshFailureKind::Authentication => "SSH authentication failed.",
            SshFailureKind::Timeout => "SSH connection timed out.",
            SshFailureKind::Ssh => "SSH connection failed.",
        };
        formatter.write_str(message)
    }
}

impl Error for SshFailure {}

/// Bounds stderr before classifying it and returns a redacted failure value.
pub fn classify_stderr(stderr: &[u8]) -> SshFailure {
    let bounded = &stderr[..stderr.len().min(MAX_CAPTURED_STDERR_BYTES)];
    let stderr = String::from_utf8_lossy(bounded);
    let kind = if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || stderr.contains("Offending ") && stderr.contains(" key in ")
    {
        SshFailureKind::HostKeyChanged
    } else if stderr.contains("Host key verification failed")
        || stderr.contains("is not known") && stderr.contains("strict checking")
    {
        SshFailureKind::HostKeyUnknown
    } else if stderr.lines().any(is_ssh_authentication_line) {
        SshFailureKind::Authentication
    } else if stderr.lines().any(is_ssh_timeout_line) {
        SshFailureKind::Timeout
    } else {
        SshFailureKind::Ssh
    };
    SshFailure { kind }
}

fn is_ssh_authentication_line(line: &str) -> bool {
    let line = line.trim_end_matches('\r');
    let Some((target, methods)) = line.split_once(": Permission denied (") else {
        return false;
    };
    let Some(methods) = methods.strip_suffix(").") else {
        return false;
    };

    let Some((user, host)) = target.split_once('@') else {
        return false;
    };

    is_graphic_component(user)
        && is_graphic_component(host)
        && !host.contains('@')
        && methods
            .split(',')
            .all(|method| !method.is_empty() && method.bytes().all(is_auth_method_byte))
}

fn is_ssh_timeout_line(line: &str) -> bool {
    let Some(details) = line
        .trim_end_matches('\r')
        .strip_prefix("ssh: connect to host ")
    else {
        return false;
    };
    let Some((host, port_and_message)) = details.rsplit_once(" port ") else {
        return false;
    };
    let Some((port, message)) = port_and_message.split_once(": ") else {
        return false;
    };

    let valid_port = !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0);

    is_graphic_component(host)
        && valid_port
        && matches!(message, "Operation timed out" | "Connection timed out")
}

fn is_graphic_component(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn is_auth_method_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-'
}
