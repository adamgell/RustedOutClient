#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::Result;
use rustedoutclient::app::RustedOutClient;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RustedOutClient")
            .with_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    eframe::run_native(
        "RustedOutClient",
        native_options,
        Box::new(|cc| Ok(Box::new(RustedOutClient::new(cc)))),
    )
    .map_err(|error| anyhow::anyhow!("eframe error: {error}"))
}
