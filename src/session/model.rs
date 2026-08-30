use thiserror::Error;
use uuid::Uuid;

use crate::model::VmId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Opening,
    StartingProxy,
    NegotiatingRfb,
    Ready,
    Disconnecting,
    Disconnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub profile_name: String,
    pub vmid: VmId,
    pub phase: SessionPhase,
}

impl SessionSnapshot {
    pub fn opening(session_id: SessionId, profile_name: String, vmid: VmId) -> Self {
        Self {
            session_id,
            profile_name,
            vmid,
            phase: SessionPhase::Opening,
        }
    }

    pub fn transition_to(&mut self, next: SessionPhase) -> Result<(), SessionTransitionError> {
        let allowed = matches!(
            (self.phase, next),
            (SessionPhase::Opening, SessionPhase::StartingProxy)
                | (SessionPhase::StartingProxy, SessionPhase::NegotiatingRfb)
                | (SessionPhase::NegotiatingRfb, SessionPhase::Ready)
                | (
                    SessionPhase::Opening
                        | SessionPhase::StartingProxy
                        | SessionPhase::NegotiatingRfb
                        | SessionPhase::Ready,
                    SessionPhase::Disconnecting
                )
                | (SessionPhase::Disconnecting, SessionPhase::Disconnected)
        );
        if !allowed {
            return Err(SessionTransitionError {
                from: self.phase,
                to: next,
            });
        }
        self.phase = next;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("invalid session transition from {from:?} to {to:?}")]
pub struct SessionTransitionError {
    pub from: SessionPhase,
    pub to: SessionPhase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicErrorKind {
    Config,
    HostKeyUnknown,
    HostKeyChanged,
    SshUnavailable,
    SshAuthentication,
    Inventory,
    VmNotFound,
    VmNotRunning,
    Capacity,
    Proxy,
    RfbProtocol,
    RfbSecurity,
    RfbLimit,
    Decoder,
    ViewerFallback,
    Cleanup,
    Queue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicError {
    kind: PublicErrorKind,
    session_id: Option<SessionId>,
    vmid: Option<VmId>,
    cleanup_failed: bool,
}

impl PublicError {
    pub fn new(kind: PublicErrorKind) -> Self {
        Self {
            kind,
            session_id: None,
            vmid: None,
            cleanup_failed: false,
        }
    }

    pub fn kind(self) -> PublicErrorKind {
        self.kind
    }

    pub fn session_id(self) -> Option<SessionId> {
        self.session_id
    }

    pub fn vmid(self) -> Option<VmId> {
        self.vmid
    }

    pub fn has_cleanup_failure(self) -> bool {
        self.cleanup_failed
    }

    pub(crate) fn for_session(mut self, session_id: SessionId, vmid: VmId) -> Self {
        self.session_id = Some(session_id);
        self.vmid = Some(vmid);
        self
    }

    pub(crate) fn with_cleanup_failure(mut self) -> Self {
        self.cleanup_failed = true;
        self
    }
}

impl std::fmt::Display for PublicError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self.kind {
            PublicErrorKind::Config => "configuration failed",
            PublicErrorKind::HostKeyUnknown => "SSH host key is not trusted",
            PublicErrorKind::HostKeyChanged => "SSH host key changed",
            PublicErrorKind::SshUnavailable => "SSH is unavailable",
            PublicErrorKind::SshAuthentication => "SSH authentication failed",
            PublicErrorKind::Inventory => "inventory refresh failed",
            PublicErrorKind::VmNotFound => "VM was not found",
            PublicErrorKind::VmNotRunning => "VM is not running",
            PublicErrorKind::Capacity => "native console capacity is full",
            PublicErrorKind::Proxy => "console proxy failed",
            PublicErrorKind::RfbProtocol => "RFB protocol failed",
            PublicErrorKind::RfbSecurity => "RFB security negotiation failed",
            PublicErrorKind::RfbLimit => "RFB protocol limit was exceeded",
            PublicErrorKind::Decoder => "framebuffer decoder failed",
            PublicErrorKind::ViewerFallback => "fallback viewer failed",
            PublicErrorKind::Cleanup => "owned resource cleanup failed",
            PublicErrorKind::Queue => "bounded session queue is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PublicError {}
