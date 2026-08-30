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
    } else if stderr.contains("Permission denied") {
        SshFailureKind::Authentication
    } else if stderr.contains("Operation timed out") || stderr.contains("Connection timed out") {
        SshFailureKind::Timeout
    } else {
        SshFailureKind::Ssh
    };
    SshFailure { kind }
}
