//! beer, a fast, software-rendered, Wayland-native terminal emulator.

mod bindings;
mod config;
mod font;
mod grid;
mod pty;
mod render;
mod theme;
mod vt;
mod wayland;

use std::path::PathBuf;
use std::process::ExitCode;

use pound::Parse;

use crate::config::Config;

/// A fast, software-rendered, Wayland-native terminal emulator.
#[derive(Parse)]
#[pound(name = "beer", version = "0.2.0")]
struct Cli {
    /// Run as a daemon hosting multiple windows.
    #[pound(long)]
    server: bool,
    /// Path to a config file (default: $XDG_CONFIG_HOME/beer/beer.toml).
    #[pound(long)]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    init_logging();
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("beer: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_env("BEER_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("warn"));

    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    if cli.server {
        anyhow::bail!("server mode is not implemented yet");
    }

    let config = Config::load(cli.config.as_deref());
    tracing::info!("starting beer");
    wayland::run(config, cli.config)
}
