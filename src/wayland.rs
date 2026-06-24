//! Wayland front-end: connection, surface, and software-drawn window.
//!
//! Uses smithay-client-toolkit for protocol boilerplate and calloop for the
//! event loop, so the PTY master fd and timers share one loop.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitCode;

use anyhow::Context;
use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};
use calloop_wayland_source::WaylandSource;

use crate::font::Fonts;
use crate::pty::Pty;
use crate::render::Renderer;
use crate::vt::Term;
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    data_device_manager::{
        DataDeviceManagerState, WritePipe,
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{CopyPasteSource, DataSourceHandler},
    },
    delegate_compositor, delegate_data_device, delegate_keyboard, delegate_output,
    delegate_pointer, delegate_primary_selection, delegate_registry, delegate_seat, delegate_shm,
    delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    primary_selection::{
        PrimarySelectionManagerState,
        device::{PrimarySelectionDevice, PrimarySelectionDeviceHandler},
        selection::{PrimarySelectionSource, PrimarySelectionSourceHandler},
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers, RepeatInfo},
        pointer::{BTN_LEFT, BTN_MIDDLE, PointerEvent, PointerEventKind, PointerHandler},
    },
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
    protocol::{
        wl_data_device::WlDataDevice, wl_data_device_manager::DndAction,
        wl_data_source::WlDataSource, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm,
        wl_surface,
    },
};

/// MIME types beer offers and accepts for clipboard text.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "STRING",
    "text/plain",
    "TEXT",
];

/// Pick the first MIME type we understand from an offer's advertised set.
fn pick_mime(mimes: &[String]) -> Option<String> {
    mimes
        .iter()
        .find(|m| TEXT_MIMES.contains(&m.as_str()))
        .cloned()
}

/// Max gap between clicks counted as a multi-click (ms).
const MULTI_CLICK_MS: u32 = 400;

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
    let data_device_manager = DataDeviceManagerState::bind(&globals, &qh)
        .context("wl_data_device_manager not available")?;
    let primary_manager = PrimarySelectionManagerState::bind(&globals, &qh).ok();

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

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        pool,
        window,
        renderer,
        loop_handle: event_loop.handle(),
        qh: qh.clone(),
        data_device_manager,
        primary_manager,
        data_device: None,
        primary_device: None,
        copy_source: None,
        primary_source: None,
        clipboard: String::new(),
        primary_clip: String::new(),
        selecting: false,
        pointer_pos: (0.0, 0.0),
        last_click: None,
        serial: 0,
        keyboard: None,
        pointer: None,
        modifiers: Modifiers::default(),
        // The PTY is spawned on the first configure, once the real window size
        // is known, so the shell starts at the final size and is not hit by a
        // startup SIGWINCH storm that makes it reprint its prompt.
        session: None,
        title: None,
        width: DEFAULT_W,
        height: DEFAULT_H,
        dirty: false,
        focused: true,
        exit: false,
        exit_code: ExitCode::SUCCESS,
    };

    // Block until an event (PTY output, input, configure) arrives - every
    // repaint is driven by one, so there is nothing to do on a timer.
    while !app.exit {
        event_loop
            .dispatch(None, &mut app)
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

/// The per-window terminal: the PTY and the parsed screen behind it. Created on
/// the first configure, once the real size is known.
#[derive(Debug)]
struct Session {
    pty: Pty,
    term: Term,
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
    renderer: Renderer,
    loop_handle: LoopHandle<'static, App>,
    qh: QueueHandle<App>,
    data_device_manager: DataDeviceManagerState,
    primary_manager: Option<PrimarySelectionManagerState>,
    data_device: Option<DataDevice>,
    primary_device: Option<PrimarySelectionDevice>,
    /// Held while we own the clipboard / primary selection, serving paste reads.
    copy_source: Option<CopyPasteSource>,
    primary_source: Option<PrimarySelectionSource>,
    clipboard: String,
    primary_clip: String,
    /// A left-button drag is in progress.
    selecting: bool,
    pointer_pos: (f64, f64),
    /// Last click (time ms, abs row, col, count) for double/triple detection.
    last_click: Option<(u32, usize, usize, u32)>,
    /// Most recent input serial, used to claim selections.
    serial: u32,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    modifiers: Modifiers,
    /// `None` until the first configure spawns the shell.
    session: Option<Session>,
    /// Last title applied to the toplevel, to avoid redundant requests.
    title: Option<String>,
    width: u32,
    height: u32,
    /// The grid changed and the window needs repainting.
    dirty: bool,
    /// Whether the toplevel currently has keyboard focus (drives the cursor).
    focused: bool,
    exit: bool,
    /// Exit code to return, taken from the shell when it exits.
    exit_code: ExitCode,
}

impl App {
    /// Spawn the shell at the current window size and start reading its output.
    fn spawn_session(&mut self) {
        let (cols, rows) = grid_size(self.renderer.metrics(), self.width, self.height);
        let pty = match Pty::spawn(cols, rows) {
            Ok(pty) => pty,
            Err(err) => {
                tracing::error!("spawn shell: {err:#}");
                self.exit = true;
                return;
            }
        };
        let read_fd = match pty.master().try_clone() {
            Ok(fd) => fd,
            Err(err) => {
                tracing::error!("clone pty master: {err}");
                self.exit = true;
                return;
            }
        };

        let mut parser = vte::Parser::new();
        let source = Generic::new(read_fd, Interest::READ, Mode::Level);
        let registered = self
            .loop_handle
            .insert_source(source, move |_, fd, app: &mut App| {
                let mut buf = [0u8; 4096];
                let n = match rustix::io::read(&*fd, &mut buf) {
                    Ok(0) => {
                        app.child_exited();
                        return Ok(PostAction::Remove);
                    }
                    Ok(n) => n,
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {
                        return Ok(PostAction::Continue);
                    }
                    Err(_) => {
                        app.child_exited();
                        return Ok(PostAction::Remove);
                    }
                };
                if let Some(session) = app.session.as_mut() {
                    parser.advance(&mut session.term, &buf[..n]);
                }
                app.after_feed();
                Ok(PostAction::Continue)
            });
        if let Err(err) = registered {
            tracing::error!("register pty in event loop: {err}");
            self.exit = true;
            return;
        }

        self.session = Some(Session {
            pty,
            term: Term::new(cols as usize, rows as usize),
        });
    }

    /// Handle a key (initial press or repeat): Shift+PageUp/PageDown scroll the
    /// viewport locally; anything else is encoded to the shell and snaps the
    /// viewport back to the live screen.
    fn handle_key(&mut self, event: &KeyEvent) {
        // Ctrl+Shift+C/V copy the selection and paste the clipboard; these take
        // precedence over the control bytes the chord would otherwise encode.
        if self.modifiers.ctrl && self.modifiers.shift {
            match event.keysym {
                Keysym::C | Keysym::c => {
                    let qh = self.qh.clone();
                    self.set_clipboard(&qh);
                    return;
                }
                Keysym::V | Keysym::v => {
                    self.paste_clipboard();
                    return;
                }
                _ => {}
            }
        }
        if self.modifiers.shift && matches!(event.keysym, Keysym::Page_Up | Keysym::Page_Down) {
            if let Some(session) = self.session.as_mut() {
                let page = session.term.page() as isize;
                let delta = if event.keysym == Keysym::Page_Up {
                    page
                } else {
                    -page
                };
                session.term.scroll_view(delta);
                self.dirty = true;
            }
            return;
        }

        let app_cursor = self
            .session
            .as_ref()
            .is_some_and(|s| s.term.grid().app_cursor());
        if let Some(bytes) = crate::input::encode(event, self.modifiers, app_cursor)
            && let Some(session) = self.session.as_mut()
        {
            session.term.scroll_to_bottom();
            session.term.grid_mut().clear_selection();
            self.dirty = true;
            if let Err(err) = write_all(session.pty.master(), &bytes) {
                tracing::warn!("write key to pty: {err}");
            }
        }
    }

    /// Map window pixel coordinates to an absolute `(row, col)` grid point.
    fn cell_at(&self, px: f64, py: f64) -> Option<(usize, usize)> {
        let session = self.session.as_ref()?;
        let m = self.renderer.metrics();
        let grid = session.term.grid();
        let col = (px.max(0.0) as usize / m.width as usize).min(grid.cols().saturating_sub(1));
        let vrow = (py.max(0.0) as usize / m.height as usize).min(grid.rows().saturating_sub(1));
        Some((grid.view_to_abs(vrow), col))
    }

    /// Left-button press: start (or word/line-extend) a selection.
    fn pointer_press(&mut self, time: u32) {
        let Some((row, col)) = self.cell_at(self.pointer_pos.0, self.pointer_pos.1) else {
            return;
        };
        let count = match self.last_click {
            Some((t, r, c, n))
                if time.wrapping_sub(t) <= MULTI_CLICK_MS && r == row && c == col =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((time, row, col, count));
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let grid = session.term.grid_mut();
        match count {
            2 => grid.select_word(row, col),
            3 => grid.select_line(row),
            _ => grid.start_selection(row, col),
        }
        self.selecting = true;
        self.dirty = true;
    }

    /// Pointer motion during a drag: extend the selection head.
    fn pointer_drag(&mut self) {
        if !self.selecting {
            return;
        }
        let Some((row, col)) = self.cell_at(self.pointer_pos.0, self.pointer_pos.1) else {
            return;
        };
        if let Some(session) = self.session.as_mut() {
            session.term.grid_mut().extend_selection(row, col);
            self.dirty = true;
        }
    }

    /// Left-button release: a completed selection becomes the primary selection.
    fn pointer_release(&mut self, qh: &QueueHandle<App>) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        self.set_primary(qh);
    }

    /// The current selection text, if any and non-empty.
    fn selection_text(&self) -> Option<String> {
        let text = self.session.as_ref()?.term.grid().selection_text()?;
        (!text.is_empty()).then_some(text)
    }

    /// Claim the clipboard (CLIPBOARD) with the current selection (Ctrl+Shift+C).
    fn set_clipboard(&mut self, qh: &QueueHandle<App>) {
        let Some(text) = self.selection_text() else {
            return;
        };
        let Some(device) = self.data_device.as_ref() else {
            return;
        };
        let source = self
            .data_device_manager
            .create_copy_paste_source(qh, TEXT_MIMES.iter().copied());
        source.set_selection(device, self.serial);
        self.clipboard = text;
        self.copy_source = Some(source);
    }

    /// Claim the primary selection with the current selection (select-to-copy).
    fn set_primary(&mut self, qh: &QueueHandle<App>) {
        let Some(text) = self.selection_text() else {
            return;
        };
        let (Some(manager), Some(device)) =
            (self.primary_manager.as_ref(), self.primary_device.as_ref())
        else {
            return;
        };
        let source = manager.create_selection_source(qh, TEXT_MIMES.iter().copied());
        source.set_selection(device, self.serial);
        self.primary_clip = text;
        self.primary_source = Some(source);
    }

    /// Paste the CLIPBOARD selection into the shell (Ctrl+Shift+V).
    fn paste_clipboard(&mut self) {
        let Some(offer) = self
            .data_device
            .as_ref()
            .and_then(|d| d.data().selection_offer())
        else {
            return;
        };
        let Some(mime) = offer.with_mime_types(pick_mime) else {
            return;
        };
        if let Ok(pipe) = offer.receive(mime) {
            self.read_paste(pipe);
        }
    }

    /// Paste the primary selection into the shell (middle click).
    fn paste_primary(&mut self) {
        let Some(offer) = self
            .primary_device
            .as_ref()
            .and_then(|d| d.data().selection_offer())
        else {
            return;
        };
        let Some(mime) = offer.with_mime_types(pick_mime) else {
            return;
        };
        if let Ok(pipe) = offer.receive(mime) {
            self.read_paste(pipe);
        }
    }

    /// Drain a clipboard read-pipe on the event loop, writing the bytes to the
    /// PTY once the source closes its end.
    fn read_paste(&mut self, pipe: smithay_client_toolkit::data_device_manager::ReadPipe) {
        let mut data: Vec<u8> = Vec::new();
        let registered = self
            .loop_handle
            .insert_source(pipe, move |_, file, app: &mut App| {
                // SAFETY: the file is owned by the source and not closed while read.
                let f: &mut File = unsafe { file.get_mut() };
                let mut tmp = [0u8; 4096];
                match f.read(&mut tmp) {
                    Ok(0) => {
                        app.paste_bytes(&data);
                        PostAction::Remove
                    }
                    Ok(n) => {
                        data.extend_from_slice(&tmp[..n]);
                        PostAction::Continue
                    }
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::Interrupted) => {
                        PostAction::Continue
                    }
                    Err(e) => {
                        tracing::warn!("read paste pipe: {e}");
                        PostAction::Remove
                    }
                }
            });
        if let Err(err) = registered {
            tracing::warn!("register paste pipe: {err}");
        }
    }

    /// Write pasted bytes to the PTY, framing them for bracketed-paste mode and
    /// snapping the viewport to the live screen.
    fn paste_bytes(&mut self, data: &[u8]) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        session.term.scroll_to_bottom();
        self.dirty = true;
        let bracketed = session.term.grid().bracketed_paste();
        // Strip control bytes a terminal must never receive raw from a paste;
        // keep tab and newlines (CR is what the shell expects for Enter).
        let mut clean: Vec<u8> = Vec::with_capacity(data.len());
        for &b in data {
            match b {
                b'\n' => clean.push(b'\r'),
                b'\t' | b'\r' => clean.push(b),
                0x20..=0xff => clean.push(b),
                _ => {}
            }
        }
        let fd = session.pty.master();
        if bracketed {
            let _ = write_all(fd, b"\x1b[200~");
            let _ = write_all(fd, &clean);
            let _ = write_all(fd, b"\x1b[201~");
        } else if let Err(err) = write_all(fd, &clean) {
            tracing::warn!("write paste to pty: {err}");
        }
    }

    /// After parsing child output: send any replies, sync the title, repaint.
    fn after_feed(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let reply = session.term.take_response();
        if !reply.is_empty()
            && let Err(err) = write_all(session.pty.master(), &reply)
        {
            tracing::warn!("write to pty: {err}");
        }

        let new_title = session.term.title().map(str::to_owned);
        if new_title.as_deref() != self.title.as_deref() {
            self.title = new_title;
            self.window
                .set_title(self.title.clone().unwrap_or_default());
        }
        self.dirty = true;
    }

    /// Recompute the grid size for the current window and tell the grid and the
    /// PTY about it if it changed.
    fn resize_grid(&mut self) {
        let (cols, rows) = grid_size(self.renderer.metrics(), self.width, self.height);
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if (cols as usize, rows as usize)
            == (session.term.grid().cols(), session.term.grid().rows())
        {
            return;
        }
        session.term.resize(cols as usize, rows as usize);
        if let Err(err) = session.pty.resize(cols, rows) {
            tracing::warn!("resize pty: {err}");
        }
    }

    /// The child shell has gone away; reap it, capture its code, and tear the
    /// window down.
    fn child_exited(&mut self) {
        if let Some(session) = self.session.as_mut() {
            match session.pty.wait() {
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
        }
        self.exit = true;
    }

    /// Render the grid into a fresh buffer and present it.
    fn draw(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
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

        self.renderer.render(
            session.term.grid(),
            canvas,
            w as usize,
            h as usize,
            self.focused,
        );

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
        self.focused = configure.is_activated();
        if self.session.is_none() {
            self.spawn_session();
        } else {
            self.resize_grid();
        }
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
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if self.data_device.is_none() {
            self.data_device = Some(self.data_device_manager.get_data_device(qh, &seat));
        }
        if self.primary_device.is_none() {
            self.primary_device = self
                .primary_manager
                .as_ref()
                .map(|m| m.get_selection_device(qh, &seat));
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            // get_keyboard_with_repeat drives key repeat off a calloop timer and
            // delivers each repeat through the callback.
            let loop_handle = self.loop_handle.clone();
            let keyboard = self.seat_state.get_keyboard_with_repeat(
                qh,
                &seat,
                None,
                loop_handle,
                Box::new(|app: &mut App, _kbd, event| app.handle_key(&event)),
            );
            match keyboard {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                Err(err) => tracing::warn!("get keyboard: {err}"),
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(err) => tracing::warn!("get pointer: {err}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                if let Some(keyboard) = self.keyboard.take() {
                    keyboard.release();
                }
            }
            Capability::Pointer => {
                if let Some(pointer) = self.pointer.take() {
                    pointer.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.serial = serial;
        self.focused = true;
        self.dirty = true;
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.focused = false;
        self.dirty = true;
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.serial = serial;
        self.handle_key(&event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
        // Repeats are delivered through the get_keyboard_with_repeat callback;
        // this non-calloop hook is unused.
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: RepeatInfo,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        let cell_h = f64::from(self.renderer.metrics().height);
        for event in events {
            match &event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.pointer_pos = event.position;
                    self.pointer_drag();
                }
                PointerEventKind::Press {
                    button,
                    serial,
                    time,
                    ..
                } => {
                    self.serial = *serial;
                    self.pointer_pos = event.position;
                    match *button {
                        BTN_LEFT => self.pointer_press(*time),
                        BTN_MIDDLE => self.paste_primary(),
                        _ => {}
                    }
                }
                PointerEventKind::Release { button, .. } if *button == BTN_LEFT => {
                    self.pointer_release(qh);
                }
                PointerEventKind::Axis { vertical, .. } => {
                    // Wheel notches arrive as value120 (÷120) or legacy discrete
                    // steps, ~3 lines each; touchpads send pixels per cell height.
                    let (raw, scale) = if vertical.value120 != 0 {
                        (f64::from(vertical.value120) / 120.0, 3.0)
                    } else if vertical.discrete != 0 {
                        (f64::from(vertical.discrete), 3.0)
                    } else if cell_h > 0.0 {
                        (vertical.absolute / cell_h, 1.0)
                    } else {
                        continue;
                    };
                    if raw == 0.0 {
                        continue;
                    }
                    // Positive axis = scroll down (toward live); the viewport
                    // scrolls the opposite way (negative offset delta).
                    let lines = (raw.abs() * scale).ceil().max(1.0) as isize;
                    let delta = if raw < 0.0 { lines } else { -lines };
                    if let Some(session) = self.session.as_mut() {
                        session.term.scroll_view(delta);
                        self.dirty = true;
                    }
                }
                _ => {}
            }
        }
    }
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

/// Serve the held clipboard text when a paste target requests it.
fn serve(text: &str, fd: WritePipe) {
    let mut file = File::from(OwnedFd::from(fd));
    let _ = file.write_all(text.as_bytes());
}

impl DataDeviceHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &wl_surface::WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for App {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl DataSourceHandler for App {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        _mime: String,
        fd: WritePipe,
    ) {
        if self
            .copy_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            serve(&self.clipboard, fd);
        }
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if self
            .copy_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            self.copy_source = None;
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl PrimarySelectionDeviceHandler for App {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionDeviceV1,
    ) {
    }
}

impl PrimarySelectionSourceHandler for App {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        _mime: String,
        fd: WritePipe,
    ) {
        if self
            .primary_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            serve(&self.primary_clip, fd);
        }
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        if self
            .primary_source
            .as_ref()
            .is_some_and(|s| s.inner() == source)
        {
            self.primary_source = None;
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_data_device!(App);
delegate_primary_selection!(App);
delegate_registry!(App);
