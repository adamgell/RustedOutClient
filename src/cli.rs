use std::{future::Future, io::Write, path::Path, pin::Pin, time::Duration};

use clap::{Parser, Subcommand, ValueEnum};
use tokio::time::{sleep_until, timeout_at, Instant};

use crate::{
    config::{default_config_path, load_config_from_path},
    diagnostics::{ProbeReport, ProbeResult},
    model::VmId,
    session::{
        AppCommand, AppEvent, OpenOptions, PublicError, PublicErrorKind, SessionId, SessionManager,
        SessionPhase,
    },
    ssh::{InventorySnapshot, VmStatus},
};

const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(
    name = "rustedoutclient",
    version,
    about = "RustedOutClient Proxmox console"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    List,
    Open {
        selector: String,
        #[arg(long)]
        fullscreen: bool,
        #[arg(long)]
        view_only: bool,
        #[arg(long, value_enum, default_value_t = ViewerMode::Native)]
        viewer: ViewerMode,
    },
    Probe {
        selector: String,
        #[arg(long, default_value_t = 30)]
        timeout_seconds: u64,
        #[arg(long)]
        json: bool,
    },
}

impl Command {
    pub fn startup_request(&self) -> Option<StartupRequest> {
        match self {
            Self::Open {
                selector,
                fullscreen,
                view_only,
                viewer,
            } => Some(StartupRequest {
                selector: selector.clone(),
                fullscreen: *fullscreen,
                view_only: *view_only,
                viewer: *viewer,
            }),
            Self::List | Self::Probe { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ViewerMode {
    Native,
    #[value(name = "tiger-vnc")]
    TigerVnc,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupRequest {
    selector: String,
    fullscreen: bool,
    view_only: bool,
    viewer: ViewerMode,
}

impl StartupRequest {
    pub fn selector(&self) -> &str {
        &self.selector
    }

    pub fn fullscreen(&self) -> bool {
        self.fullscreen
    }

    pub fn view_only(&self) -> bool {
        self.view_only
    }

    pub fn viewer(&self) -> ViewerMode {
        self.viewer
    }
}

pub type CliFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait CliRuntime: Sized {
    fn send(&self, command: AppCommand) -> CliFuture<'_, Result<(), PublicError>>;
    fn recv(&mut self) -> CliFuture<'_, Option<AppEvent>>;
    fn shutdown(self) -> CliFuture<'static, Result<(), PublicError>>;
}

impl CliRuntime for SessionManager {
    fn send(&self, command: AppCommand) -> CliFuture<'_, Result<(), PublicError>> {
        Box::pin(async move {
            SessionManager::send(self, command)
                .await
                .map_err(|_| PublicError::new(PublicErrorKind::Queue))
        })
    }

    fn recv(&mut self) -> CliFuture<'_, Option<AppEvent>> {
        Box::pin(SessionManager::recv(self))
    }

    fn shutdown(self) -> CliFuture<'static, Result<(), PublicError>> {
        Box::pin(SessionManager::shutdown(self))
    }
}

pub struct ProductionCliRuntime(SessionManager);

impl ProductionCliRuntime {
    pub fn load() -> Result<Self, PublicError> {
        let config_path =
            default_config_path().map_err(|_| PublicError::new(PublicErrorKind::Config))?;
        let config = load_config_from_path(&config_path)
            .map_err(|_| PublicError::new(PublicErrorKind::Config))?;
        let config_directory = Path::new(&config_path)
            .parent()
            .ok_or_else(|| PublicError::new(PublicErrorKind::Config))?;
        let manager =
            SessionManager::spawn_production(config, config_directory.join("inventory-v1.json"))?;
        Ok(Self(manager))
    }
}

impl CliRuntime for ProductionCliRuntime {
    fn send(&self, command: AppCommand) -> CliFuture<'_, Result<(), PublicError>> {
        CliRuntime::send(&self.0, command)
    }

    fn recv(&mut self) -> CliFuture<'_, Option<AppEvent>> {
        CliRuntime::recv(&mut self.0)
    }

    fn shutdown(self) -> CliFuture<'static, Result<(), PublicError>> {
        CliRuntime::shutdown(self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CliExit {
    success: bool,
    error: Option<PublicError>,
}

impl CliExit {
    fn ok() -> Self {
        Self {
            success: true,
            error: None,
        }
    }

    fn failed(error: PublicError) -> Self {
        Self {
            success: false,
            error: Some(error),
        }
    }

    fn report_failure() -> Self {
        Self {
            success: false,
            error: None,
        }
    }

    pub fn success(&self) -> bool {
        self.success
    }

    pub fn error(&self) -> Option<PublicError> {
        self.error
    }
}

pub async fn execute_headless<R, W>(command: Command, runtime: R, output: &mut W) -> CliExit
where
    R: CliRuntime,
    W: Write,
{
    match command {
        Command::List => execute_list(runtime, output).await,
        Command::Probe {
            selector,
            timeout_seconds,
            json,
        } => execute_probe(runtime, output, &selector, timeout_seconds, json).await,
        Command::Open { .. } => {
            let shutdown = runtime.shutdown().await;
            CliExit::failed(
                shutdown
                    .err()
                    .unwrap_or_else(|| PublicError::new(PublicErrorKind::Config)),
            )
        }
    }
}

async fn execute_list<R, W>(mut runtime: R, output: &mut W) -> CliExit
where
    R: CliRuntime,
    W: Write,
{
    let deadline = Instant::now() + LIST_TIMEOUT;
    let operation = wait_for_live_inventory(&mut runtime, deadline)
        .await
        .and_then(|snapshot| {
            for item in snapshot.vms {
                let status = match item.status {
                    VmStatus::Running => "running",
                    VmStatus::Stopped => "stopped",
                };
                writeln!(output, "{}\t{status}\t{}", item.vmid, item.name)
                    .map_err(|_| PublicError::new(PublicErrorKind::Queue))?;
            }
            Ok(())
        });
    let cleanup = runtime.shutdown().await;
    match operation.and(cleanup) {
        Ok(()) => CliExit::ok(),
        Err(error) => CliExit::failed(error),
    }
}

async fn wait_for_live_inventory<R>(
    runtime: &mut R,
    deadline: Instant,
) -> Result<InventorySnapshot, PublicError>
where
    R: CliRuntime,
{
    loop {
        match timeout_at(deadline, runtime.recv()).await {
            Ok(Some(AppEvent::LiveInventory(snapshot))) => return Ok(snapshot),
            Ok(Some(AppEvent::Error(error))) => return Err(error),
            Ok(Some(_)) => {}
            Ok(None) => {
                sleep_until(deadline).await;
                return Err(PublicError::new(PublicErrorKind::Queue));
            }
            Err(_) => return Err(PublicError::new(PublicErrorKind::Queue)),
        }
    }
}

struct ProbeObservation {
    session_id: Option<SessionId>,
    report: Result<ProbeReport, ProbeResult>,
}

async fn execute_probe<R, W>(
    mut runtime: R,
    output: &mut W,
    selector: &str,
    timeout_seconds: u64,
    json: bool,
) -> CliExit
where
    R: CliRuntime,
    W: Write,
{
    let timeout_duration = Duration::from_secs(timeout_seconds);
    let selection_deadline = Instant::now() + timeout_duration;
    let selection = wait_for_live_inventory_for_probe(&mut runtime, selection_deadline).await;
    let (selected, mut observation) = match selection {
        Ok(snapshot) => match select_running(&snapshot, selector) {
            Ok(selected) => (
                Some(selected),
                observe_probe(&mut runtime, selected, timeout_duration).await,
            ),
            Err(result) => (
                None,
                ProbeObservation {
                    session_id: None,
                    report: Err(result),
                },
            ),
        },
        Err(result) => (
            None,
            ProbeObservation {
                session_id: None,
                report: Err(result),
            },
        ),
    };

    let close_result = if let Some(session_id) = observation.session_id {
        close_exact_session(&mut runtime, session_id).await
    } else {
        Ok(())
    };
    let shutdown_result = runtime.shutdown().await;
    if close_result.is_err() || shutdown_result.is_err() {
        observation.report = Err(ProbeResult::Cleanup);
    }

    let report = match observation.report {
        Ok(report) => report,
        Err(result) => ProbeReport::failure(selected, result),
    };
    let rendered = if json {
        report
            .to_json()
            .map_err(|_| PublicError::new(PublicErrorKind::Queue))
    } else {
        Ok(report.to_text())
    };
    let write_result = rendered.and_then(|rendered| {
        writeln!(output, "{rendered}").map_err(|_| PublicError::new(PublicErrorKind::Queue))
    });

    match (report_is_success(&report), write_result) {
        (true, Ok(())) => CliExit::ok(),
        (_, Err(error)) => CliExit::failed(error),
        (false, Ok(())) => CliExit::report_failure(),
    }
}

async fn wait_for_live_inventory_for_probe<R>(
    runtime: &mut R,
    deadline: Instant,
) -> Result<InventorySnapshot, ProbeResult>
where
    R: CliRuntime,
{
    loop {
        match timeout_at(deadline, runtime.recv()).await {
            Ok(Some(AppEvent::LiveInventory(snapshot))) => return Ok(snapshot),
            Ok(Some(AppEvent::Error(error))) => return Err(error.kind().into()),
            Ok(Some(_)) => {}
            Ok(None) => {
                sleep_until(deadline).await;
                return Err(ProbeResult::Timeout);
            }
            Err(_) => return Err(ProbeResult::Timeout),
        }
    }
}

fn select_running(snapshot: &InventorySnapshot, selector: &str) -> Result<VmId, ProbeResult> {
    let item = snapshot
        .select(selector)
        .map_err(|_| ProbeResult::VmNotFound)?;
    if item.status != VmStatus::Running {
        return Err(ProbeResult::VmNotRunning);
    }
    Ok(item.vmid)
}

async fn observe_probe<R>(
    runtime: &mut R,
    selected: VmId,
    timeout_duration: Duration,
) -> ProbeObservation
where
    R: CliRuntime,
{
    let started = Instant::now();
    let deadline = started + timeout_duration;
    if let Err(error) = runtime
        .send(AppCommand::Open {
            vmid: selected,
            options: OpenOptions::default(),
        })
        .await
    {
        return ProbeObservation {
            session_id: None,
            report: Err(error.kind().into()),
        };
    }

    let mut session_id = None;
    let mut size = None;
    loop {
        let event = match timeout_at(deadline, runtime.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => {
                sleep_until(deadline).await;
                return ProbeObservation {
                    session_id,
                    report: Err(ProbeResult::Timeout),
                };
            }
            Err(_) => {
                return ProbeObservation {
                    session_id,
                    report: Err(ProbeResult::Timeout),
                }
            }
        };
        match event {
            AppEvent::SessionChanged(snapshot) if snapshot.vmid == selected => {
                session_id.get_or_insert(snapshot.session_id);
                if session_id == Some(snapshot.session_id) {
                    if let Some(guest_size) = snapshot.guest_size {
                        size = Some(guest_size);
                    }
                    if snapshot.phase == SessionPhase::Disconnected {
                        return ProbeObservation {
                            session_id,
                            report: Err(ProbeResult::Cleanup),
                        };
                    }
                }
            }
            AppEvent::Framebuffer {
                session_id: frame_session,
                rects,
            } if Some(frame_session) == session_id && !rects.is_empty() => {
                let Some(frame_size) = size else {
                    return ProbeObservation {
                        session_id,
                        report: Err(ProbeResult::RfbProtocol),
                    };
                };
                return ProbeObservation {
                    session_id,
                    report: ProbeReport::from_frame(
                        selected,
                        started.elapsed(),
                        frame_size,
                        &rects,
                    )
                    .map_err(|error| error.kind().into()),
                };
            }
            AppEvent::Error(error)
                if error.session_id().is_none() || error.session_id() == session_id =>
            {
                return ProbeObservation {
                    session_id,
                    report: Err(error.kind().into()),
                };
            }
            _ => {}
        }
    }
}

async fn close_exact_session<R>(runtime: &mut R, session_id: SessionId) -> Result<(), PublicError>
where
    R: CliRuntime,
{
    runtime.send(AppCommand::Close { session_id }).await?;
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    loop {
        match timeout_at(deadline, runtime.recv()).await {
            Ok(Some(AppEvent::SessionChanged(snapshot)))
                if snapshot.session_id == session_id
                    && snapshot.phase == SessionPhase::Disconnected =>
            {
                return Ok(())
            }
            Ok(Some(AppEvent::Error(error)))
                if error.session_id().is_none() || error.session_id() == Some(session_id) =>
            {
                return Err(error)
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return Err(PublicError::new(PublicErrorKind::Cleanup)),
        }
    }
}

fn report_is_success(report: &ProbeReport) -> bool {
    report.result() == ProbeResult::Success
}
