//! Daemon socket sources for the Wayland event loop.

use super::*;

/// A client must send its tiny initial request promptly; this bounds idle IPC
/// file descriptors without delaying the Wayland event loop.
const IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Check request deadlines frequently enough to release stalled clients while
/// avoiding another high-frequency wakeup in the idle server.
const IPC_REQUEST_SWEEP: Duration = Duration::from_secs(1);
/// Prevent local connection floods from consuming the server's file-descriptor
/// budget while requests are waiting for their initial frame.
const MAX_PENDING_IPC_CLIENTS: usize = 64;
/// Bound listener work in one dispatch so a connection flood cannot starve
/// input, PTY, and rendering events already waiting in the loop.
const MAX_IPC_ACCEPTS_PER_WAKE: usize = 16;

/// Bind the daemon socket and register its listener and request-deadline
/// sources. The caller removes the returned path during shutdown.
pub(super) fn bind_server_socket(
  event_loop: &EventLoop<App>,
) -> anyhow::Result<PathBuf> {
  let (listener, path) = ipc::bind_listener().context("bind daemon socket")?;
  listener
    .set_nonblocking(true)
    .context("set socket non-blocking")?;

  let source = Generic::new(listener, Interest::READ, Mode::Level);
  event_loop
    .handle()
    .insert_source(source, |_, listener, app: &mut App| {
      // The listener is level-triggered. Bound this dispatch so a flood cannot
      // monopolize the shared Wayland/PTY event loop.
      for _ in 0..MAX_IPC_ACCEPTS_PER_WAKE {
        match listener.accept() {
          Ok((stream, _)) => app.register_ipc_client(stream),
          Err(err) if err.kind() == ErrorKind::WouldBlock => break,
          Err(err) => {
            tracing::warn!("accept client: {err}");
            break;
          },
        }
      }
      Ok(PostAction::Continue)
    })
    .map_err(|e| anyhow::anyhow!("register socket source: {e}"))?;

  event_loop
    .handle()
    .insert_source(
      Timer::from_duration(IPC_REQUEST_SWEEP),
      |_, _, app: &mut App| {
        app.expire_ipc_clients();
        TimeoutAction::ToDuration(IPC_REQUEST_SWEEP)
      },
    )
    .map_err(|e| anyhow::anyhow!("register IPC timeout source: {e}"))?;
  Ok(path)
}

impl App {
  /// Register one accepted daemon client without blocking the shared event
  /// loop. The stream source removes itself once the single request is ready.
  fn register_ipc_client(&mut self, stream: UnixStream) {
    if self.pending_ipc_clients.len() >= MAX_PENDING_IPC_CLIENTS {
      tracing::warn!("too many pending daemon clients");
      return;
    }
    if let Err(err) = stream.set_nonblocking(true) {
      tracing::warn!("set client socket non-blocking: {err}");
      return;
    }
    let id = self.next_ipc_client_id;
    self.next_ipc_client_id += 1;
    let mut reader = ipc::RequestReader::default();
    let source = Generic::new(stream, Interest::READ, Mode::Level);
    match self
      .loop_handle
      .insert_source(source, move |_, stream, app| {
        match reader.read_from(&**stream) {
          Ok(None) => Ok(PostAction::Continue),
          Ok(Some(req)) => {
            app.pending_ipc_clients.remove(&id);
            match stream.try_clone() {
              Ok(client) => {
                app.open_window(
                  req.cwd.map(PathBuf::from),
                  req.env,
                  Some(client),
                );
              },
              Err(err) => tracing::warn!("clone client socket: {err}"),
            }
            Ok(PostAction::Remove)
          },
          Err(err) => {
            app.pending_ipc_clients.remove(&id);
            tracing::warn!("bad client request: {err}");
            Ok(PostAction::Remove)
          },
        }
      }) {
      Ok(token) => {
        self
          .pending_ipc_clients
          .insert(id, (token, Instant::now() + IPC_REQUEST_TIMEOUT));
      },
      Err(err) => tracing::warn!("register client socket: {err}"),
    }
  }

  /// Drop clients that connected but did not finish their bounded request in
  /// time. Their sources own the streams, so removing the source closes them.
  fn expire_ipc_clients(&mut self) {
    let now = Instant::now();
    let expired: Vec<_> = self
      .pending_ipc_clients
      .iter()
      .filter_map(|(&id, &(token, deadline))| {
        (deadline <= now).then_some((id, token))
      })
      .collect();
    for (id, token) in expired {
      self.pending_ipc_clients.remove(&id);
      self.loop_handle.remove(token);
      tracing::warn!("daemon client request timed out");
    }
  }
}
