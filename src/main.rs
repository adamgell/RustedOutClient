#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::Result;
use clap::Parser;
use rustedoutclient::app::RustedOutClientApp;
use rustedoutclient::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    if Cli::parse().command.is_some() {
        anyhow::bail!("CLI commands are not wired until the session manager is available")
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RustedOutClient")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "RustedOutClient",
        native_options,
        Box::new(|cc| Ok(Box::new(RustedOutClientApp::new(cc)))),
    )
    .map_err(|error| anyhow::anyhow!("eframe error: {error}"))
}
