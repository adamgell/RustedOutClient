#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{io, process::ExitCode};

use clap::Parser;
use rustedoutclient::{
    app::RustedOutClientApp,
    cli::{execute_headless, Cli, Command, ProductionCliRuntime, StartupRequest},
};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let command = Cli::parse().command;
    match command {
        Some(command @ (Command::List | Command::Probe { .. })) => run_headless(command).await,
        Some(command @ Command::Open { .. }) => {
            launch_gui(command.startup_request()).map_or(ExitCode::FAILURE, |()| ExitCode::SUCCESS)
        }
        None => launch_gui(None).map_or(ExitCode::FAILURE, |()| ExitCode::SUCCESS),
    }
}

async fn run_headless(command: Command) -> ExitCode {
    let runtime = match ProductionCliRuntime::load() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = io::stdout().lock();
    let exit = execute_headless(command, runtime, &mut stdout).await;
    if exit.success() {
        ExitCode::SUCCESS
    } else {
        if let Some(error) = exit.error() {
            eprintln!("{error}");
        }
        ExitCode::FAILURE
    }
}

fn launch_gui(startup_request: Option<StartupRequest>) -> Result<(), ()> {
    let fullscreen = startup_request
        .as_ref()
        .is_some_and(StartupRequest::fullscreen);
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RustedOutClient")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 640.0])
            .with_fullscreen(fullscreen),
        ..Default::default()
    };

    eframe::run_native(
        "RustedOutClient",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(RustedOutClientApp::new_with_startup(
                cc,
                startup_request,
            )))
        }),
    )
    .map_err(|_| {
        eprintln!("application window failed");
    })
}
