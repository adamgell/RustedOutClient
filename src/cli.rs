use std::{future::Future, io::Write, path::Path, pin::Pin, time::Duration};

use clap::{Parser, Subcommand, ValueEnum};
use tokio::time::{timeout_at, Instant};

use crate::{
    config::{default_config_path, load_config_from_path},
    connection::DesktopSize,
    diagnostics::{ProbeReport, ProbeResult},
    model::VmId,
    session::{
        AppCommand, AppEvent, OpenOptions, PublicError, PublicErrorKind, ResizeStatus, SessionId,
        SessionManager, SessionPhase,
    },
    ssh::{InventorySnapshot, VmStatus},
    vnc::{normalize_resize_request, ProtocolLimits},
};

const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_PROBE_TIMEOUT_SECONDS: u64 = 30;
const MIN_PROBE_TIMEOUT_SECONDS: u64 = 1;
const MAX_PROBE_TIMEOUT_SECONDS: u64 = 300;

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
        #[arg(
            long,
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "true",
            value_parser = parse_view_only_presence
        )]
        view_only: Option<bool>,
        #[arg(long, value_enum, default_value_t = ViewerMode::Native)]
        viewer: ViewerMode,
    },
    Probe {
        selector: String,
        #[arg(
            long,
            default_value_t = DEFAULT_PROBE_TIMEOUT_SECONDS,
            value_parser = parse_probe_timeout_seconds
        )]
        timeout_seconds: u64,
        #[arg(long, value_parser = parse_resize_target)]
        resize: Option<DesktopSize>,
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
    view_only: Option<bool>,
    viewer: ViewerMode,
}

impl StartupRequest {
    pub fn selector(&self) -> &str {
        &self.selector
    }

    pub fn fullscreen(&self) -> bool {
        self.fullscreen
    }

    pub fn view_only(&self) -> Option<bool> {
        self.view_only
    }

    pub fn viewer(&self) -> ViewerMode {
        self.viewer
    }
}

fn parse_probe_timeout_seconds(value: &str) -> Result<u64, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "timeout must be an integer number of seconds".to_owned())?;
    if !(MIN_PROBE_TIMEOUT_SECONDS..=MAX_PROBE_TIMEOUT_SECONDS).contains(&seconds) {
        return Err(format!(
            "timeout must be between {MIN_PROBE_TIMEOUT_SECONDS} and {MAX_PROBE_TIMEOUT_SECONDS} seconds"
        ));
    }
    Ok(seconds)
}

fn parse_resize_target(value: &str) -> Result<DesktopSize, String> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| "resize must use WIDTHxHEIGHT".to_owned())?;
    let width = width
        .parse::<u32>()
        .map_err(|_| "resize width must be an integer".to_owned())?;
    let height = height
        .parse::<u32>()
        .map_err(|_| "resize height must be an integer".to_owned())?;
    normalize_resize_request(width, height, ProtocolLimits::default())
        .map_err(|_| "resize is outside the supported framebuffer limits".to_owned())
}

fn parse_view_only_presence(value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        _ => Err("--view-only is a presence-only flag".to_owned()),
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
            resize,
            json,
        } => execute_probe(runtime, output, &selector, timeout_seconds, resize, json).await,
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
    let operation = match checked_deadline(Instant::now(), LIST_TIMEOUT) {
        Some(deadline) => wait_for_live_inventory(&mut runtime, deadline)
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
            }),
        None => Err(PublicError::new(PublicErrorKind::Queue)),
    };
    let cleanup = runtime.shutdown().await;
    match compose_operation_cleanup(operation, cleanup) {
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
            Ok(None) => return Err(PublicError::new(PublicErrorKind::Queue)),
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
    resize: Option<DesktopSize>,
    json: bool,
) -> CliExit
where
    R: CliRuntime,
    W: Write,
{
    let timeout_duration = Duration::from_secs(timeout_seconds);
    let selection_deadline = checked_deadline(Instant::now(), timeout_duration);
    let selection = match selection_deadline {
        Some(deadline) => wait_for_live_inventory_for_probe(&mut runtime, deadline).await,
        None => Err(ProbeResult::Queue),
    };
    let (selected, mut observation) = match selection {
        Ok(snapshot) => match select_running(&snapshot, selector) {
            Ok(selected) => (
                Some(selected),
                observe_probe(&mut runtime, selected, timeout_duration, resize).await,
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
            Ok(None) => return Err(ProbeResult::Queue),
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
    resize_target: Option<DesktopSize>,
) -> ProbeObservation
where
    R: CliRuntime,
{
    let started = Instant::now();
    let Some(deadline) = checked_deadline(started, timeout_duration) else {
        return ProbeObservation {
            session_id: None,
            report: Err(ProbeResult::Queue),
        };
    };
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
    let mut resize_sent = false;
    let mut resize_retried = false;
    loop {
        let event = match timeout_at(deadline, runtime.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => {
                return ProbeObservation {
                    session_id,
                    report: Err(ProbeResult::Queue),
                }
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
                    if snapshot.phase == SessionPhase::Ready && !resize_sent {
                        if let Some(target) = resize_target {
                            if let Err(error) = runtime
                                .send(AppCommand::ViewportChanged {
                                    session_id: snapshot.session_id,
                                    backing_width: u32::from(target.width),
                                    backing_height: u32::from(target.height),
                                })
                                .await
                            {
                                return ProbeObservation {
                                    session_id,
                                    report: Err(error.kind().into()),
                                };
                            }
                            resize_sent = true;
                        }
                    }
                    if resize_target.is_some()
                        && resize_sent
                        && !resize_retried
                        && snapshot.resize_status == ResizeStatus::TimedOut
                    {
                        if let Err(error) = runtime
                            .send(AppCommand::RetryDynamicResolution {
                                session_id: snapshot.session_id,
                            })
                            .await
                        {
                            return ProbeObservation {
                                session_id,
                                report: Err(error.kind().into()),
                            };
                        }
                        resize_retried = true;
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
                if resize_target.is_some() && size != resize_target {
                    continue;
                }
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
    let deadline = checked_deadline(Instant::now(), CLEANUP_TIMEOUT)
        .ok_or_else(|| PublicError::new(PublicErrorKind::Cleanup))?;
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

fn checked_deadline(start: Instant, duration: Duration) -> Option<Instant> {
    start.checked_add(duration)
}

fn compose_operation_cleanup<T>(
    operation: Result<T, PublicError>,
    cleanup: Result<(), PublicError>,
) -> Result<T, PublicError> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(_)) => Err(primary.with_cleanup_failure()),
    }
}

#[cfg(test)]
mod tests {
    use super::checked_deadline;
    use std::time::Duration;
    use tokio::time::Instant;

    #[test]
    fn checked_deadline_rejects_unrepresentable_duration() {
        assert!(checked_deadline(Instant::now(), Duration::MAX).is_none());
    }
}
