//! Wayland front-end: connection, surface, and software-drawn window.
//!
//! Uses smithay-client-toolkit for protocol boilerplate and calloop for the
//! event loop, so the PTY master fd and timers share one loop.

use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use calloop_wayland_source::WaylandSource;

use crate::font::Fonts;
use crate::pty::Pty;
use crate::render::Renderer;
use crate::vt::Term;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_seat, delegate_shm,
    delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_shm, wl_surface},
};

/// Default window size in pixels before the compositor suggests one.
const DEFAULT_W: u32 = 800;
const DEFAULT_H: u32 = 600;
/// Primary font family and pixel size, resolved via fontconfig.
const FONT_FAMILY: &str = "monospace";
const FONT_SIZE_PX: u32 = 16;

/// Run a single window until it is closed, returning the shell's exit code.
pub fn run() -> anyhow::Result<ExitCode> {
    let conn = Connection::connect_to_env().context("connect to Wayland compositor")?;
    let (globals, event_queue) =
        registry_queue_init(&conn).context("initialize Wayland registry")?;
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().context("create calloop event loop")?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("insert Wayland source into event loop: {e}"))?;

    let compositor = CompositorState::bind(&globals, &qh).context("compositor not available")?;
    let xdg_shell = XdgShell::bind(&globals, &qh).context("xdg_wm_base not available")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("beer");
    window.set_app_id("dev.notashelf.beer");
    window.set_min_size(Some((1, 1)));
    // First commit with no buffer kicks off the initial configure.
    window.commit();

    let pool = SlotPool::new(DEFAULT_W as usize * DEFAULT_H as usize * 4, &shm)
        .context("create shm slot pool")?;

    let fonts = Fonts::new(FONT_FAMILY, FONT_SIZE_PX).context("load font")?;
    let renderer = Renderer::new(fonts);
    let (cols, rows) = grid_size(renderer.metrics(), DEFAULT_W, DEFAULT_H);

    let pty = Pty::spawn(cols, rows).context("spawn shell on pty")?;
    let term = Term::new(cols as usize, rows as usize);
    let mut parser = vte::Parser::new();

    // Read child output off a clone of the master; the original stays in `pty`
    // for writing input back.
    let read_fd = pty.master().try_clone().context("clone pty master")?;
    event_loop
        .handle()
        .insert_source(
            Generic::new(read_fd, Interest::READ, Mode::Level),
            move |_, fd, app: &mut App| {
                let mut buf = [0u8; 4096];
                match rustix::io::read(&*fd, &mut buf) {
                    Ok(0) => {
                        app.child_exited();
                        Ok(PostAction::Remove)
                    }
                    Ok(n) => {
                        parser.advance(&mut app.term, &buf[..n]);
                        app.after_feed();
                        Ok(PostAction::Continue)
                    }
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {
                        Ok(PostAction::Continue)
                    }
                    Err(_) => {
                        app.child_exited();
                        Ok(PostAction::Remove)
                    }
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("register pty in event loop: {e}"))?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        window,
        pty,
        term,
        renderer,
        title: None,
        width: DEFAULT_W,
        height: DEFAULT_H,
        dirty: false,
        exit: false,
        exit_code: ExitCode::SUCCESS,
    };

    while !app.exit {
        event_loop
            .dispatch(Duration::from_millis(16), &mut app)
            .context("dispatch event loop")?;
        if app.dirty {
            app.draw();
            app.dirty = false;
        }
    }
    Ok(app.exit_code)
}

/// Columns and rows that fit a `width`×`height` px window at `metrics`.
fn grid_size(metrics: crate::font::CellMetrics, width: u32, height: u32) -> (u16, u16) {
    let cols = (width / metrics.width).max(1);
    let rows = (height / metrics.height).max(1);
    (cols as u16, rows as u16)
}

/// Write every byte to `fd`, retrying short writes and interrupts.
fn write_all(fd: &OwnedFd, mut buf: &[u8]) -> rustix::io::Result<()> {
    while !buf.is_empty() {
        match rustix::io::write(fd, buf) {
            Ok(0) => return Err(rustix::io::Errno::IO),
            Ok(n) => buf = &buf[n..],
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Window + Wayland client state shared across all protocol handlers.
#[derive(Debug)]
struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    pool: SlotPool,
    window: Window,
    pty: Pty,
    term: Term,
    renderer: Renderer,
    /// Last title applied to the toplevel, to avoid redundant requests.
    title: Option<String>,
    width: u32,
    height: u32,
    /// The grid changed and the window needs repainting.
    dirty: bool,
    exit: bool,
    /// Exit code to return, taken from the shell when it exits.
    exit_code: ExitCode,
}

impl App {
    /// After parsing child output: send any replies, sync the title, repaint.
    fn after_feed(&mut self) {
        let reply = self.term.take_response();
        if !reply.is_empty()
            && let Err(err) = write_all(self.pty.master(), &reply)
        {
            tracing::warn!("write to pty: {err}");
        }

        if self.term.title() != self.title.as_deref() {
            self.title = self.term.title().map(str::to_owned);
            self.window
                .set_title(self.title.clone().unwrap_or_default());
        }
        self.dirty = true;
    }

    /// Recompute the grid size for the current window and tell the grid and the
    /// PTY about it if it changed.
    fn resize_grid(&mut self) {
        let (cols, rows) = grid_size(self.renderer.metrics(), self.width, self.height);
        if (cols as usize, rows as usize) == (self.term.grid().cols(), self.term.grid().rows()) {
            return;
        }
        self.term.resize(cols as usize, rows as usize);
        if let Err(err) = self.pty.resize(cols, rows) {
            tracing::warn!("resize pty: {err}");
        }
    }

    /// The child shell has gone away; reap it, capture its code, and tear the
    /// window down.
    fn child_exited(&mut self) {
        match self.pty.wait() {
            Ok(status) => {
                tracing::info!("shell exited: {status}");
                // Mirror the shell's status: its code, or 128+signal if killed.
                let code = status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
                self.exit_code = ExitCode::from(code as u8);
            }
            Err(err) => tracing::warn!("reap shell: {err}"),
        }
        self.exit = true;
    }

    /// Render the grid into a fresh buffer and present it.
    fn draw(&mut self) {
        let (w, h) = (self.width, self.height);
        let stride = w as i32 * 4;

        let (buffer, canvas) =
            match self
                .pool
                .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            {
                Ok(buf) => buf,
                Err(err) => {
                    tracing::error!("allocate shm buffer: {err}");
                    return;
                }
            };

        self.renderer
            .render(self.term.grid(), canvas, w as usize, h as usize);

        let surface = self.window.wl_surface();
        if let Err(err) = buffer.attach_to(surface) {
            tracing::error!("attach buffer: {err}");
            return;
        }
        surface.damage_buffer(0, 0, w as i32, h as i32);
        self.window.commit();
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        if let (Some(w), Some(h)) = configure.new_size {
            self.width = w.get();
            self.height = h.get();
        }
        self.resize_grid();
        self.draw();
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_registry!(App);
