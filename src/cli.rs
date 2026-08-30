use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "rustedoutclient", about = "RustedOutClient Proxmox console")]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ViewerMode {
    Native,
    #[value(name = "tiger-vnc")]
    TigerVnc,
}
