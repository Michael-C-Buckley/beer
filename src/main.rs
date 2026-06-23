//! beer, a fast, software-rendered, Wayland-native terminal emulator.

mod grid;
mod pty;
mod vt;
mod wayland;

use std::process::ExitCode;

use pound::Parse;

/// A fast, software-rendered, Wayland-native terminal emulator.
#[derive(Parse)]
#[pound(name = "beer", version = "0.0.0")]
struct Cli {
    /// Run as a daemon hosting multiple windows.
    #[pound(long)]
    server: bool,
}

fn main() -> ExitCode {
    init_logging();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
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

fn run(cli: Cli) -> anyhow::Result<()> {
    if cli.server {
        tracing::info!("starting beer server");
        todo!("server mode")
    }

    tracing::info!("starting beer");
    wayland::run()
}
