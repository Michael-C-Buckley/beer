//! beer, a fast, software-rendered, Wayland-native terminal emulator.

mod app;
mod bindings;
mod config;
mod font;
mod graphics;
mod grid;
mod pty;
mod render;
mod theme;
mod vt;

use std::{env, io, path::PathBuf, process::ExitCode};

use beer_ipc as ipc;
use pound::Parse;

use crate::config::Config;

/// A fast, software-rendered, Wayland-native terminal emulator.
#[derive(Parse)]
#[pound(name = "beer")]
struct Cli {
  /// Run as a daemon hosting multiple windows.
  #[pound(long)]
  server:            bool,
  /// Always run a private window, never connect to a running server.
  #[pound(long)]
  no_daemon:         bool,
  /// Path to a config file (default: $XDG_CONFIG_HOME/beer/beer.toml).
  /// Repeatable; later files take priority over earlier ones.
  #[pound(long)]
  config:            Vec<PathBuf>,
  /// Initial title for the new window.
  #[pound(long)]
  title:             Option<String>,
  /// Wayland app_id for the new window.
  #[pound(long)]
  app_id:            Option<String>,
  /// Start in this directory instead of the current directory.
  #[pound(long)]
  working_directory: Option<PathBuf>,
  /// Keep the window open after the command exits.
  #[pound(long)]
  hold:              bool,
  /// Command and arguments to execute instead of the login shell.
  #[pound(trailing)]
  command:           Vec<String>,
}

fn main() -> ExitCode {
  init_logging();
  match run(Cli::parse()) {
    Ok(code) => code,
    Err(err) => {
      tracing::error!("{err:#}");
      ExitCode::FAILURE
    },
  }
}

fn init_logging() {
  use tracing_subscriber::{EnvFilter, fmt};

  let filter = EnvFilter::try_from_env("BEER_LOG")
    .or_else(|_| EnvFilter::try_from_default_env())
    .unwrap_or_else(|_| EnvFilter::new("warn"));

  fmt().with_env_filter(filter).with_writer(io::stderr).init();
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
  // A plain `beer` prefers a running server: forward the request and mirror the
  // window's exit status. `--no-daemon` and `--server` opt out.
  if !cli.server && !cli.no_daemon {
    let req = ipc::OpenRequest {
      cwd:     cli
        .working_directory
        .clone()
        .or_else(|| env::current_dir().ok())
        .map(|p| p.to_string_lossy().into_owned()),
      env:     env::vars().collect(),
      title:   cli.title.clone(),
      app_id:  cli.app_id.clone(),
      command: cli.command.clone(),
      hold:    cli.hold,
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
  let initial = ipc::OpenRequest {
    cwd:     cli
      .working_directory
      .or_else(|| env::current_dir().ok())
      .map(|p| p.to_string_lossy().into_owned()),
    env:     env::vars().collect(),
    title:   cli.title,
    app_id:  cli.app_id,
    command: cli.command,
    hold:    cli.hold,
  };
  let app = app::App::new(config, paths, cli.server, initial)?;
  let code = beer_wayland::run(Box::new(app))?;
  Ok(ExitCode::from(code))
}

/// Merge `--config` paths with `$BEER_CONFIG` (the env paths rank lower).
fn config_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
  if let Some(config_env) = env::var_os("BEER_CONFIG").filter(|s| !s.is_empty())
  {
    let extra: Vec<PathBuf> = env::split_paths(&config_env).collect();
    if extra.is_empty() {
      paths.push(PathBuf::from(&config_env));
    }
    let mut combined = extra;
    combined.append(&mut paths);
    paths = combined;
  }
  paths
}
