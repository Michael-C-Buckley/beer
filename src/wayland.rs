//! Wayland front-end: connection, surface, and software-drawn window.
//!
//! Uses smithay-client-toolkit for protocol boilerplate and calloop for the
//! event loop, so the PTY master fd and timers share one loop.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitCode;

use std::time::Duration;

use anyhow::Context;
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use calloop_wayland_source::WaylandSource;

use crate::config::Config;
use crate::font::Fonts;
use crate::grid::{Cell, CursorShape, Grid, MouseProtocol};
use crate::pty::Pty;
use crate::render::Renderer;
use crate::vt::Term;
use smithay_client_toolkit::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape, WpCursorShapeDeviceV1,
};
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
        pointer::{
            BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, PointerEvent, PointerEventKind, PointerHandler,
            cursor_shape::CursorShapeManager,
        },
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::{GlobalList, registry_queue_init},
    protocol::{
        wl_data_device::WlDataDevice, wl_data_device_manager::DndAction,
        wl_data_source::WlDataSource, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm,
        wl_surface,
    },
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
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

/// Blink half-period: cells/cursor toggle visibility this often.
const BLINK_MS: u64 = 500;

/// Buffers kept for double/triple buffering before we wait for a release.
const MAX_BUFFERS: usize = 3;

/// How long synchronized output (DECSET 2026) may hold the screen before we
/// present anyway, so a misbehaving app cannot freeze the window.
const SYNC_TIMEOUT_MS: u64 = 150;

/// Interval between autoscroll steps while a drag selection runs off an edge.
const AUTOSCROLL_MS: u64 = 40;

/// How long the visual bell inverts the screen.
const FLASH_MS: u64 = 80;

/// What determines one rendered row's pixels: its cells, the cursor on it, the
/// selection span over it, and the blink phase. Two equal `RowSnap`s render
/// identically, so a buffer holding an equal snapshot needs no repaint.
#[derive(Clone, PartialEq, Debug)]
struct RowSnap {
    cells: Vec<Cell>,
    /// `(col, shape, focused)` when the cursor is drawn on this row.
    cursor: Option<(usize, CursorShape, bool)>,
    /// Inclusive selected column span on this row.
    sel: Option<(usize, usize)>,
    /// Search-match spans `(lo, hi, is_current)` highlighted on this row.
    search: Vec<(usize, usize, bool)>,
    /// Search-prompt text drawn over this row (only the bottom row, when active).
    overlay: Option<String>,
    /// Blink phase, but only varied when the row actually has blinking ink, so
    /// non-blinking rows stay equal across phase toggles.
    blink: bool,
}

/// One shm buffer plus the per-row snapshot of what it currently displays.
#[derive(Debug)]
struct FrameBuf {
    buffer: Buffer,
    rows: Vec<RowSnap>,
}

/// Snapshot the determinants of viewport row `y`'s pixels.
fn row_snap(grid: &Grid, y: usize, focused: bool, blink_on: bool) -> RowSnap {
    let abs = grid.view_to_abs(y);
    let cells = grid.view_row(y).to_vec();
    let cursor = if grid.view_at_bottom() && grid.cursor().1 == y {
        let visible = grid.cursor_visible() && (!grid.cursor_blink() || blink_on);
        visible.then(|| (grid.cursor().0, grid.cursor_shape(), focused))
    } else {
        None
    };
    let has_blink = cells
        .iter()
        .any(|c| c.flags.contains(crate::grid::Flags::BLINK));
    RowSnap {
        cells,
        cursor,
        sel: grid.selection_span_on(abs),
        search: grid.search_spans_on(abs),
        overlay: None,
        blink: if has_blink { blink_on } else { true },
    }
}

/// Fallback window size in pixels if the configured geometry yields nothing.
const DEFAULT_W: u32 = 800;
const DEFAULT_H: u32 = 600;

/// Run a single window until it is closed, returning the shell's exit code.
pub fn run(config: Config, config_path: Option<std::path::PathBuf>) -> anyhow::Result<ExitCode> {
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
    let cursor_shape_manager = CursorShapeManager::bind(&globals, &qh).ok();

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("beer");
    window.set_app_id("dev.notashelf.beer");
    window.set_min_size(Some((1, 1)));

    // Decorrelate buffer pixels from surface size so we can render at the
    // compositor's preferred fractional scale (crisp glyphs on a 150% output)
    // and present at the logical size. Both are optional; without them the
    // window falls back to integer buffer scaling via `scale_factor_changed`.
    let viewport = bind_global::<WpViewporter>(&globals, &qh)
        .map(|vp| vp.get_viewport(window.wl_surface(), &qh, ()));
    // Fractional scaling needs a viewport to present the scaled buffer back at
    // the logical size; without one we can only do integer buffer scaling.
    let fractional_scale = viewport.as_ref().and_then(|_| {
        bind_global::<WpFractionalScaleManagerV1>(&globals, &qh)
            .map(|mgr| mgr.get_fractional_scale(window.wl_surface(), &qh, ()))
    });

    // First commit with no buffer kicks off the initial configure.
    window.commit();

    let fonts = Fonts::new(&config.main.font, config.main.font_size).context("load font")?;
    let mut renderer = Renderer::new(fonts);
    renderer.set_padding(config.main.pad_x, config.main.pad_y);

    // Start at the configured cell geometry plus padding; the compositor may
    // override it on the first configure.
    let m = renderer.metrics();
    let width = (u32::from(config.main.initial_cols) * m.width + 2 * config.main.pad_x).max(1);
    let height = (u32::from(config.main.initial_rows) * m.height + 2 * config.main.pad_y).max(1);
    let pool = SlotPool::new(
        (width * height * 4).max(DEFAULT_W * DEFAULT_H) as usize,
        &shm,
    )
    .context("create shm slot pool")?;

    let bindings =
        crate::bindings::Bindings::from_config(&config.key_bindings, &config.text_bindings);
    let font_size = config.main.font_size;

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
        cursor_shape_manager,
        cursor_shape_device: None,
        viewport,
        fractional_scale,
        scale120: 120,
        data_device: None,
        primary_device: None,
        copy_source: None,
        primary_source: None,
        clipboard: String::new(),
        primary_clip: String::new(),
        selecting: false,
        pressed_button: None,
        last_report_cell: None,
        autoscroll: 0,
        autoscroll_timer: None,
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
        config,
        config_path,
        bindings,
        font_size,
        fullscreen: false,
        width,
        height,
        needs_draw: false,
        frame_pending: false,
        frames: Vec::new(),
        buf_dims: (0, 0),
        blink_on: true,
        sync_timeout: None,
        flashing: false,
        flash_timer: None,
        searching: false,
        focused: true,
        exit: false,
        exit_code: ExitCode::SUCCESS,
    };

    // Toggle the blink phase on a timer so blinking text and cursors animate.
    let blink = Timer::from_duration(Duration::from_millis(BLINK_MS));
    let blink_registered = event_loop
        .handle()
        .insert_source(blink, |_, _, app: &mut App| {
            app.blink_on = !app.blink_on;
            app.needs_draw = true;
            TimeoutAction::ToDuration(Duration::from_millis(BLINK_MS))
        });
    if let Err(err) = blink_registered {
        tracing::warn!("register blink timer: {err}");
    }

    // SIGUSR1 reloads the config in place.
    match calloop::signals::Signals::new(&[calloop::signals::Signal::SIGUSR1]) {
        Ok(signals) => {
            let registered = event_loop
                .handle()
                .insert_source(signals, |_, _, app: &mut App| app.reload_config());
            if let Err(err) = registered {
                tracing::warn!("register signal source: {err}");
            }
        }
        Err(err) => tracing::warn!("install SIGUSR1 handler: {err}"),
    }

    // Each iteration blocks until an event (PTY output, input, configure, frame
    // callback, blink) arrives, then presents at most one frame; bursts of PTY
    // output between frame callbacks coalesce into a single repaint.
    while !app.exit {
        event_loop
            .dispatch(None, &mut app)
            .context("dispatch event loop")?;
        app.flush();
    }
    Ok(app.exit_code)
}

/// Bind a singleton global at version 1 with `()` user-data, or `None` if the
/// compositor does not advertise it.
fn bind_global<I>(globals: &GlobalList, qh: &QueueHandle<App>) -> Option<I>
where
    I: Proxy + 'static,
    App: Dispatch<I, ()>,
{
    match globals.bind(qh, 1..=1, ()) {
        Ok(global) => Some(global),
        Err(err) => {
            tracing::debug!("bind {}: {err}", I::interface().name);
            None
        }
    }
}

/// Columns and rows that fit a `width`×`height` px window at `metrics`, after
/// reserving `2 * pad` pixels of inner padding on each axis.
fn grid_size(
    metrics: crate::font::CellMetrics,
    width: u32,
    height: u32,
    pad: (u32, u32),
) -> (u16, u16) {
    let cols = (width.saturating_sub(2 * pad.0) / metrics.width).max(1);
    let rows = (height.saturating_sub(2 * pad.1) / metrics.height).max(1);
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
    /// Sets the pointer to an I-beam over the window (cursor-shape-v1).
    cursor_shape_manager: Option<CursorShapeManager>,
    cursor_shape_device: Option<WpCursorShapeDeviceV1>,
    /// Presents a scaled buffer at the logical surface size (viewporter).
    viewport: Option<WpViewport>,
    /// Per-surface fractional-scale object; kept alive to receive scale events.
    fractional_scale: Option<WpFractionalScaleV1>,
    /// Compositor's preferred scale in 120ths (120 = 1.0, 180 = 1.5).
    scale120: u32,
    data_device: Option<DataDevice>,
    primary_device: Option<PrimarySelectionDevice>,
    /// Held while we own the clipboard / primary selection, serving paste reads.
    copy_source: Option<CopyPasteSource>,
    primary_source: Option<PrimarySelectionSource>,
    clipboard: String,
    primary_clip: String,
    /// A left-button drag is in progress.
    selecting: bool,
    /// Button base code held down while mouse reporting, for drag reports.
    pressed_button: Option<u8>,
    /// Last cell a motion report was emitted for, to suppress duplicates.
    last_report_cell: Option<(usize, usize)>,
    /// Autoscroll direction while dragging past an edge: +1 back, -1 toward live.
    autoscroll: isize,
    /// Calloop token for the repeating autoscroll timer, when armed.
    autoscroll_timer: Option<RegistrationToken>,
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
    /// The active user configuration.
    config: Config,
    /// Path the config was loaded from, for SIGUSR1 live reload.
    config_path: Option<std::path::PathBuf>,
    /// Resolved key/text bindings.
    bindings: crate::bindings::Bindings,
    /// Current font size in pixels (changed by font-resize bindings).
    font_size: u32,
    /// Whether the toplevel is fullscreen.
    fullscreen: bool,
    width: u32,
    height: u32,
    /// The grid changed and the window wants repainting on the next frame.
    needs_draw: bool,
    /// A `wl_surface.frame` callback is in flight; defer drawing until it fires.
    frame_pending: bool,
    /// Double/triple-buffer ring, each tagged with the rows it currently shows.
    frames: Vec<FrameBuf>,
    /// Pixel size the `frames` buffers were allocated for.
    buf_dims: (u32, u32),
    /// Current blink phase, toggled by a timer; off hides blinking ink.
    blink_on: bool,
    /// Armed while synchronized output holds the screen, to force it open.
    sync_timeout: Option<RegistrationToken>,
    /// The visual bell is inverting the screen.
    flashing: bool,
    /// Timer that ends the visual-bell flash.
    flash_timer: Option<RegistrationToken>,
    /// Whether incremental search mode is active (the query lives in the grid).
    searching: bool,
    /// Whether the toplevel currently has keyboard focus (drives the cursor).
    focused: bool,
    exit: bool,
    /// Exit code to return, taken from the shell when it exits.
    exit_code: ExitCode,
}

impl App {
    /// Spawn the shell at the current window size and start reading its output.
    fn spawn_session(&mut self) {
        let (cols, rows) = self.grid_dims();
        let pty = match Pty::spawn(cols, rows, &self.config.main.term) {
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

        let mut term = Term::new(cols as usize, rows as usize);
        term.set_theme(crate::theme::Theme::from_config(&self.config.colors));
        let grid = term.grid_mut();
        grid.set_word_delimiters(self.config.main.word_delimiters.clone());
        grid.set_scrollback_cap(self.config.scrollback.lines);
        if let Some(shape) = cursor_shape_from(self.config.cursor.style.as_deref()) {
            grid.set_cursor_shape(shape);
        }
        grid.set_cursor_blink(self.config.cursor.blink);
        self.session = Some(Session { pty, term });
    }

    /// Handle a key (initial press or repeat): configured bindings first, then
    /// text bindings, else the byte encoding sent to the shell (which snaps the
    /// viewport back to the live screen).
    fn handle_key(&mut self, event: &KeyEvent) {
        // While searching, the keyboard edits the query and navigates matches.
        if self.searching {
            self.search_key(event);
            return;
        }
        if let Some(action) = self.bindings.action(event, self.modifiers) {
            self.dispatch_action(action);
            return;
        }
        if let Some(text) = self.bindings.text(event, self.modifiers) {
            let bytes = text.to_vec();
            self.send_to_shell(&bytes);
            return;
        }

        let app_cursor = self
            .session
            .as_ref()
            .is_some_and(|s| s.term.grid().app_cursor());
        if let Some(bytes) = crate::input::encode(event, self.modifiers, app_cursor) {
            self.send_to_shell(&bytes);
        }
    }

    /// Write key/text bytes to the shell, snapping the viewport to the live
    /// screen and clearing any selection first.
    fn send_to_shell(&mut self, bytes: &[u8]) {
        if let Some(session) = self.session.as_mut() {
            session.term.scroll_to_bottom();
            session.term.grid_mut().clear_selection();
            self.needs_draw = true;
            if let Err(err) = write_all(session.pty.master(), bytes) {
                tracing::warn!("write key to pty: {err}");
            }
        }
    }

    /// Run a bound editor action.
    fn dispatch_action(&mut self, action: crate::bindings::Action) {
        use crate::bindings::Action;
        match action {
            Action::Copy => {
                let qh = self.qh.clone();
                self.set_clipboard(&qh);
            }
            Action::Paste => self.paste_clipboard(),
            Action::PastePrimary => self.paste_primary(),
            Action::ScrollPageUp => self.scroll_page(true),
            Action::ScrollPageDown => self.scroll_page(false),
            Action::ScrollTop => {
                if let Some(session) = self.session.as_mut() {
                    session.term.scroll_view(isize::MAX);
                    self.needs_draw = true;
                }
            }
            Action::ScrollBottom => {
                if let Some(session) = self.session.as_mut() {
                    session.term.scroll_to_bottom();
                    self.needs_draw = true;
                }
            }
            Action::SearchStart => self.toggle_search(),
            Action::FontIncrease => self.change_font_size(self.font_size + 1),
            Action::FontDecrease => self.change_font_size(self.font_size.saturating_sub(1)),
            Action::FontReset => self.change_font_size(self.config.main.font_size),
            Action::Fullscreen => self.toggle_fullscreen(),
        }
    }

    /// Scroll the viewport one page back (`up`) or toward the live screen.
    fn scroll_page(&mut self, up: bool) {
        if let Some(session) = self.session.as_mut() {
            let page = session.term.page() as isize;
            session.term.scroll_view(if up { page } else { -page });
            self.needs_draw = true;
        }
    }

    /// Toggle the toplevel between fullscreen and windowed.
    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        if self.fullscreen {
            self.window.set_fullscreen(None);
        } else {
            self.window.unset_fullscreen();
        }
    }

    /// Re-read the config file and apply it in place (SIGUSR1).
    fn reload_config(&mut self) {
        let new = Config::load(self.config_path.as_deref());
        self.bindings =
            crate::bindings::Bindings::from_config(&new.key_bindings, &new.text_bindings);
        let font_changed = new.main.font != self.config.main.font
            || new.main.font_size != self.config.main.font_size;
        if font_changed {
            self.font_size = new.main.font_size;
        }
        self.config.main.font = new.main.font.clone();
        self.config.main.font_size = new.main.font_size;
        self.config.main.pad_x = new.main.pad_x;
        self.config.main.pad_y = new.main.pad_y;
        // Re-rasterize at the active scale and update padding in one place.
        self.rescale_render();
        if let Some(session) = self.session.as_mut() {
            session
                .term
                .set_theme(crate::theme::Theme::from_config(&new.colors));
            let grid = session.term.grid_mut();
            grid.set_word_delimiters(new.main.word_delimiters.clone());
            grid.set_scrollback_cap(new.scrollback.lines);
            if let Some(shape) = cursor_shape_from(new.cursor.style.as_deref()) {
                grid.set_cursor_shape(shape);
            }
            grid.set_cursor_blink(new.cursor.blink);
        }
        self.config = new;
        self.frames.clear();
        self.resize_grid();
        self.needs_draw = true;
        tracing::info!("config reloaded");
    }

    /// Re-rasterize the font at `new_size`, then re-derive the grid geometry.
    fn change_font_size(&mut self, new_size: u32) {
        let new_size = new_size.clamp(6, 200);
        if new_size == self.font_size {
            return;
        }
        self.font_size = new_size;
        self.rescale_render();
        self.frames.clear();
        self.resize_grid();
        self.needs_draw = true;
    }

    /// Enter or leave incremental search mode.
    fn toggle_search(&mut self) {
        self.searching = !self.searching;
        if let Some(session) = self.session.as_mut() {
            if self.searching {
                session.term.grid_mut().set_search("");
            } else {
                session.term.grid_mut().clear_search();
            }
        }
        self.needs_draw = true;
    }

    /// Handle a key while search mode is active: edit the query incrementally,
    /// step between matches, or exit.
    fn search_key(&mut self, event: &KeyEvent) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let grid = session.term.grid_mut();
        match event.keysym {
            Keysym::Escape => {
                grid.clear_search();
                self.searching = false;
            }
            Keysym::Return | Keysym::KP_Enter | Keysym::Up | Keysym::Page_Up => {
                grid.search_step(false);
            }
            Keysym::Down | Keysym::Page_Down => grid.search_step(true),
            Keysym::BackSpace => {
                let mut query = grid.search_query().unwrap_or("").to_string();
                query.pop();
                grid.set_search(&query);
            }
            _ => {
                // Append typed text, ignoring control characters.
                if let Some(text) = event.utf8.as_ref() {
                    let printable: String = text.chars().filter(|c| !c.is_control()).collect();
                    if !printable.is_empty() {
                        let mut query = grid.search_query().unwrap_or("").to_string();
                        query.push_str(&printable);
                        grid.set_search(&query);
                    }
                }
            }
        }
        self.needs_draw = true;
    }

    /// Scale a logical pixel length to physical (buffer) pixels at the current
    /// fractional scale, rounding to nearest.
    fn to_phys(&self, v: u32) -> u32 {
        ((u64::from(v) * u64::from(self.scale120) + 60) / 120) as u32
    }

    /// Physical (buffer) pixel size at the current scale.
    fn phys_dims(&self) -> (u32, u32) {
        (
            self.to_phys(self.width).max(1),
            self.to_phys(self.height).max(1),
        )
    }

    /// Columns and rows for the current physical size, metrics, and padding.
    /// Scale-invariant: every term scales together so the cell count is stable.
    fn grid_dims(&self) -> (u16, u16) {
        let (pw, ph) = self.phys_dims();
        grid_size(
            self.renderer.metrics(),
            pw,
            ph,
            (
                self.to_phys(self.config.main.pad_x),
                self.to_phys(self.config.main.pad_y),
            ),
        )
    }

    /// Re-rasterize the font and padding at the current scale × the logical
    /// font size, so glyphs are crisp at fractional scales.
    fn rescale_render(&mut self) {
        self.renderer.set_padding(
            self.to_phys(self.config.main.pad_x),
            self.to_phys(self.config.main.pad_y),
        );
        let px = self.to_phys(self.font_size).max(1);
        if let Err(err) = self.renderer.set_font(&self.config.main.font, px) {
            tracing::warn!("rasterize font at scale {}: {err:#}", self.scale120);
        }
    }

    /// Adopt a new preferred scale (in 120ths): re-rasterize, re-derive the grid
    /// geometry, and update the viewport so the logical size stays put.
    fn set_scale(&mut self, scale120: u32) {
        let scale120 = scale120.max(1);
        if scale120 == self.scale120 {
            return;
        }
        self.scale120 = scale120;
        self.rescale_render();
        self.frames.clear();
        self.buf_dims = (0, 0);
        if let Some(vp) = &self.viewport {
            vp.set_destination(self.width.max(1) as i32, self.height.max(1) as i32);
        }
        self.resize_grid();
        self.needs_draw = true;
    }

    /// Inner padding `(x, y)` in physical pixels (pointer coords are converted
    /// to physical before use).
    fn padding(&self) -> (f64, f64) {
        (
            f64::from(self.to_phys(self.config.main.pad_x)),
            f64::from(self.to_phys(self.config.main.pad_y)),
        )
    }

    /// Convert a logical surface coordinate to a physical buffer coordinate.
    fn to_phys_f(&self, v: f64) -> f64 {
        v * f64::from(self.scale120) / 120.0
    }

    /// Map window pixel coordinates to an absolute `(row, col)` grid point.
    fn cell_at(&self, px: f64, py: f64) -> Option<(usize, usize)> {
        let session = self.session.as_ref()?;
        let m = self.renderer.metrics();
        let (pad_x, pad_y) = self.padding();
        let (px, py) = (self.to_phys_f(px), self.to_phys_f(py));
        let grid = session.term.grid();
        let col =
            ((px - pad_x).max(0.0) as usize / m.width as usize).min(grid.cols().saturating_sub(1));
        let vrow =
            ((py - pad_y).max(0.0) as usize / m.height as usize).min(grid.rows().saturating_sub(1));
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
        let ctrl = self.modifiers.ctrl;
        let grid = session.term.grid_mut();
        match count {
            2 => grid.select_word(row, col),
            3 => grid.select_line(row),
            // Holding Ctrl starts a rectangular (block) selection.
            _ if ctrl => grid.start_block_selection(row, col),
            _ => grid.start_selection(row, col),
        }
        self.selecting = true;
        self.needs_draw = true;
    }

    /// Pointer motion during a drag: extend the selection head and, if the
    /// pointer has left the top/bottom edge, start autoscrolling.
    fn pointer_drag(&mut self) {
        if !self.selecting {
            return;
        }
        let Some((row, col)) = self.cell_at(self.pointer_pos.0, self.pointer_pos.1) else {
            return;
        };
        if let Some(session) = self.session.as_mut() {
            session.term.grid_mut().extend_selection(row, col);
            self.needs_draw = true;
        }
        self.update_autoscroll();
    }

    /// Arm or disarm edge autoscroll based on the pointer's vertical position.
    fn update_autoscroll(&mut self) {
        let dir = if self.pointer_pos.1 < 0.0 {
            1 // above the top: reveal older lines
        } else if self.pointer_pos.1 >= f64::from(self.height) {
            -1 // below the bottom: advance toward the live screen
        } else {
            0
        };
        self.autoscroll = dir;
        if dir != 0 && self.autoscroll_timer.is_none() {
            let timer = Timer::immediate();
            self.autoscroll_timer = self
                .loop_handle
                .insert_source(timer, |_, _, app: &mut App| app.autoscroll_step())
                .ok();
        }
    }

    /// One autoscroll step: scroll the viewport and drag the selection head to
    /// the edge cell under the pointer. Reschedules until the drag ends or the
    /// pointer returns inside the window.
    fn autoscroll_step(&mut self) -> TimeoutAction {
        if !self.selecting || self.autoscroll == 0 {
            self.autoscroll_timer = None;
            return TimeoutAction::Drop;
        }
        if let Some(session) = self.session.as_mut() {
            session.term.scroll_view(self.autoscroll);
        }
        let edge_y = if self.autoscroll > 0 {
            0.0
        } else {
            f64::from(self.height) - 1.0
        };
        if let Some((row, col)) = self.cell_at(self.pointer_pos.0, edge_y)
            && let Some(session) = self.session.as_mut()
        {
            session.term.grid_mut().extend_selection(row, col);
        }
        self.needs_draw = true;
        TimeoutAction::ToDuration(Duration::from_millis(AUTOSCROLL_MS))
    }

    /// Left-button release: stop autoscrolling and publish the primary selection.
    fn pointer_release(&mut self, qh: &QueueHandle<App>) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        self.autoscroll = 0;
        if let Some(token) = self.autoscroll_timer.take() {
            self.loop_handle.remove(token);
        }
        self.set_primary(qh);
    }

    /// Whether the application wants mouse reports and the user is not holding
    /// Shift (which forces local selection regardless of mode).
    fn mouse_reporting(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.term.grid().mouse_protocol() != MouseProtocol::Off)
            && !self.modifiers.shift
    }

    /// The viewport cell `(col, row)` under the pointer, clamped to the screen.
    fn report_screen_cell(&self) -> Option<(usize, usize)> {
        let session = self.session.as_ref()?;
        let m = self.renderer.metrics();
        let (pad_x, pad_y) = self.padding();
        let (ppx, ppy) = (
            self.to_phys_f(self.pointer_pos.0),
            self.to_phys_f(self.pointer_pos.1),
        );
        let grid = session.term.grid();
        let col =
            ((ppx - pad_x).max(0.0) as usize / m.width as usize).min(grid.cols().saturating_sub(1));
        let row = ((ppy - pad_y).max(0.0) as usize / m.height as usize)
            .min(grid.rows().saturating_sub(1));
        Some((col, row))
    }

    /// Report a button press/release to the application, if reporting is active.
    /// Returns whether the event was consumed (so local handling is skipped).
    fn try_report_button(&mut self, code: u8, pressed: bool) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        let grid = session.term.grid();
        let proto = grid.mouse_protocol();
        if proto == MouseProtocol::Off || self.modifiers.shift {
            return false;
        }
        let enc = grid.mouse_encoding();
        // X10 (mode 9) reports presses only; a release is swallowed, not sent.
        if (pressed || proto != MouseProtocol::X10)
            && let Some((col, row)) = self.report_screen_cell()
        {
            let bytes =
                crate::input::encode_mouse(enc, code, col, row, pressed, false, self.modifiers);
            self.write_to_pty(&bytes);
            self.last_report_cell = Some((col, row));
        }
        true
    }

    /// Report pointer motion to the application when the active mode wants it.
    /// Returns whether reporting consumed the motion (suppressing local drag).
    fn try_report_motion(&mut self) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        let grid = session.term.grid();
        let proto = grid.mouse_protocol();
        if proto == MouseProtocol::Off || self.modifiers.shift {
            return false;
        }
        let enc = grid.mouse_encoding();
        let wants = match proto {
            MouseProtocol::Any => true,
            MouseProtocol::Button => self.pressed_button.is_some(),
            _ => false,
        };
        if wants
            && let Some((col, row)) = self.report_screen_cell()
            && self.last_report_cell != Some((col, row))
        {
            // Any-event motion with no button held uses the "no button" code 3.
            let code = self.pressed_button.unwrap_or(3);
            let bytes = crate::input::encode_mouse(enc, code, col, row, true, true, self.modifiers);
            self.write_to_pty(&bytes);
            self.last_report_cell = Some((col, row));
        }
        true
    }

    /// Send focus in/out (DECSET 1004) to the application when it asked for it.
    fn report_focus(&mut self, focused: bool) {
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.term.grid().focus_events())
        {
            self.write_to_pty(if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// Write bytes to the PTY master, logging on failure.
    fn write_to_pty(&mut self, bytes: &[u8]) {
        if let Some(session) = self.session.as_mut()
            && let Err(err) = write_all(session.pty.master(), bytes)
        {
            tracing::warn!("write to pty: {err}");
        }
    }

    /// The current selection text, if any and non-empty.
    fn selection_text(&self) -> Option<String> {
        let text = self.session.as_ref()?.term.grid().selection_text()?;
        (!text.is_empty()).then_some(text)
    }

    /// Take ownership of the CLIPBOARD selection, serving `text` to pasters.
    fn claim_clipboard(&mut self, text: String, qh: &QueueHandle<App>) {
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

    /// Take ownership of the primary selection, serving `text` to pasters.
    fn claim_primary(&mut self, text: String, qh: &QueueHandle<App>) {
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

    /// Claim the clipboard (CLIPBOARD) with the current selection (Ctrl+Shift+C).
    fn set_clipboard(&mut self, qh: &QueueHandle<App>) {
        if let Some(text) = self.selection_text() {
            self.claim_clipboard(text, qh);
        }
    }

    /// Claim the primary selection with the current selection (select-to-copy).
    fn set_primary(&mut self, qh: &QueueHandle<App>) {
        if let Some(text) = self.selection_text() {
            self.claim_primary(text, qh);
        }
    }

    /// Act on the OSC 52 clipboard requests an application made: take ownership
    /// of the selection it set, or answer a query with what we currently hold.
    fn handle_clipboard_ops(&mut self, ops: Vec<crate::vt::ClipboardOp>) {
        use crate::vt::ClipboardOp;
        let qh = self.qh.clone();
        for op in ops {
            match op {
                ClipboardOp::Set {
                    primary: true,
                    text,
                } => self.claim_primary(text, &qh),
                ClipboardOp::Set {
                    primary: false,
                    text,
                } => self.claim_clipboard(text, &qh),
                ClipboardOp::Query { primary } => {
                    let text = if primary {
                        &self.primary_clip
                    } else {
                        &self.clipboard
                    };
                    let kind = if primary { 'p' } else { 'c' };
                    let reply = format!(
                        "\x1b]52;{kind};{}\x07",
                        crate::vt::base64_encode(text.as_bytes())
                    );
                    self.write_to_pty(reply.as_bytes());
                }
            }
        }
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
        self.needs_draw = true;
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
        let rang = session.term.take_bell();
        let ops = session.term.take_clipboard_ops();
        if !ops.is_empty() {
            self.handle_clipboard_ops(ops);
        }
        if rang && self.config.bell.visual {
            self.start_flash();
        }
        self.needs_draw = true;
    }

    /// Begin a visual-bell flash: invert the screen for a moment. Clearing the
    /// buffer ring forces a full repaint with the inverted theme.
    fn start_flash(&mut self) {
        self.flashing = true;
        self.frames.clear();
        self.needs_draw = true;
        if self.flash_timer.is_none() {
            let timer = Timer::from_duration(Duration::from_millis(FLASH_MS));
            self.flash_timer = self
                .loop_handle
                .insert_source(timer, |_, _, app: &mut App| {
                    app.flashing = false;
                    app.frames.clear();
                    app.needs_draw = true;
                    app.flash_timer = None;
                    TimeoutAction::Drop
                })
                .ok();
        }
    }

    /// Recompute the grid size for the current window and tell the grid and the
    /// PTY about it if it changed.
    fn resize_grid(&mut self) {
        let (cols, rows) = self.grid_dims();
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

    /// Present a frame if one is wanted and the compositor is ready for it.
    /// Called after every event-loop wake; the frame-callback gate keeps draws
    /// paced to the display instead of one per PTY read. While the app holds
    /// synchronized output (DECSET 2026) we withhold the frame, but arm a
    /// timeout so a stuck `2026h` cannot freeze the window.
    fn flush(&mut self) {
        let sync = self
            .session
            .as_ref()
            .is_some_and(|s| s.term.grid().sync_active());
        if sync {
            if self.sync_timeout.is_none() {
                let timer = Timer::from_duration(Duration::from_millis(SYNC_TIMEOUT_MS));
                self.sync_timeout = self
                    .loop_handle
                    .insert_source(timer, |_, _, app: &mut App| {
                        if let Some(session) = app.session.as_mut() {
                            session.term.grid_mut().set_sync(false);
                        }
                        app.sync_timeout = None;
                        app.needs_draw = true;
                        TimeoutAction::Drop
                    })
                    .ok();
            }
            return;
        }
        if let Some(token) = self.sync_timeout.take() {
            self.loop_handle.remove(token);
        }
        if self.needs_draw && !self.frame_pending && self.session.is_some() {
            self.present();
        }
    }

    /// Render only the rows that changed since the chosen buffer last displayed
    /// them, damage just those rows, and commit with a frame-callback request.
    fn present(&mut self) {
        self.needs_draw = false;
        // Render into a buffer sized in physical pixels (logical × scale); the
        // viewport presents it back at the logical surface size.
        let (w, h) = self.phys_dims();
        let m = self.renderer.metrics();
        let (focused, blink_on) = (self.focused, self.blink_on);

        // A resize invalidates every buffer's contents and size.
        if self.buf_dims != (w, h) {
            self.frames.clear();
            self.buf_dims = (w, h);
        }

        let Some(session) = self.session.as_ref() else {
            return;
        };
        let grid = session.term.grid();
        // The visual bell inverts fg/bg for the duration of the flash.
        let flashed = self.flashing.then(|| session.term.theme().inverted());
        let theme = flashed.as_ref().unwrap_or(session.term.theme());
        let rows = grid.rows();
        let mut cur: Vec<RowSnap> = (0..rows)
            .map(|y| row_snap(grid, y, focused, blink_on))
            .collect();

        // The search prompt occupies the bottom row while search mode is active.
        // Recording it in the snapshot keeps the row's damage/diff correct.
        let bar_text = self.searching.then(|| {
            let (n, total) = grid.search_count();
            format!(
                "search: {}  [{n}/{total}]",
                grid.search_query().unwrap_or("")
            )
        });
        if let Some(text) = &bar_text
            && rows > 0
        {
            cur[rows - 1].overlay = Some(text.clone());
        }

        // Reuse a buffer the compositor has released, else grow the ring.
        let stride = w as i32 * 4;
        let mut idx = None;
        for i in 0..self.frames.len() {
            if self.pool.canvas(&self.frames[i].buffer).is_some() {
                idx = Some(i);
                break;
            }
        }
        let idx = match idx {
            Some(i) => i,
            None if self.frames.len() < MAX_BUFFERS => {
                match self
                    .pool
                    .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
                {
                    Ok((buffer, _)) => {
                        self.frames.push(FrameBuf {
                            buffer,
                            rows: Vec::new(),
                        });
                        self.frames.len() - 1
                    }
                    Err(err) => {
                        tracing::error!("allocate shm buffer: {err}");
                        return;
                    }
                }
            }
            // All buffers are still held by the compositor; a release event will
            // wake us and `needs_draw` (re-set below) retries then.
            None => {
                self.needs_draw = true;
                return;
            }
        };

        // Rows that differ from what this buffer last showed (all, if fresh).
        let prev = &self.frames[idx].rows;
        let dirty: Vec<usize> = (0..rows)
            .filter(|&y| prev.get(y) != Some(&cur[y]))
            .collect();
        if dirty.is_empty() {
            return;
        }

        // A buffer used for the first time has uninitialized margins; paint the
        // whole thing (background + padding) once, then damage it in full below.
        let fresh = self.frames[idx].rows.is_empty();
        let pad_y = self.to_phys(self.config.main.pad_y) as i32;
        let Some(canvas) = self.pool.canvas(&self.frames[idx].buffer) else {
            return;
        };
        let dims = (w as usize, h as usize);
        let frame = crate::render::Frame {
            theme,
            focused,
            blink_on,
        };
        if fresh {
            self.renderer.clear(canvas, dims, theme);
        }
        for &y in &dirty {
            self.renderer.render_row(canvas, dims, grid, &frame, y);
        }
        // Draw the search prompt over the (now repainted) bottom row.
        if let Some(text) = &bar_text
            && dirty.contains(&(rows - 1))
        {
            self.renderer
                .render_search_bar(canvas, dims, theme, rows - 1, text);
        }
        self.frames[idx].rows = cur;

        let surface = self.window.wl_surface();
        if let Err(err) = self.frames[idx].buffer.attach_to(surface) {
            tracing::error!("attach buffer: {err}");
            return;
        }
        // With a viewport the buffer is presented at the logical destination, so
        // its own scale stays 1; without one, fall back to integer buffer scale.
        if let Some(vp) = &self.viewport {
            surface.set_buffer_scale(1);
            vp.set_destination(self.width.max(1) as i32, self.height.max(1) as i32);
        } else {
            surface.set_buffer_scale((self.scale120 / 120).max(1) as i32);
        }
        if fresh {
            surface.damage_buffer(0, 0, w as i32, h as i32);
        } else {
            for &y in &dirty {
                let top = pad_y + y as i32 * m.height as i32;
                surface.damage_buffer(0, top, w as i32, m.height as i32);
            }
        }
        surface.frame(&self.qh, surface.clone());
        self.window.commit();
        self.frame_pending = true;
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        factor: i32,
    ) {
        // Integer fallback for compositors without fractional-scale-v1; ignored
        // when the fractional-scale object drives the scale instead.
        if self.fractional_scale.is_none() {
            self.set_scale((factor.max(1) as u32) * 120);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // The compositor is ready for another frame; `flush` will repaint if the
        // grid has changed since the last present.
        self.frame_pending = false;
    }

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
            if let Some(vp) = &self.viewport {
                vp.set_destination(self.width.max(1) as i32, self.height.max(1) as i32);
            }
        }
        self.focused = configure.is_activated();
        if self.session.is_none() {
            self.spawn_session();
        } else {
            self.resize_grid();
        }
        self.needs_draw = true;
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
                Ok(pointer) => {
                    self.cursor_shape_device = self
                        .cursor_shape_manager
                        .as_ref()
                        .map(|m| m.get_shape_device(&pointer, qh));
                    self.pointer = Some(pointer);
                }
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
        self.report_focus(true);
        self.needs_draw = true;
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
        self.report_focus(false);
        self.needs_draw = true;
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

/// Parse a configured cursor-style name into a [`CursorShape`].
fn cursor_shape_from(style: Option<&str>) -> Option<CursorShape> {
    match style? {
        "block" => Some(CursorShape::Block),
        "beam" | "bar" => Some(CursorShape::Beam),
        "underline" => Some(CursorShape::Underline),
        other => {
            tracing::warn!("unknown cursor style {other:?}");
            None
        }
    }
}

/// Map a Wayland button code to the terminal mouse base code, if reportable.
fn button_code(button: u32) -> Option<u8> {
    match button {
        BTN_LEFT => Some(0),
        BTN_MIDDLE => Some(1),
        BTN_RIGHT => Some(2),
        _ => None,
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
                PointerEventKind::Enter { serial } => {
                    self.pointer_pos = event.position;
                    if let Some(device) = self.cursor_shape_device.as_ref() {
                        device.set_shape(*serial, Shape::Text);
                    }
                    self.pointer_drag();
                }
                PointerEventKind::Motion { .. } => {
                    self.pointer_pos = event.position;
                    if self.try_report_motion() {
                        continue;
                    }
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
                    if let Some(code) = button_code(*button)
                        && self.try_report_button(code, true)
                    {
                        self.pressed_button = Some(code);
                        continue;
                    }
                    match *button {
                        BTN_LEFT => self.pointer_press(*time),
                        BTN_MIDDLE => self.paste_primary(),
                        _ => {}
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    self.pointer_pos = event.position;
                    let code = button_code(*button);
                    if let Some(code) = code
                        && self.try_report_button(code, false)
                    {
                        if self.pressed_button == Some(code) {
                            self.pressed_button = None;
                        }
                        continue;
                    }
                    if *button == BTN_LEFT {
                        self.pointer_release(qh);
                    }
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
                    let lines = (raw.abs() * scale).ceil().max(1.0) as isize;
                    // Reporting apps get wheel buttons (64 up / 65 down) as
                    // presses, one per line, capped so a flick cannot flood.
                    if self.mouse_reporting() {
                        let code = if raw < 0.0 { 64 } else { 65 };
                        for _ in 0..lines.clamp(1, 8) {
                            self.try_report_button(code, true);
                        }
                        continue;
                    }
                    // Positive axis = scroll down (toward live); the viewport
                    // scrolls the opposite way (negative offset delta).
                    let delta = if raw < 0.0 { lines } else { -lines };
                    if let Some(session) = self.session.as_mut() {
                        session.term.scroll_view(delta);
                        self.needs_draw = true;
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

// Fractional-scale and viewporter are not wrapped by sctk, so dispatch them by
// hand. Only the fractional-scale object carries an event we act on.
impl Dispatch<WpFractionalScaleV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.set_scale(scale);
        }
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewporter, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for App {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
