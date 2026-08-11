//! Backend entry point: connect to the compositor, bind globals, build the
//! [`WaylandState`], let the app start (arm timers, bind the IPC socket, open
//! the first window), then run the calloop loop until the app exits.

use std::collections::HashMap;

use anyhow::Context as _;
use beer_window::{App, WindowId};
use calloop::{
  EventLoop,
  signals::{Signal, Signals},
};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
  activation::ActivationState,
  compositor::CompositorState,
  data_device_manager::DataDeviceManagerState,
  output::OutputState,
  primary_selection::PrimarySelectionManagerState,
  registry::RegistryState,
  seat::{SeatState, pointer::cursor_shape::CursorShapeManager},
  shell::xdg::XdgShell,
  shm::Shm,
};
use wayland_client::{
  Connection,
  Dispatch,
  Proxy,
  QueueHandle,
  globals::{GlobalList, registry_queue_init},
};
use wayland_protocols::wp::{
  content_type::v1::client::wp_content_type_manager_v1::WpContentTypeManagerV1,
  fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
  idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
  text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3,
  viewporter::client::wp_viewporter::WpViewporter,
};

use crate::state::{Platform, WaylandState};

/// Bind a singleton global at version 1 with `()` user-data, or `None` if the
/// compositor does not advertise it.
fn bind_global<I>(
  globals: &GlobalList,
  qh: &QueueHandle<WaylandState>,
) -> Option<I>
where
  I: Proxy + 'static,
  WaylandState: Dispatch<I, ()>,
{
  match globals.bind(qh, 1..=1, ()) {
    Ok(global) => Some(global),
    Err(err) => {
      tracing::debug!("bind {}: {err}", I::interface().name);
      None
    },
  }
}

/// Run the Wayland backend, driving `app` until it exits; returns the process
/// exit status the app requested via [`beer_window::WindowCtx::exit`].
///
/// # Errors
///
/// Fails if the compositor connection, registry, required globals, or the event
/// loop cannot be initialised.
pub fn run(app: Box<dyn App>) -> anyhow::Result<u8> {
  let conn =
    Connection::connect_to_env().context("connect to Wayland compositor")?;
  let (globals, event_queue) =
    registry_queue_init(&conn).context("initialize Wayland registry")?;
  let qh = event_queue.handle();

  let mut event_loop: EventLoop<WaylandState> =
    EventLoop::try_new().context("create calloop event loop")?;
  WaylandSource::new(conn, event_queue)
    .insert(event_loop.handle())
    .map_err(|e| anyhow::anyhow!("insert Wayland source: {e}"))?;

  let compositor =
    CompositorState::bind(&globals, &qh).context("compositor not available")?;
  let xdg_shell =
    XdgShell::bind(&globals, &qh).context("xdg_wm_base not available")?;
  let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;
  let data_device_manager = DataDeviceManagerState::bind(&globals, &qh)
    .context("wl_data_device_manager not available")?;
  let primary_manager = PrimarySelectionManagerState::bind(&globals, &qh).ok();
  let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).ok();
  let viewporter = bind_global::<WpViewporter>(&globals, &qh);
  let fractional_manager =
    bind_global::<WpFractionalScaleManagerV1>(&globals, &qh);
  let content_type_manager =
    bind_global::<WpContentTypeManagerV1>(&globals, &qh);
  let text_input_manager = bind_global::<ZwpTextInputManagerV3>(&globals, &qh);
  let activation = ActivationState::bind(&globals, &qh).ok();
  let idle_inhibit_manager =
    bind_global::<ZwpIdleInhibitManagerV1>(&globals, &qh);

  let plat = Platform {
    registry_state: RegistryState::new(&globals),
    output_state: OutputState::new(&globals, &qh),
    seat_state: SeatState::new(&globals, &qh),
    shm,
    loop_handle: event_loop.handle(),
    qh: qh.clone(),
    compositor,
    xdg_shell,
    viewporter,
    fractional_manager,
    content_type_manager,
    data_device_manager,
    primary_manager,
    cursor_shape_manager,
    text_input_manager,
    activation,
    idle_inhibit_manager,
    seats: Vec::new(),
    active_seat: 0,
    copy_source: None,
    primary_source: None,
    clipboard: String::new(),
    primary_clip: String::new(),
    serial: 0,
    windows: Vec::new(),
    focused_window: 0,
    touch_focus: HashMap::new(),
    next_window_id: 1,
    sources: HashMap::new(),
    exit: false,
    exit_code: 0,
  };
  let mut state = WaylandState { app, plat };

  // The app arms its timers, begins serving the IPC socket (server mode), and
  // opens the initial window (standalone).
  {
    let WaylandState { app, plat } = &mut state;
    app.start(plat);
  }

  // SIGUSR1 reloads the config in place; the termination signals unwind the
  // loop cleanly so the app's Drop runs (e.g. unlinking the daemon socket).
  let signals = Signals::new(&[
    Signal::SIGUSR1,
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGHUP,
  ]);
  match signals {
    Ok(signals) => {
      let reg = event_loop.handle().insert_source(
        signals,
        |event, (), state: &mut WaylandState| {
          match event.signal() {
            Signal::SIGUSR1 => {
              let WaylandState { app, plat } = state;
              app.on_reload(plat);
            },
            _ => state.plat.exit = true,
          }
        },
      );
      if let Err(err) = reg {
        tracing::warn!("register signal source: {err}");
      }
    },
    Err(err) => tracing::warn!("install signal handlers: {err}"),
  }

  // Each iteration blocks until an event arrives, then repaints any window the
  // app has marked and that is not waiting on a frame callback.
  while !state.plat.exit {
    event_loop
      .dispatch(None, &mut state)
      .context("dispatch event loop")?;
    let ids: Vec<WindowId> = state
      .plat
      .windows
      .iter()
      .filter(|w| w.wants_draw && !w.frame_pending)
      .map(|w| w.id)
      .collect();
    for id in ids {
      let WaylandState { app, plat } = &mut state;
      if let Some(i) = plat.window_index(id) {
        plat.windows[i].wants_draw = false;
      }
      app.render(plat, id);
    }
  }
  Ok(state.plat.exit_code)
}
