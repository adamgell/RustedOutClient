use std::{collections::BTreeSet, time::Duration};

use rustedoutclient::{
    app::{
        dispatch_action, AppCommandSink, AppState, ClipboardAdapter, ClipboardAdapterError,
        CommandQueueError, UiAction,
    },
    config::AppConfig,
    connection::{DesktopSize, FbRect},
    diagnostics::{
        ChildExitStatus, DiagnosticFailure, DiagnosticRecord, PhaseTiming, ProbeReport,
        ProbeResult, UPSTREAM_BASE_SHA,
    },
    model::{NodeName, PveProfile, SshTarget, VmId},
    session::{
        AppCommand, AppEvent, PublicError, PublicErrorKind, ResizeStatus, SessionId, SessionPhase,
        SessionSnapshot,
    },
    ssh::{InventorySnapshot, VmInventoryItem, VmStatus},
};

fn profile() -> PveProfile {
    PveProfile {
        name: "Synthetic profile".to_owned(),
        ssh_target: SshTarget::parse("root@pve.example.invalid").unwrap(),
        node: NodeName::parse("pve2").unwrap(),
    }
}

fn vmid(value: u32) -> VmId {
    VmId::new(value).unwrap()
}

#[test]
fn diagnostic_json_is_an_exact_typed_allowlist_and_never_accepts_raw_failure_material() {
    struct InternalFailureFixture {
        target: &'static str,
        ticket: &'static str,
        private_key: &'static str,
        clipboard: &'static str,
        stderr: &'static str,
        framebuffer: &'static [u8],
        path: &'static str,
        pid: u32,
        session_uuid: &'static str,
        host_fingerprint: &'static str,
        vm_name: &'static str,
        public: PublicError,
    }

    let session_id = SessionId::new();
    let failure = InternalFailureFixture {
        target: "root@example.invalid",
        ticket: "Ab12Cd34",
        private_key: "PRIVATE KEY",
        clipboard: "clipboard sentinel",
        stderr: "raw ssh sentinel",
        framebuffer: b"framebuffer sentinel",
        path: "/private/synthetic/path",
        pid: 4242,
        session_uuid: "11111111-2222-3333-4444-555555555555",
        host_fingerprint: "SHA256:synthetic-host-fingerprint",
        vm_name: "VM-NAME-SENTINEL",
        public: PublicError::new(PublicErrorKind::RfbProtocol)
            .with_public_context(session_id, vmid(107))
            .with_cleanup_failure(),
    };
    let phases = vec![
        PhaseTiming::new(SessionPhase::Opening, Duration::from_millis(7)),
        PhaseTiming::new(SessionPhase::StartingProxy, Duration::from_millis(11)),
        PhaseTiming::new(SessionPhase::NegotiatingRfb, Duration::from_millis(13)),
    ];
    let record = DiagnosticRecord::new(
        profile().name,
        profile().node,
        Some(vmid(107)),
        phases,
        Some(ChildExitStatus::new(23)),
        Some(DiagnosticFailure::from_public(failure.public)),
    );

    let exported = record.to_json().unwrap();
    let json: serde_json::Value = serde_json::from_str(&exported).unwrap();
    let keys = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        BTreeSet::from([
            "app_version",
            "architecture",
            "child_exit_status",
            "cleanup_failed",
            "error_category",
            "node",
            "os",
            "phases",
            "profile_display_name",
            "upstream_base_sha",
            "vmid",
        ])
    );
    assert_eq!(json["app_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["upstream_base_sha"], UPSTREAM_BASE_SHA);
    assert_eq!(json["profile_display_name"], "Synthetic profile");
    assert_eq!(json["node"], "pve2");
    assert_eq!(json["vmid"], 107);
    assert_eq!(json["child_exit_status"], 23);
    assert_eq!(json["error_category"], "rfb_protocol");
    assert_eq!(json["cleanup_failed"], true);
    assert_eq!(
        json["phases"],
        serde_json::json!([
            {"phase": "opening", "duration_ms": 7},
            {"phase": "starting_proxy", "duration_ms": 11},
            {"phase": "negotiating_rfb", "duration_ms": 13}
        ])
    );

    for forbidden in [
        failure.target,
        failure.ticket,
        failure.private_key,
        failure.clipboard,
        failure.stderr,
        std::str::from_utf8(failure.framebuffer).unwrap(),
        failure.path,
        &failure.pid.to_string(),
        failure.session_uuid,
        failure.host_fingerprint,
        failure.vm_name,
    ] {
        assert!(!exported.contains(forbidden), "leaked {forbidden:?}");
    }
    assert!(!format!("{record:?}").contains(failure.target));
}

#[test]
fn phase_timing_uses_bounded_monotonic_milliseconds() {
    let saturated = PhaseTiming::new(SessionPhase::Ready, Duration::MAX);
    assert_eq!(saturated.duration_ms(), u64::MAX);
    assert_eq!(saturated.phase(), SessionPhase::Ready);
}

#[test]
fn probe_json_has_exact_keys_counts_rgb_only_and_has_safe_typed_failure_output() {
    let report = ProbeReport::from_frame(
        vmid(107),
        Duration::from_millis(37),
        DesktopSize::new(2, 1),
        &[FbRect {
            x: 0,
            y: 0,
            w: 2,
            h: 1,
            rgba: vec![0, 0, 0, 255, 1, 2, 3, 0],
        }],
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&report.to_json().unwrap()).unwrap();
    assert_eq!(
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "first_frame_ms",
            "frame_height",
            "frame_width",
            "non_black_pixels",
            "result",
            "vmid",
        ])
    );
    assert_eq!(
        value,
        serde_json::json!({
            "vmid": 107,
            "first_frame_ms": 37,
            "frame_width": 2,
            "frame_height": 1,
            "non_black_pixels": 1,
            "result": "success"
        })
    );
    assert!(!report.to_text().contains("pve.example.invalid"));

    let failure = ProbeReport::failure(Some(vmid(107)), ProbeResult::Timeout);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&failure.to_json().unwrap()).unwrap(),
        serde_json::json!({
            "vmid": 107,
            "first_frame_ms": 0,
            "frame_width": 0,
            "frame_height": 0,
            "non_black_pixels": 0,
            "result": "timeout"
        })
    );
    for forbidden in [
        "root@example.invalid",
        "Ab12Cd34",
        "raw ssh sentinel",
        "clipboard sentinel",
    ] {
        assert!(!failure.to_json().unwrap().contains(forbidden));
    }
}

#[derive(Default)]
struct RecordingClipboard {
    writes: Vec<String>,
}

impl ClipboardAdapter for RecordingClipboard {
    fn read_text(&mut self) -> Result<Option<String>, ClipboardAdapterError> {
        Ok(None)
    }

    fn write_text(&mut self, text: String) -> Result<(), ClipboardAdapterError> {
        self.writes.push(text);
        Ok(())
    }
}

struct NoCommands;

impl AppCommandSink for NoCommands {
    fn try_send(&self, _command: AppCommand) -> Result<(), CommandQueueError> {
        panic!("copy diagnostics must remain a local typed export")
    }
}

#[test]
fn gui_text_and_copy_use_the_same_typed_record_with_ordered_phase_and_cleanup_truth() {
    let mut state = AppState::from_config(&AppConfig::new(profile()));
    state
        .apply(AppEvent::LiveInventory(InventorySnapshot::new(
            1,
            false,
            vec![VmInventoryItem {
                vmid: vmid(107),
                name: "VM-NAME-SENTINEL".to_owned(),
                node: profile().node,
                status: VmStatus::Running,
                template: false,
            }],
        )))
        .unwrap();
    let session_id = SessionId::new();
    state
        .apply(AppEvent::SessionChanged(SessionSnapshot {
            session_id,
            profile_name: "Synthetic profile".to_owned(),
            vmid: vmid(107),
            phase: SessionPhase::NegotiatingRfb,
            view_only: false,
            clipboard_enabled: false,
            dynamic_resolution_enabled: true,
            guest_size: Some(DesktopSize::new(64, 64)),
            resize_status: ResizeStatus::Waiting,
        }))
        .unwrap();
    for timing in [
        PhaseTiming::new(SessionPhase::Opening, Duration::from_millis(3)),
        PhaseTiming::new(SessionPhase::StartingProxy, Duration::from_millis(5)),
    ] {
        state
            .apply(AppEvent::PhaseTiming { session_id, timing })
            .unwrap();
    }
    state
        .apply(AppEvent::ChildExitStatus {
            session_id,
            status: ChildExitStatus::new(9),
        })
        .unwrap();
    state
        .apply(AppEvent::Error(
            PublicError::new(PublicErrorKind::Decoder)
                .with_public_context(session_id, vmid(107))
                .with_cleanup_failure(),
        ))
        .unwrap();

    let record = state.diagnostic_record();
    let rendered = record.to_text();
    assert_eq!(state.diagnostics_summary(), rendered);
    assert!(rendered.contains("opening: 3 ms"));
    assert!(rendered.contains("starting_proxy: 5 ms"));
    assert!(rendered.contains("Child exit status: 9"));
    assert!(rendered.contains("Error category: decoder"));
    assert!(rendered.contains("Cleanup failure: true"));
    assert!(!rendered.contains("VM-NAME-SENTINEL"));
    assert!(!rendered.contains("64x64"));

    let mut clipboard = RecordingClipboard::default();
    assert_eq!(
        dispatch_action(
            &mut state,
            &NoCommands,
            &mut clipboard,
            UiAction::CopyDiagnostics,
        ),
        rustedoutclient::app::DispatchOutcome::AppliedLocally
    );
    assert_eq!(clipboard.writes, [record.to_text()]);
}
