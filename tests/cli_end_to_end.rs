use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
};

use assert_cmd::Command as AssertCommand;
use rustedoutclient::{
    app::{StartupAction, StartupCoordinator},
    cli::{execute_headless, Cli, CliFuture, CliRuntime, Command, ViewerMode},
    connection::{DesktopSize, FbRect},
    model::{NodeName, VmId},
    session::{
        AppCommand, AppEvent, PublicError, PublicErrorKind, ResizeStatus, SessionId, SessionPhase,
        SessionSnapshot,
    },
    ssh::{InventorySnapshot, VmInventoryItem, VmStatus},
};

fn vmid(value: u32) -> VmId {
    VmId::new(value).unwrap()
}

fn inventory(stale: bool, second_status: VmStatus) -> InventorySnapshot {
    InventorySnapshot::new(
        if stale { 1 } else { 2 },
        stale,
        vec![
            VmInventoryItem {
                vmid: vmid(107),
                name: if stale {
                    "CACHED-NAME".to_owned()
                } else {
                    "SYNTHETIC-107".to_owned()
                },
                node: NodeName::parse("pve2").unwrap(),
                status: VmStatus::Running,
                template: false,
            },
            VmInventoryItem {
                vmid: vmid(205),
                name: "SYNTHETIC-205".to_owned(),
                node: NodeName::parse("pve2").unwrap(),
                status: second_status,
                template: false,
            },
        ],
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandFact {
    Open(VmId),
    Close(SessionId),
    Other,
}

#[derive(Default)]
struct RuntimeEvidence {
    commands: Vec<CommandFact>,
    shutdowns: usize,
}

struct SyntheticRuntime {
    events: VecDeque<AppEvent>,
    evidence: Arc<Mutex<RuntimeEvidence>>,
    shutdown_error: Option<PublicError>,
    pending_when_empty: bool,
}

impl SyntheticRuntime {
    fn new(events: impl IntoIterator<Item = AppEvent>) -> (Self, Arc<Mutex<RuntimeEvidence>>) {
        let evidence = Arc::new(Mutex::new(RuntimeEvidence::default()));
        (
            Self {
                events: events.into_iter().collect(),
                evidence: Arc::clone(&evidence),
                shutdown_error: None,
                pending_when_empty: false,
            },
            evidence,
        )
    }

    fn pending_after(
        events: impl IntoIterator<Item = AppEvent>,
    ) -> (Self, Arc<Mutex<RuntimeEvidence>>) {
        let (mut runtime, evidence) = Self::new(events);
        runtime.pending_when_empty = true;
        (runtime, evidence)
    }

    fn with_shutdown_error(
        events: impl IntoIterator<Item = AppEvent>,
        error: PublicError,
    ) -> (Self, Arc<Mutex<RuntimeEvidence>>) {
        let (mut runtime, evidence) = Self::new(events);
        runtime.shutdown_error = Some(error);
        (runtime, evidence)
    }
}

impl CliRuntime for SyntheticRuntime {
    fn send(&self, command: AppCommand) -> CliFuture<'_, Result<(), PublicError>> {
        let fact = match command {
            AppCommand::Open { vmid, .. } => CommandFact::Open(vmid),
            AppCommand::Close { session_id } => CommandFact::Close(session_id),
            _ => CommandFact::Other,
        };
        self.evidence.lock().unwrap().commands.push(fact);
        Box::pin(async { Ok(()) })
    }

    fn recv(&mut self) -> CliFuture<'_, Option<AppEvent>> {
        let event = self.events.pop_front();
        let pending_when_empty = self.pending_when_empty;
        Box::pin(async move {
            match event {
                Some(event) => Some(event),
                None if pending_when_empty => std::future::pending().await,
                None => None,
            }
        })
    }

    fn shutdown(self) -> CliFuture<'static, Result<(), PublicError>> {
        self.evidence.lock().unwrap().shutdowns += 1;
        Box::pin(async move { self.shutdown_error.map_or(Ok(()), Err) })
    }
}

#[tokio::test]
async fn list_ignores_cache_waits_for_live_inventory_prints_only_three_fields_and_shuts_down() {
    let (runtime, evidence) = SyntheticRuntime::new([
        AppEvent::CachedInventory(inventory(true, VmStatus::Stopped)),
        AppEvent::LiveInventory(inventory(false, VmStatus::Stopped)),
    ]);
    let mut output = Vec::new();

    let exit = execute_headless(Command::List, runtime, &mut output).await;

    assert!(exit.success());
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "107\trunning\tSYNTHETIC-107\n205\tstopped\tSYNTHETIC-205\n"
    );
    assert_eq!(evidence.lock().unwrap().shutdowns, 1);
    assert!(evidence.lock().unwrap().commands.is_empty());
}

fn snapshot(session_id: SessionId, phase: SessionPhase) -> SessionSnapshot {
    SessionSnapshot {
        session_id,
        profile_name: "Synthetic profile".to_owned(),
        vmid: vmid(107),
        phase,
        view_only: false,
        clipboard_enabled: false,
        dynamic_resolution_enabled: true,
        guest_size: Some(DesktopSize::new(2, 1)),
        resize_status: ResizeStatus::Waiting,
    }
}

#[tokio::test]
async fn probe_uses_live_selection_observes_one_frame_closes_exact_session_then_shuts_down() {
    let session_id = SessionId::new();
    let (runtime, evidence) = SyntheticRuntime::new([
        AppEvent::CachedInventory(inventory(true, VmStatus::Stopped)),
        AppEvent::LiveInventory(inventory(false, VmStatus::Stopped)),
        AppEvent::SessionChanged(snapshot(session_id, SessionPhase::Opening)),
        AppEvent::SessionChanged(snapshot(session_id, SessionPhase::StartingProxy)),
        AppEvent::SessionChanged(snapshot(session_id, SessionPhase::NegotiatingRfb)),
        AppEvent::SessionChanged(snapshot(session_id, SessionPhase::Ready)),
        AppEvent::Framebuffer {
            session_id,
            rects: vec![FbRect {
                x: 0,
                y: 0,
                w: 2,
                h: 1,
                rgba: vec![0, 0, 0, 255, 9, 8, 7, 255],
            }],
        },
        AppEvent::SessionChanged(snapshot(session_id, SessionPhase::Disconnecting)),
        AppEvent::SessionChanged(snapshot(session_id, SessionPhase::Disconnected)),
    ]);
    let mut output = Vec::new();

    let exit = execute_headless(
        Command::Probe {
            selector: "107".to_owned(),
            timeout_seconds: 30,
            json: true,
        },
        runtime,
        &mut output,
    )
    .await;

    assert!(exit.success());
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["vmid"], 107);
    assert_eq!(value["frame_width"], 2);
    assert_eq!(value["frame_height"], 1);
    assert_eq!(value["non_black_pixels"], 1);
    assert_eq!(value["result"], "success");
    assert!(!String::from_utf8(output)
        .unwrap()
        .contains("pve.example.invalid"));
    let evidence = evidence.lock().unwrap();
    assert_eq!(
        evidence.commands,
        [CommandFact::Open(vmid(107)), CommandFact::Close(session_id)]
    );
    assert_eq!(evidence.shutdowns, 1);
}

#[tokio::test(start_paused = true)]
async fn probe_timeout_is_typed_safe_nonzero_and_still_shuts_down() {
    let (runtime, evidence) = SyntheticRuntime::pending_after([AppEvent::LiveInventory(
        inventory(false, VmStatus::Stopped),
    )]);
    let mut output = Vec::new();

    let exit = execute_headless(
        Command::Probe {
            selector: "107".to_owned(),
            timeout_seconds: 1,
            json: true,
        },
        runtime,
        &mut output,
    )
    .await;

    assert!(!exit.success());
    let rendered = String::from_utf8(output).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&rendered).unwrap()["result"],
        "timeout"
    );
    for forbidden in [
        "root@example.invalid",
        "Ab12Cd34",
        "PRIVATE KEY",
        "raw ssh sentinel",
        "clipboard sentinel",
    ] {
        assert!(!rendered.contains(forbidden));
    }
    assert_eq!(evidence.lock().unwrap().shutdowns, 1);
}

#[test]
fn probe_timeout_parser_accepts_only_the_approved_finite_range() {
    use clap::Parser;

    for accepted in [1_u64, 30, 300] {
        assert!(Cli::try_parse_from([
            "rustedoutclient",
            "probe",
            "107",
            "--timeout-seconds",
            &accepted.to_string(),
        ])
        .is_ok());
    }
    for rejected in [0_u64, 301, u64::MAX] {
        assert!(Cli::try_parse_from([
            "rustedoutclient",
            "probe",
            "107",
            "--timeout-seconds",
            &rejected.to_string(),
        ])
        .is_err());
    }
}

#[tokio::test]
async fn closed_inventory_channel_fails_promptly_and_still_shuts_down() {
    let (runtime, evidence) = SyntheticRuntime::new([]);
    let mut output = Vec::new();

    let exit = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        execute_headless(Command::List, runtime, &mut output),
    )
    .await
    .expect("closed inventory channel must not wait for the 30-second deadline");

    assert_eq!(exit.error().unwrap().kind(), PublicErrorKind::Queue);
    assert_eq!(evidence.lock().unwrap().shutdowns, 1);
}

#[tokio::test]
async fn closed_probe_channel_fails_promptly_attempts_exact_close_and_shuts_down() {
    let session_id = SessionId::new();
    let (runtime, evidence) = SyntheticRuntime::new([
        AppEvent::LiveInventory(inventory(false, VmStatus::Stopped)),
        AppEvent::SessionChanged(snapshot(session_id, SessionPhase::Opening)),
    ]);
    let mut output = Vec::new();

    let exit = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        execute_headless(
            Command::Probe {
                selector: "107".to_owned(),
                timeout_seconds: 30,
                json: true,
            },
            runtime,
            &mut output,
        ),
    )
    .await
    .expect("closed probe channel must not wait for the 30-second deadline");

    assert!(!exit.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output).unwrap()["result"],
        "cleanup"
    );
    let evidence = evidence.lock().unwrap();
    assert_eq!(
        evidence.commands,
        [CommandFact::Open(vmid(107)), CommandFact::Close(session_id)]
    );
    assert_eq!(evidence.shutdowns, 1);
}

#[tokio::test]
async fn direct_runtime_timeout_overflow_is_typed_instead_of_panicking() {
    let (runtime, evidence) = SyntheticRuntime::new([]);
    let task = tokio::spawn(async move {
        let mut output = Vec::new();
        let exit = execute_headless(
            Command::Probe {
                selector: "107".to_owned(),
                timeout_seconds: u64::MAX,
                json: true,
            },
            runtime,
            &mut output,
        )
        .await;
        (exit, output)
    });

    let (exit, output) = tokio::time::timeout(std::time::Duration::from_millis(250), task)
        .await
        .expect("checked timeout defense must be prompt")
        .expect("checked timeout defense must not panic");
    assert!(!exit.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output).unwrap()["result"],
        "queue"
    );
    assert_eq!(evidence.lock().unwrap().shutdowns, 1);
}

#[tokio::test]
async fn list_composes_primary_and_cleanup_results_without_losing_truth() {
    let live = || AppEvent::LiveInventory(inventory(false, VmStatus::Stopped));
    let primary = || AppEvent::Error(PublicError::new(PublicErrorKind::Inventory));
    let cleanup = || PublicError::new(PublicErrorKind::Cleanup);

    let (runtime, _) = SyntheticRuntime::new([live()]);
    let mut output = Vec::new();
    let exit = execute_headless(Command::List, runtime, &mut output).await;
    assert!(exit.success());

    let (runtime, _) = SyntheticRuntime::new([primary()]);
    let mut output = Vec::new();
    let exit = execute_headless(Command::List, runtime, &mut output).await;
    let error = exit.error().unwrap();
    assert_eq!(error.kind(), PublicErrorKind::Inventory);
    assert!(!error.has_cleanup_failure());

    let (runtime, _) = SyntheticRuntime::with_shutdown_error([live()], cleanup());
    let mut output = Vec::new();
    let exit = execute_headless(Command::List, runtime, &mut output).await;
    let error = exit.error().unwrap();
    assert_eq!(error.kind(), PublicErrorKind::Cleanup);
    assert!(!error.has_cleanup_failure());

    let (runtime, _) = SyntheticRuntime::with_shutdown_error([primary()], cleanup());
    let mut output = Vec::new();
    let exit = execute_headless(Command::List, runtime, &mut output).await;
    let error = exit.error().unwrap();
    assert_eq!(error.kind(), PublicErrorKind::Inventory);
    assert!(error.has_cleanup_failure());
}

#[test]
fn native_and_explicit_fallback_startup_intents_wait_for_live_inventory_and_never_auto_fallback() {
    use clap::Parser;

    for (arguments, tiger) in [
        (
            vec!["rustedoutclient", "open", "107", "--fullscreen"],
            false,
        ),
        (
            vec![
                "rustedoutclient",
                "open",
                "107",
                "--fullscreen",
                "--view-only",
                "--viewer",
                "tiger-vnc",
            ],
            true,
        ),
    ] {
        let command = Cli::try_parse_from(arguments).unwrap().command.unwrap();
        let request = command
            .startup_request()
            .expect("open must create one startup request");
        let mut coordinator = StartupCoordinator::new(request);
        assert!(coordinator
            .observe(
                &AppEvent::CachedInventory(inventory(true, VmStatus::Stopped)),
                true
            )
            .is_none());
        let command = coordinator
            .observe(
                &AppEvent::LiveInventory(inventory(false, VmStatus::Stopped)),
                true,
            )
            .expect("live inventory must resolve the startup request")
            .unwrap();
        match command {
            StartupAction::Native {
                vmid: selected,
                view_only,
            } if !tiger => {
                assert_eq!(selected, vmid(107));
                assert_eq!(view_only, None);
            }
            StartupAction::TigerVnc {
                vmid: selected,
                fullscreen,
                view_only,
            } if tiger => {
                assert_eq!(selected, vmid(107));
                assert!(fullscreen);
                assert_eq!(view_only, Some(true));
            }
            _ => panic!("startup intent changed transport or fell back automatically"),
        }
        assert!(coordinator
            .observe(
                &AppEvent::LiveInventory(inventory(false, VmStatus::Stopped)),
                true
            )
            .is_none());
    }

    let command = Command::Open {
        selector: "205".to_owned(),
        fullscreen: false,
        view_only: None,
        viewer: ViewerMode::Native,
    };
    let mut coordinator = StartupCoordinator::new(command.startup_request().unwrap());
    let error = match coordinator
        .observe(
            &AppEvent::LiveInventory(inventory(false, VmStatus::Stopped)),
            true,
        )
        .expect("live inventory must resolve the request")
    {
        Err(error) => error,
        Ok(_) => panic!("stopped VM unexpectedly produced a startup command"),
    };
    assert_eq!(error.kind(), PublicErrorKind::VmNotRunning);
}

#[test]
fn synthetic_process_entry() {
    if std::env::var_os("ROC_SYNTHETIC_CLI_CHILD").is_none() {
        return;
    }
    let (runtime, _) =
        SyntheticRuntime::new([AppEvent::LiveInventory(inventory(false, VmStatus::Stopped))]);
    let mut output = Vec::new();
    let exit = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(execute_headless(Command::List, runtime, &mut output));
    assert!(exit.success());
    print!("{}", String::from_utf8(output).unwrap());
}

#[test]
fn successful_synthetic_process_self_spawns_test_only_while_real_binary_stays_override_free() {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args(["synthetic_process_entry", "--exact", "--nocapture"])
        .env("ROC_SYNTHETIC_CLI_CHILD", "1");
    let output = AssertCommand::from_std(child)
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("107\trunning\tSYNTHETIC-107"));
    assert!(!stdout.contains("root@pve.example.invalid"));

    let help = AssertCommand::cargo_bin("rustedoutclient")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .clone();
    let help = String::from_utf8(help.stdout).unwrap();
    for forbidden in [
        "--host",
        "--endpoint",
        "--password",
        "--ticket",
        "--ssh-executable",
        "--backend",
        "--fixture",
        "--fake-ssh",
    ] {
        assert!(!help.contains(forbidden));
    }
    AssertCommand::cargo_bin("rustedoutclient")
        .unwrap()
        .arg("--version")
        .assert()
        .success();
}

#[test]
fn real_binary_missing_private_config_failure_is_typed_and_redacted() {
    let home = tempfile::tempdir().unwrap();
    let output = AssertCommand::cargo_bin("rustedoutclient")
        .unwrap()
        .env("HOME", home.path())
        .arg("list")
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("configuration failed"));
    for forbidden in [
        home.path().to_string_lossy().as_ref(),
        "root@example.invalid",
        "Ab12Cd34",
        "PRIVATE KEY",
        "raw ssh sentinel",
    ] {
        assert!(!stderr.contains(forbidden));
    }
}

#[allow(dead_code)]
fn _io_type_is_not_a_runtime_override(_: io::Error) {}
