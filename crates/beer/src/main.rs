//! beer, a fast, software-rendered, Wayland-native terminal emulator.

mod bindings;
mod config;
mod font;
mod graphics;
mod grid;
mod ipc;
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
#[pound(name = "beer")]
struct Cli {
    /// Run as a daemon hosting multiple windows.
    #[pound(long)]
    server: bool,
    /// Always run a private window, never connect to a running server.
    #[pound(long)]
    no_daemon: bool,
    /// Path to a config file (default: $XDG_CONFIG_HOME/beer/beer.toml).
    /// Repeatable; later files take priority over earlier ones.
    #[pound(long)]
    config: Vec<PathBuf>,
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
    // A plain `beer` prefers a running server: forward the request and mirror the
    // window's exit status. `--no-daemon` and `--server` opt out.
    if !cli.server && !cli.no_daemon {
        let req = ipc::OpenRequest {
            cwd: std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
            env: std::env::vars().collect(),
        };
        match ipc::run_client(&req) {
            Ok(code) => return Ok(ExitCode::from(code)),
            // No server (or it went away mid-handshake): run our own window.
            Err(err) => tracing::debug!("no server, running standalone: {err}"),
        }
    }

    let paths = config_paths(cli.config);
    let config = Config::load(&paths);
    tracing::info!(server = cli.server, "starting beer");
    wayland::run(config, paths, cli.server)
}

/// Merge `--config` paths with `$BEER_CONFIG` (the env paths rank lower).
fn config_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    if let Some(env) = std::env::var_os("BEER_CONFIG").filter(|s| !s.is_empty()) {
        let extra: Vec<PathBuf> = std::env::split_paths(&env).collect();
        if extra.is_empty() {
            paths.push(PathBuf::from(&env));
        }
        let mut combined = extra;
        combined.append(&mut paths);
        paths = combined;
    }
    paths
}
