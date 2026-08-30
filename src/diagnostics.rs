use std::time::Duration;

use serde::Serialize;

use crate::{
    connection::{DesktopSize, FbRect},
    model::{NodeName, VmId},
    session::{PublicError, PublicErrorKind, SessionPhase},
    vnc::{validate_framebuffer_layout, ProtocolLimits},
};

pub const UPSTREAM_BASE_SHA: &str = "999e00e3a3672efdbf8e8f307e7bd60875dee67e";
const MAX_DIAGNOSTIC_PHASES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DiagnosticPhase {
    Opening,
    StartingProxy,
    NegotiatingRfb,
    Ready,
    Disconnecting,
    Disconnected,
}

impl From<SessionPhase> for DiagnosticPhase {
    fn from(phase: SessionPhase) -> Self {
        match phase {
            SessionPhase::Opening => Self::Opening,
            SessionPhase::StartingProxy => Self::StartingProxy,
            SessionPhase::NegotiatingRfb => Self::NegotiatingRfb,
            SessionPhase::Ready => Self::Ready,
            SessionPhase::Disconnecting => Self::Disconnecting,
            SessionPhase::Disconnected => Self::Disconnected,
        }
    }
}

impl From<DiagnosticPhase> for SessionPhase {
    fn from(phase: DiagnosticPhase) -> Self {
        match phase {
            DiagnosticPhase::Opening => Self::Opening,
            DiagnosticPhase::StartingProxy => Self::StartingProxy,
            DiagnosticPhase::NegotiatingRfb => Self::NegotiatingRfb,
            DiagnosticPhase::Ready => Self::Ready,
            DiagnosticPhase::Disconnecting => Self::Disconnecting,
            DiagnosticPhase::Disconnected => Self::Disconnected,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PhaseTiming {
    phase: DiagnosticPhase,
    duration_ms: u64,
}

impl PhaseTiming {
    pub fn new(phase: SessionPhase, duration: Duration) -> Self {
        Self {
            phase: phase.into(),
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        }
    }

    pub fn phase(self) -> SessionPhase {
        self.phase.into()
    }

    pub fn duration_ms(self) -> u64 {
        self.duration_ms
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ChildExitStatus(i32);

impl ChildExitStatus {
    pub const fn new(status: i32) -> Self {
        Self(status)
    }

    pub const fn get(self) -> i32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticErrorCategory {
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

impl DiagnosticErrorCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::HostKeyUnknown => "host_key_unknown",
            Self::HostKeyChanged => "host_key_changed",
            Self::SshUnavailable => "ssh_unavailable",
            Self::SshAuthentication => "ssh_authentication",
            Self::Inventory => "inventory",
            Self::VmNotFound => "vm_not_found",
            Self::VmNotRunning => "vm_not_running",
            Self::Capacity => "capacity",
            Self::Proxy => "proxy",
            Self::RfbProtocol => "rfb_protocol",
            Self::RfbSecurity => "rfb_security",
            Self::RfbLimit => "rfb_limit",
            Self::Decoder => "decoder",
            Self::ViewerFallback => "viewer_fallback",
            Self::Cleanup => "cleanup",
            Self::Queue => "queue",
        }
    }
}

impl From<PublicErrorKind> for DiagnosticErrorCategory {
    fn from(kind: PublicErrorKind) -> Self {
        match kind {
            PublicErrorKind::Config => Self::Config,
            PublicErrorKind::HostKeyUnknown => Self::HostKeyUnknown,
            PublicErrorKind::HostKeyChanged => Self::HostKeyChanged,
            PublicErrorKind::SshUnavailable => Self::SshUnavailable,
            PublicErrorKind::SshAuthentication => Self::SshAuthentication,
            PublicErrorKind::Inventory => Self::Inventory,
            PublicErrorKind::VmNotFound => Self::VmNotFound,
            PublicErrorKind::VmNotRunning => Self::VmNotRunning,
            PublicErrorKind::Capacity => Self::Capacity,
            PublicErrorKind::Proxy => Self::Proxy,
            PublicErrorKind::RfbProtocol => Self::RfbProtocol,
            PublicErrorKind::RfbSecurity => Self::RfbSecurity,
            PublicErrorKind::RfbLimit => Self::RfbLimit,
            PublicErrorKind::Decoder => Self::Decoder,
            PublicErrorKind::ViewerFallback => Self::ViewerFallback,
            PublicErrorKind::Cleanup => Self::Cleanup,
            PublicErrorKind::Queue => Self::Queue,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticFailure {
    category: DiagnosticErrorCategory,
    cleanup_failed: bool,
}

impl DiagnosticFailure {
    pub fn from_public(error: PublicError) -> Self {
        Self {
            category: error.kind().into(),
            cleanup_failed: error.has_cleanup_failure(),
        }
    }

    pub const fn category(self) -> DiagnosticErrorCategory {
        self.category
    }

    pub const fn cleanup_failed(self) -> bool {
        self.cleanup_failed
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticRecord {
    app_version: &'static str,
    upstream_base_sha: &'static str,
    os: &'static str,
    architecture: &'static str,
    profile_display_name: String,
    node: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    vmid: Option<u32>,
    phases: Vec<PhaseTiming>,
    #[serde(skip_serializing_if = "Option::is_none")]
    child_exit_status: Option<ChildExitStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_category: Option<DiagnosticErrorCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_failed: Option<bool>,
}

impl DiagnosticRecord {
    pub fn new(
        profile_display_name: String,
        node: NodeName,
        vmid: Option<VmId>,
        phases: Vec<PhaseTiming>,
        child_exit_status: Option<ChildExitStatus>,
        failure: Option<DiagnosticFailure>,
    ) -> Self {
        Self {
            app_version: env!("CARGO_PKG_VERSION"),
            upstream_base_sha: UPSTREAM_BASE_SHA,
            os: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            profile_display_name,
            node: node.as_str().to_owned(),
            vmid: vmid.map(VmId::get),
            phases: phases.into_iter().take(MAX_DIAGNOSTIC_PHASES).collect(),
            child_exit_status,
            error_category: failure.map(DiagnosticFailure::category),
            cleanup_failed: failure.map(DiagnosticFailure::cleanup_failed),
        }
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    pub fn to_text(&self) -> String {
        let mut lines = vec![
            format!("RustedOutClient {}", self.app_version),
            format!("Upstream base: {}", self.upstream_base_sha),
            format!("Platform: {}/{}", self.os, self.architecture),
            format!("Profile: {}", self.profile_display_name),
            format!("Node: {}", self.node),
        ];
        if let Some(vmid) = self.vmid {
            lines.push(format!("VMID: {vmid}"));
        }
        for timing in &self.phases {
            let phase: SessionPhase = timing.phase.into();
            lines.push(format!(
                "{}: {} ms",
                DiagnosticPhase::from(phase).as_str(),
                timing.duration_ms
            ));
        }
        if let Some(status) = self.child_exit_status {
            lines.push(format!("Child exit status: {}", status.get()));
        }
        if let Some(category) = self.error_category {
            lines.push(format!("Error category: {}", category.as_str()));
        }
        if let Some(cleanup_failed) = self.cleanup_failed {
            lines.push(format!("Cleanup failure: {cleanup_failed}"));
        }
        lines.join("\n")
    }
}

impl DiagnosticPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Opening => "opening",
            Self::StartingProxy => "starting_proxy",
            Self::NegotiatingRfb => "negotiating_rfb",
            Self::Ready => "ready",
            Self::Disconnecting => "disconnecting",
            Self::Disconnected => "disconnected",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeResult {
    Success,
    Timeout,
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
    Cleanup,
    Queue,
}

impl From<PublicErrorKind> for ProbeResult {
    fn from(kind: PublicErrorKind) -> Self {
        match kind {
            PublicErrorKind::Config => Self::Config,
            PublicErrorKind::HostKeyUnknown => Self::HostKeyUnknown,
            PublicErrorKind::HostKeyChanged => Self::HostKeyChanged,
            PublicErrorKind::SshUnavailable => Self::SshUnavailable,
            PublicErrorKind::SshAuthentication => Self::SshAuthentication,
            PublicErrorKind::Inventory => Self::Inventory,
            PublicErrorKind::VmNotFound => Self::VmNotFound,
            PublicErrorKind::VmNotRunning => Self::VmNotRunning,
            PublicErrorKind::Capacity => Self::Capacity,
            PublicErrorKind::Proxy => Self::Proxy,
            PublicErrorKind::RfbProtocol => Self::RfbProtocol,
            PublicErrorKind::RfbSecurity => Self::RfbSecurity,
            PublicErrorKind::RfbLimit => Self::RfbLimit,
            PublicErrorKind::Decoder => Self::Decoder,
            PublicErrorKind::ViewerFallback => Self::Proxy,
            PublicErrorKind::Cleanup => Self::Cleanup,
            PublicErrorKind::Queue => Self::Queue,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProbeReport {
    vmid: u32,
    first_frame_ms: u64,
    frame_width: u16,
    frame_height: u16,
    non_black_pixels: u64,
    result: ProbeResult,
}

impl ProbeReport {
    pub fn from_frame(
        vmid: VmId,
        elapsed: Duration,
        size: DesktopSize,
        rects: &[FbRect],
    ) -> Result<Self, PublicError> {
        let limits = ProtocolLimits::default();
        validate_framebuffer_layout(size.width, size.height, limits)
            .map_err(|_| PublicError::new(PublicErrorKind::RfbLimit))?;
        if rects.is_empty() || rects.len() > usize::from(limits.max_rectangles) {
            return Err(PublicError::new(PublicErrorKind::RfbLimit));
        }
        let mut non_black_pixels = 0_u64;
        for rect in rects {
            let right = rect
                .x
                .checked_add(rect.w)
                .ok_or_else(|| PublicError::new(PublicErrorKind::RfbLimit))?;
            let bottom = rect
                .y
                .checked_add(rect.h)
                .ok_or_else(|| PublicError::new(PublicErrorKind::RfbLimit))?;
            if rect.w == 0
                || rect.h == 0
                || right > u32::from(size.width)
                || bottom > u32::from(size.height)
            {
                return Err(PublicError::new(PublicErrorKind::RfbLimit));
            }
            let expected = u64::from(rect.w)
                .checked_mul(u64::from(rect.h))
                .and_then(|pixels| pixels.checked_mul(4))
                .and_then(|bytes| usize::try_from(bytes).ok())
                .ok_or_else(|| PublicError::new(PublicErrorKind::RfbLimit))?;
            if expected != rect.rgba.len()
                || expected > usize::try_from(limits.max_framebuffer_bytes).unwrap_or(usize::MAX)
            {
                return Err(PublicError::new(PublicErrorKind::RfbLimit));
            }
            let count = rect
                .rgba
                .chunks_exact(4)
                .filter(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)
                .count();
            non_black_pixels = non_black_pixels
                .checked_add(u64::try_from(count).unwrap_or(u64::MAX))
                .ok_or_else(|| PublicError::new(PublicErrorKind::RfbLimit))?;
        }
        Ok(Self {
            vmid: vmid.get(),
            first_frame_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            frame_width: size.width,
            frame_height: size.height,
            non_black_pixels,
            result: ProbeResult::Success,
        })
    }

    pub fn failure(vmid: Option<VmId>, result: ProbeResult) -> Self {
        Self {
            vmid: vmid.map_or(0, VmId::get),
            first_frame_ms: 0,
            frame_width: 0,
            frame_height: 0,
            non_black_pixels: 0,
            result,
        }
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    pub const fn result(&self) -> ProbeResult {
        self.result
    }

    pub fn to_text(&self) -> String {
        let result = serde_json::to_value(self.result)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "queue".to_owned());
        format!(
            "VMID: {}\nFirst frame: {} ms\nFrame: {}x{}\nNon-black pixels: {}\nResult: {}",
            self.vmid,
            self.first_frame_ms,
            self.frame_width,
            self.frame_height,
            self.non_black_pixels,
            result
        )
    }
}
