//! Wayland front-end: connection, surface, and software-drawn window.
//!
//! Uses smithay-client-toolkit for protocol boilerplate and calloop for the
//! event loop, so the PTY master fd and timers share one loop.

mod handlers;
mod rendering;

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::num::NonZeroU16;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitCode;

use std::time::Duration;

use anyhow::Context;
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use calloop_wayland_source::WaylandSource;

use crate::config::Config;
use crate::font::Fonts;
use crate::grid::{Cell, CursorShape, Grid, MouseProtocol, UrlHit};
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
    activation::{ActivationHandler, ActivationState, RequestData},
    compositor::{CompositorHandler, CompositorState},
    data_device_manager::{
        DataDeviceManagerState, WritePipe,
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{CopyPasteSource, DataSourceHandler},
    },
        delegate_activation, delegate_compositor, delegate_data_device, delegate_keyboard,
    delegate_output,
    delegate_pointer, delegate_primary_selection, delegate_registry, delegate_seat, delegate_shm,
    delegate_touch, delegate_xdg_shell, delegate_xdg_window,
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
        touch::TouchHandler,
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window as XdgWindow, WindowConfigure, WindowDecorations, WindowHandler},
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
        wl_surface, wl_touch,
    },
};
use wayland_protocols::wp::content_type::v1::client::{
    wp_content_type_manager_v1::WpContentTypeManagerV1,
    wp_content_type_v1::{self, WpContentTypeV1},
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
    zwp_idle_inhibitor_v1::{self, ZwpIdleInhibitorV1},
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{self, ContentHint, ContentPurpose, ZwpTextInputV3},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use handlers::{cursor_shape_from, hint_labels};
use rendering::FrameBuf;

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

/// Frame interval while a graphics-protocol animation is playing; when none is,
/// the timer idles at a slower beat so it does not wake the loop needlessly.
const ANIM_MS: u64 = 40;
const ANIM_IDLE_MS: u64 = 250;

/// Fallback window size in pixels if the configured geometry yields nothing.
const DEFAULT_W: u32 = 800;
const DEFAULT_H: u32 = 600;

/// Run a single window until it is closed, returning the shell's exit code.
pub fn run(config: Config, config_paths: Vec<std::path::PathBuf>) -> anyhow::Result<ExitCode> {
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

    // Surface-creation infrastructure lives on `App` so `open_window` can make
    // new toplevels after startup. The viewport/fractional-scale/content-type
    // objects are created per surface from these managers.
    let viewporter = bind_global::<WpViewporter>(&globals, &qh);
    let fractional_manager = bind_global::<WpFractionalScaleManagerV1>(&globals, &qh);
    let content_type_manager = bind_global::<WpContentTypeManagerV1>(&globals, &qh);
    let text_input_manager = bind_global::<ZwpTextInputManagerV3>(&globals, &qh);
    let activation = ActivationState::bind(&globals, &qh).ok();
    let idle_inhibit_manager = bind_global::<ZwpIdleInhibitManagerV1>(&globals, &qh);

    let fonts = Fonts::new(&config.main.font, config.main.font_size).context("load font")?;
    let mut renderer = Renderer::new(fonts);
    renderer.set_padding(config.main.pad_x, config.main.pad_y);

    let bindings = crate::bindings::Bindings::from_config(
        &config.key_bindings,
        &config.text_bindings,
        &config.mouse_bindings,
    );
    let font_size = config.main.font_size;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        renderer,
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
        modifiers: Modifiers::default(),
        config,
        config_paths,
        bindings,
        font_size,
        blink_on: true,
        exit: false,
        exit_code: ExitCode::SUCCESS,
        windows: Vec::new(),
        focused_window: 0,
        next_window_id: 1,
    };

    // The initial window. The PTY spawns on its first configure, once the real
    // window size is known, so the shell starts at the final size and is not hit
    // by a startup SIGWINCH storm that makes it reprint its prompt.
    app.open_window(None);

    // Toggle the blink phase on a timer so blinking text and cursors animate.
    let blink = Timer::from_duration(Duration::from_millis(BLINK_MS));
    let blink_registered = event_loop
        .handle()
        .insert_source(blink, |_, _, app: &mut App| {
            app.blink_on = !app.blink_on;
            for win in &mut app.windows {
                win.needs_draw = true;
            }
            TimeoutAction::ToDuration(Duration::from_millis(BLINK_MS))
        });
    if let Err(err) = blink_registered {
        tracing::warn!("register blink timer: {err}");
    }

    // Advance graphics-protocol animations. A frame change does not alter the
    // grid cells (only the image pixels), so the buffer ring is dropped to force
    // a repaint of the image rows. The timer slows to an idle beat when nothing
    // is animating.
    let anim = Timer::from_duration(Duration::from_millis(ANIM_IDLE_MS));
    let anim_registered = event_loop
        .handle()
        .insert_source(anim, |_, _, app: &mut App| {
            let mut animating = false;
            for win in &mut app.windows {
                let (changed, anim) = match win.session.as_mut() {
                    Some(session) => (
                        session.term.animation_tick(ANIM_MS as u32),
                        session.term.is_animating(),
                    ),
                    None => (false, false),
                };
                if changed {
                    win.frames.clear();
                    win.needs_draw = true;
                }
                animating |= anim;
            }
            let next = if animating { ANIM_MS } else { ANIM_IDLE_MS };
            TimeoutAction::ToDuration(Duration::from_millis(next))
        });
    if let Err(err) = anim_registered {
        tracing::warn!("register animation timer: {err}");
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
        // Flush every window; resolve each by id since a flush cannot change the
        // window set but keeping ids is robust if that ever changes.
        let ids: Vec<WindowId> = app.windows.iter().map(|w| w.id).collect();
        for id in ids {
            if let Some(idx) = app.window_index_by_id(id) {
                app.flush(idx);
            }
        }
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

/// Input devices for one seat. Several seats can drive the single window; the
/// most recently used one owns clipboard/primary claims.
#[derive(Debug)]
struct SeatData {
    seat: wl_seat::WlSeat,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_shape_device: Option<WpCursorShapeDeviceV1>,
    data_device: Option<DataDevice>,
    primary_device: Option<PrimarySelectionDevice>,
    /// text-input-v3 handle for IME preedit/commit, if the compositor offers it.
    text_input: Option<ZwpTextInputV3>,
    touch: Option<wl_touch::WlTouch>,
}

/// A single-finger touch drag in progress, used to scroll the viewport.
#[derive(Debug)]
struct TouchScroll {
    /// Touch point id we are tracking (the first finger down).
    id: i32,
    /// Surface-local y of the last motion, to take per-event deltas.
    last_y: f64,
    /// Sub-cell pixel remainder carried between motions.
    acc: f64,
}

/// Stable identifier for a window, so calloop sources and timers can name the
/// window they belong to without depending on its shifting `Vec` index.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct WindowId(u64);

/// Per-window state: the surface, its shm buffers, the terminal behind it, and
/// all the input/paint bookkeeping scoped to one toplevel. `App` owns a `Vec`
/// of these.
#[derive(Debug)]
struct Window {
    /// Stable id, assigned at creation and never reused.
    id: WindowId,
    pool: SlotPool,
    window: XdgWindow,
    /// idle-inhibitor held while this window is focused when the
    /// `[main] idle-inhibit` config is on, so the screen does not blank.
    idle_inhibitor: Option<ZwpIdleInhibitorV1>,
    /// content-type-v1 hint object. Set once at startup; held only so the
    /// object (and thus the hint) outlives construction.
    #[allow(
        dead_code,
        reason = "kept alive to preserve the surface content-type hint"
    )]
    content_type: Option<WpContentTypeV1>,
    /// Presents a scaled buffer at the logical surface size (viewporter).
    viewport: Option<WpViewport>,
    /// Per-surface fractional-scale object; kept alive to receive scale events.
    fractional_scale: Option<WpFractionalScaleV1>,
    /// Compositor's preferred scale in 120ths (120 = 1.0, 180 = 1.5).
    scale120: u32,
    /// `None` until the first configure spawns the shell.
    session: Option<Session>,
    /// Directory the shell starts in, seeded when the session spawns; a new
    /// window inherits the focused window's reported cwd.
    pending_cwd: Option<PathBuf>,
    /// Calloop token for the PTY read source, removed on teardown.
    pty_token: Option<RegistrationToken>,
    /// Last title applied to the toplevel, to avoid redundant requests.
    title: Option<String>,
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
    /// Armed while synchronized output holds the screen, to force it open.
    sync_timeout: Option<RegistrationToken>,
    /// The visual bell is inverting the screen.
    flashing: bool,
    /// Timer that ends the visual-bell flash.
    flash_timer: Option<RegistrationToken>,
    /// Committed IME preedit string shown inline at the cursor while composing.
    preedit: String,
    /// Preedit/commit accumulated since the last text-input `done`.
    ime_preedit_pending: String,
    ime_commit_pending: String,
    /// A left-button drag is in progress.
    selecting: bool,
    /// OSC 8 hyperlink under the pointer, underlined and opened on click.
    hovered_link: Option<NonZeroU16>,
    /// Serial of the last pointer enter, reused to update the cursor shape.
    pointer_enter_serial: u32,
    /// Cell `(abs_row, col)` of the last left-press, for click-to-open links.
    press_cell: Option<(usize, usize)>,
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
    /// Whether incremental search mode is active (the query lives in the grid).
    searching: bool,
    /// URL hint mode: detected URLs get keyboard labels to open them.
    url_mode: bool,
    /// Detected URLs and their hint labels (parallel), while `url_mode` is on.
    url_hits: Vec<UrlHit>,
    url_labels: Vec<String>,
    /// Label characters typed so far in URL mode.
    url_input: String,
    /// Hex digits typed so far in Unicode codepoint-input mode; `None` when off.
    unicode_input: Option<String>,
    /// Raw key codes currently held, to tell press from repeat for the kitty
    /// keyboard protocol's event-type reporting.
    keys_down: std::collections::HashSet<u32>,
    /// A single-finger touch drag in progress (scrolls the viewport).
    touch_scroll: Option<TouchScroll>,
    /// Whether the toplevel currently has keyboard focus (drives the cursor).
    focused: bool,
}

/// Process-wide Wayland client state shared across all windows and protocol
/// handlers.
#[derive(Debug)]
struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    renderer: Renderer,
    loop_handle: LoopHandle<'static, App>,
    qh: QueueHandle<App>,
    /// Surface-creation infrastructure, kept so `open_window` can build new
    /// toplevels after startup.
    compositor: CompositorState,
    xdg_shell: XdgShell,
    /// Per-surface viewport source (fractional-scale presentation).
    viewporter: Option<WpViewporter>,
    /// Per-surface fractional-scale source.
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    /// Per-surface content-type hint source.
    content_type_manager: Option<WpContentTypeManagerV1>,
    data_device_manager: DataDeviceManagerState,
    primary_manager: Option<PrimarySelectionManagerState>,
    /// Sets the pointer to an I-beam over the window (cursor-shape-v1).
    cursor_shape_manager: Option<CursorShapeManager>,
    /// IME manager (text-input-v3); per-seat handles live in `seats`.
    text_input_manager: Option<ZwpTextInputManagerV3>,
    /// xdg-activation, used to request attention on an urgent bell.
    activation: Option<ActivationState>,
    /// idle-inhibit-v1 manager; a per-window inhibitor is held while focused.
    idle_inhibit_manager: Option<ZwpIdleInhibitManagerV1>,
    /// One entry per seat; `active_seat` indexes the most recently used.
    seats: Vec<SeatData>,
    active_seat: usize,
    /// Held while we own the clipboard / primary selection, serving paste reads.
    copy_source: Option<CopyPasteSource>,
    primary_source: Option<PrimarySelectionSource>,
    clipboard: String,
    primary_clip: String,
    /// Most recent input serial, used to claim selections.
    serial: u32,
    modifiers: Modifiers,
    /// The active user configuration.
    config: Config,
    /// Paths the config was loaded from, for SIGUSR1 live reload and new windows.
    config_paths: Vec<std::path::PathBuf>,
    /// Resolved key/text bindings.
    bindings: crate::bindings::Bindings,
    /// Current font size in pixels (changed by font-resize bindings).
    font_size: u32,
    /// Current blink phase, toggled by a timer; off hides blinking ink. Global
    /// so every window blinks in step.
    blink_on: bool,
    exit: bool,
    /// Exit code to return, taken from the shell when it exits.
    exit_code: ExitCode,
    /// Hosted windows; the process exits when the last one closes.
    windows: Vec<Window>,
    /// Index into `windows` of the focused window.
    focused_window: usize,
    /// Next window id to hand out; increases monotonically.
    next_window_id: u64,
}

impl App {
    /// The index of the window backing `surface`, if any. Compares each window's
    /// surface by identity, so surface-carrying handlers route to the right one.
    fn window_index(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.windows
            .iter()
            .position(|w| w.window.wl_surface() == surface)
    }

    /// Allocate a fresh, never-reused window id.
    fn alloc_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        id
    }

    /// The index of the window with `id`, if it is still open.
    fn window_index_by_id(&self, id: WindowId) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    /// Create a new toplevel surface and its per-window state, returning its id.
    /// The session spawns on the first configure (unchanged), so the shell starts
    /// at the final size; `cwd` seeds the child's working directory then.
    fn open_window(&mut self, cwd: Option<PathBuf>) -> WindowId {
        let id = self.alloc_window_id();
        let surface = self.compositor.create_surface(&self.qh);
        let window =
            self.xdg_shell
                .create_window(surface, WindowDecorations::RequestServer, &self.qh);
        window.set_title("beer");
        window.set_app_id("dev.notashelf.beer");
        window.set_min_size(Some((1, 1)));

        // Decorrelate buffer pixels from surface size so we can render at the
        // compositor's preferred fractional scale (crisp glyphs on a 150% output)
        // and present at the logical size. Both are optional; without them the
        // window falls back to integer buffer scaling via `scale_factor_changed`.
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(window.wl_surface(), &self.qh, ()));
        // Fractional scaling needs a viewport to present the scaled buffer back at
        // the logical size; without one we can only do integer buffer scaling.
        let fractional_scale = viewport.as_ref().and_then(|_| {
            self.fractional_manager
                .as_ref()
                .map(|mgr| mgr.get_fractional_scale(window.wl_surface(), &self.qh, ()))
        });
        // Tag the surface as plain content (a terminal is none of photo/video/game)
        // so the compositor applies no media-specific treatment. Applies on commit.
        let content_type = self
            .content_type_manager
            .as_ref()
            .map(|mgr| mgr.get_surface_content_type(window.wl_surface(), &self.qh, ()));
        if let Some(ct) = &content_type {
            ct.set_content_type(wp_content_type_v1::Type::None);
        }

        // Start at the configured cell geometry plus padding; the compositor may
        // override it on the first configure.
        let m = self.renderer.metrics();
        let width = (u32::from(self.config.main.initial_cols) * m.width
            + 2 * self.config.main.pad_x)
            .max(1);
        let height = (u32::from(self.config.main.initial_rows) * m.height
            + 2 * self.config.main.pad_y)
            .max(1);
        let pool = match SlotPool::new(
            (width * height * 4).max(DEFAULT_W * DEFAULT_H) as usize,
            &self.shm,
        ) {
            Ok(pool) => pool,
            Err(err) => {
                // Without a buffer pool the window cannot draw; skip it. When it is
                // the only window the loop then exits, mirroring a startup failure.
                tracing::error!("create shm slot pool: {err}");
                if self.windows.is_empty() {
                    self.exit = true;
                }
                return id;
            }
        };

        let win = Window {
            id,
            pool,
            window,
            idle_inhibitor: None,
            content_type,
            viewport,
            fractional_scale,
            scale120: 120,
            session: None,
            pending_cwd: cwd,
            pty_token: None,
            title: None,
            fullscreen: false,
            width,
            height,
            needs_draw: false,
            frame_pending: false,
            frames: Vec::new(),
            buf_dims: (0, 0),
            sync_timeout: None,
            flashing: false,
            flash_timer: None,
            preedit: String::new(),
            ime_preedit_pending: String::new(),
            ime_commit_pending: String::new(),
            selecting: false,
            hovered_link: None,
            pointer_enter_serial: 0,
            press_cell: None,
            pressed_button: None,
            last_report_cell: None,
            autoscroll: 0,
            autoscroll_timer: None,
            pointer_pos: (0.0, 0.0),
            last_click: None,
            searching: false,
            url_mode: false,
            url_hits: Vec::new(),
            url_labels: Vec::new(),
            url_input: String::new(),
            unicode_input: None,
            keys_down: std::collections::HashSet::new(),
            touch_scroll: None,
            focused: false,
        };
        // First commit with no buffer kicks off the initial configure.
        win.window.commit();
        self.windows.push(win);
        id
    }

    /// Tear down one window: drop its long-lived calloop sources, remove it, and
    /// exit the process once the last window is gone.
    fn close_window(&mut self, id: WindowId) {
        let Some(idx) = self.window_index_by_id(id) else {
            return;
        };
        // Deregister the window's own loop sources before dropping it; the timer
        // closures also guard on an id lookup, so a pending fire is a no-op.
        let win = &mut self.windows[idx];
        let tokens = [
            win.pty_token.take(),
            win.autoscroll_timer.take(),
            win.flash_timer.take(),
            win.sync_timeout.take(),
        ];
        for token in tokens.into_iter().flatten() {
            self.loop_handle.remove(token);
        }
        self.windows.remove(idx);
        if self.windows.is_empty() {
            self.exit = true;
        } else if self.focused_window >= self.windows.len() {
            self.focused_window = 0;
        }
    }

    /// Spawn the shell at window `idx`'s current size and start reading output.
    fn spawn_session(&mut self, idx: usize) {
        let (cols, rows) = self.grid_dims(idx);
        let m = self.renderer.metrics();
        let cell = (m.width as u16, m.height as u16);
        let id = self.windows[idx].id;
        let cwd = self.windows[idx].pending_cwd.clone();
        let pty = match Pty::spawn(cols, rows, cell, &self.config.main.term, cwd.as_deref()) {
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
                // Resolve the window each fire; if it is gone the source is stale.
                let Some(idx) = app.window_index_by_id(id) else {
                    return Ok(PostAction::Remove);
                };
                let mut buf = [0u8; 4096];
                let n = match rustix::io::read(&*fd, &mut buf) {
                    Ok(0) => {
                        app.child_exited(id);
                        return Ok(PostAction::Remove);
                    }
                    Ok(n) => n,
                    Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {
                        return Ok(PostAction::Continue);
                    }
                    Err(_) => {
                        app.child_exited(id);
                        return Ok(PostAction::Remove);
                    }
                };
                let cell = app.renderer.metrics();
                if let Some(session) = app.windows[idx].session.as_mut() {
                    session
                        .term
                        .feed(&mut parser, &buf[..n], (cell.width, cell.height));
                }
                app.after_feed(idx);
                Ok(PostAction::Continue)
            });
        match registered {
            Ok(token) => self.windows[idx].pty_token = Some(token),
            Err(err) => {
                tracing::error!("register pty in event loop: {err}");
                self.exit = true;
                return;
            }
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
        self.windows[idx].session = Some(Session { pty, term });
    }

    /// Handle a key (initial press or repeat): configured bindings first, then
    /// text bindings, else the byte encoding sent to the shell (which snaps the
    /// viewport back to the live screen).
    fn handle_key(&mut self, event: &KeyEvent) {
        let idx = self.focused_window;
        // A new arrival of a held key is a repeat; otherwise a fresh press.
        let kind = if self.windows[idx].keys_down.insert(event.raw_code) {
            beer_protocols::key::KeyKind::Press
        } else {
            beer_protocols::key::KeyKind::Repeat
        };

        // The Unicode-input prompt, URL hint mode, and search each capture the
        // keyboard while active.
        if self.windows[idx].unicode_input.is_some() {
            self.unicode_key(event);
            return;
        }
        if self.windows[idx].url_mode {
            self.url_key(event);
            return;
        }
        if self.windows[idx].searching {
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

        let (app_cursor, kitty) = self.windows[idx].session.as_ref().map_or((false, 0), |s| {
            (s.term.grid().app_cursor(), s.term.grid().kitty_flags())
        });
        let bytes = if kitty != 0 {
            beer_protocols::key::kitty_encode(event, self.modifiers, kitty, kind, app_cursor)
        } else {
            beer_protocols::key::encode(event, self.modifiers, app_cursor)
        };
        if let Some(bytes) = bytes {
            self.send_to_shell(&bytes);
        }
    }

    /// Handle a key release: only the kitty keyboard protocol cares, and only
    /// when it has asked for event reporting.
    fn handle_key_release(&mut self, event: &KeyEvent) {
        let idx = self.focused_window;
        self.windows[idx].keys_down.remove(&event.raw_code);
        let (app_cursor, kitty) = self.windows[idx].session.as_ref().map_or((false, 0), |s| {
            (s.term.grid().app_cursor(), s.term.grid().kitty_flags())
        });
        if kitty == 0 {
            return;
        }
        if let Some(bytes) = beer_protocols::key::kitty_encode(
            event,
            self.modifiers,
            kitty,
            beer_protocols::key::KeyKind::Release,
            app_cursor,
        ) {
            self.send_to_shell(&bytes);
        }
    }

    /// Write key/text bytes to the shell, snapping the viewport to the live
    /// screen and clearing any selection first.
    fn send_to_shell(&mut self, bytes: &[u8]) {
        let win = &mut self.windows[self.focused_window];
        if let Some(session) = win.session.as_mut() {
            session.term.scroll_to_bottom();
            session.term.grid_mut().clear_selection();
            win.needs_draw = true;
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
                let win = &mut self.windows[self.focused_window];
                if let Some(session) = win.session.as_mut() {
                    session.term.scroll_view(isize::MAX);
                    win.needs_draw = true;
                }
            }
            Action::ScrollBottom => {
                let win = &mut self.windows[self.focused_window];
                if let Some(session) = win.session.as_mut() {
                    session.term.scroll_to_bottom();
                    win.needs_draw = true;
                }
            }
            Action::SearchStart => self.toggle_search(),
            Action::FontIncrease => self.change_font_size(self.font_size + 1),
            Action::FontDecrease => self.change_font_size(self.font_size.saturating_sub(1)),
            Action::FontReset => self.change_font_size(self.config.main.font_size),
            Action::Fullscreen => self.toggle_fullscreen(),
            Action::NewWindow => self.spawn_new_window(),
            Action::JumpPromptUp => self.jump_prompt(true),
            Action::JumpPromptDown => self.jump_prompt(false),
            Action::PipeCommandOutput => self.pipe_command_output(),
            Action::UrlMode => self.enter_url_mode(),
            Action::UnicodeInput => {
                let win = &mut self.windows[self.focused_window];
                win.unicode_input = Some(String::new());
                win.needs_draw = true;
            }
        }
    }

    /// Handle a key while Unicode codepoint-input mode is active: accumulate hex
    /// digits, then commit the codepoint as UTF-8 on Enter/Space.
    fn unicode_key(&mut self, event: &KeyEvent) {
        let idx = self.focused_window;
        match event.keysym {
            Keysym::Escape => {
                self.windows[idx].unicode_input = None;
                self.windows[idx].needs_draw = true;
            }
            Keysym::BackSpace => {
                if let Some(buf) = self.windows[idx].unicode_input.as_mut() {
                    buf.pop();
                }
                self.windows[idx].needs_draw = true;
            }
            Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                let buf = self.windows[idx].unicode_input.take().unwrap_or_default();
                if let Some(c) = u32::from_str_radix(buf.trim(), 16)
                    .ok()
                    .and_then(char::from_u32)
                {
                    let mut bytes = [0u8; 4];
                    let s = c.encode_utf8(&mut bytes).as_bytes().to_vec();
                    self.send_to_shell(&s);
                }
                self.windows[idx].needs_draw = true;
            }
            _ => {
                if let Some(text) = event.utf8.as_ref() {
                    let hex: String = text.chars().filter(char::is_ascii_hexdigit).collect();
                    // Cap at 6 hex digits - the widest valid codepoint (U+10FFFF) fits.
                    if let Some(buf) = self.windows[idx].unicode_input.as_mut()
                        && buf.len() + hex.len() <= 6
                    {
                        buf.push_str(&hex);
                    }
                    self.windows[idx].needs_draw = true;
                }
            }
        }
    }

    /// Enter URL hint mode: detect the visible URLs and label them. No-op (with
    /// a brief log) when there are none.
    fn enter_url_mode(&mut self) {
        let idx = self.focused_window;
        let Some(session) = self.windows[idx].session.as_ref() else {
            return;
        };
        let hits = session.term.grid().visible_urls();
        if hits.is_empty() {
            return;
        }
        let labels = hint_labels(hits.len());
        let win = &mut self.windows[idx];
        win.url_labels = labels;
        win.url_hits = hits;
        win.url_input = String::new();
        win.url_mode = true;
        win.needs_draw = true;
    }

    /// Leave URL hint mode, discarding any partial label input.
    fn exit_url_mode(&mut self) {
        let win = &mut self.windows[self.focused_window];
        win.url_mode = false;
        win.url_hits.clear();
        win.url_labels.clear();
        win.url_input.clear();
        // Drop the labelled buffers so the next present repaints without labels.
        win.frames.clear();
        win.needs_draw = true;
    }

    /// Handle a key while URL hint mode is active: build up a label, open the
    /// matching URL, or cancel.
    fn url_key(&mut self, event: &KeyEvent) {
        let idx = self.focused_window;
        match event.keysym {
            Keysym::Escape => self.exit_url_mode(),
            Keysym::BackSpace => {
                self.windows[idx].url_input.pop();
                self.windows[idx].needs_draw = true;
            }
            _ => {
                let Some(text) = event.utf8.as_ref() else {
                    return;
                };
                for c in text.chars().filter(|c| c.is_ascii_alphabetic()) {
                    self.windows[idx].url_input.push(c.to_ascii_lowercase());
                }
                // Exact match opens; if no label even has this prefix, cancel.
                let win = &self.windows[idx];
                if let Some(i) = win.url_labels.iter().position(|l| *l == win.url_input) {
                    let url = win.url_hits[i].url.clone();
                    self.exit_url_mode();
                    self.open_url(&url);
                } else if !win.url_labels.iter().any(|l| l.starts_with(&win.url_input)) {
                    self.exit_url_mode();
                } else {
                    self.windows[idx].needs_draw = true;
                }
            }
        }
    }

    /// Scroll the viewport to the previous/next shell prompt (OSC 133).
    fn jump_prompt(&mut self, up: bool) {
        let win = &mut self.windows[self.focused_window];
        if let Some(session) = win.session.as_mut() {
            session.term.grid_mut().jump_prompt(up);
            win.needs_draw = true;
        }
    }

    /// Feed the last command's output (between OSC 133 C and D) to the configured
    /// command on stdin.
    fn pipe_command_output(&mut self) {
        let idx = self.focused_window;
        let argv = &self.config.shell_integration.pipe_command;
        let Some((program, args)) = argv.split_first() else {
            tracing::warn!("pipe-command-output: no [shell-integration] pipe-command configured");
            return;
        };
        let Some(text) = self.windows[idx]
            .session
            .as_ref()
            .and_then(|s| s.term.grid().last_command_output())
        else {
            return;
        };
        let mut cmd = std::process::Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(cwd) = self.windows[idx]
            .session
            .as_ref()
            .and_then(|s| s.term.cwd())
        {
            cmd.current_dir(cwd);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
            }
            Err(err) => tracing::warn!("pipe-command-output: spawn failed: {err}"),
        }
    }

    /// Open a new in-process window in the focused shell's reported working
    /// directory (OSC 7), sharing this process's config and event loop.
    fn spawn_new_window(&mut self) {
        let cwd = self.windows[self.focused_window]
            .session
            .as_ref()
            .and_then(|s| s.term.cwd())
            .map(PathBuf::from);
        self.open_window(cwd);
    }

    /// Send `count` cursor-up/down keys to the shell for alternate-scroll,
    /// honouring the application cursor-key mode (DECCKM).
    fn alternate_scroll(&mut self, up: bool, count: isize) {
        let app_cursor = self.windows[self.focused_window]
            .session
            .as_ref()
            .is_some_and(|s| s.term.grid().app_cursor());
        let seq: &[u8] = match (up, app_cursor) {
            (true, false) => b"\x1b[A",
            (true, true) => b"\x1bOA",
            (false, false) => b"\x1b[B",
            (false, true) => b"\x1bOB",
        };
        for _ in 0..count {
            self.write_to_pty(seq);
        }
    }

    /// Scroll the viewport one page back (`up`) or toward the live screen.
    fn scroll_page(&mut self, up: bool) {
        let win = &mut self.windows[self.focused_window];
        if let Some(session) = win.session.as_mut() {
            let page = session.term.page() as isize;
            session.term.scroll_view(if up { page } else { -page });
            win.needs_draw = true;
        }
    }

    /// Toggle the toplevel between fullscreen and windowed.
    fn toggle_fullscreen(&mut self) {
        let win = &mut self.windows[self.focused_window];
        win.fullscreen = !win.fullscreen;
        if win.fullscreen {
            win.window.set_fullscreen(None);
        } else {
            win.window.unset_fullscreen();
        }
    }

    /// Re-read the config file and apply it in place (SIGUSR1).
    fn reload_config(&mut self) {
        let idx = self.focused_window;
        let new = Config::load(&self.config_paths);
        self.bindings = crate::bindings::Bindings::from_config(
            &new.key_bindings,
            &new.text_bindings,
            &new.mouse_bindings,
        );
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
        self.rescale_render(idx);
        if let Some(session) = self.windows[idx].session.as_mut() {
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
        self.windows[idx].frames.clear();
        self.resize_grid(idx);
        self.windows[idx].needs_draw = true;
        tracing::info!("config reloaded");
    }

    /// Re-rasterize the font at `new_size`, then re-derive the grid geometry.
    fn change_font_size(&mut self, new_size: u32) {
        let idx = self.focused_window;
        let new_size = new_size.clamp(6, 200);
        if new_size == self.font_size {
            return;
        }
        self.font_size = new_size;
        self.rescale_render(idx);
        self.windows[idx].frames.clear();
        self.resize_grid(idx);
        self.windows[idx].needs_draw = true;
    }

    /// Enter or leave incremental search mode.
    fn toggle_search(&mut self) {
        let win = &mut self.windows[self.focused_window];
        win.searching = !win.searching;
        if let Some(session) = win.session.as_mut() {
            if win.searching {
                session.term.grid_mut().set_search("");
            } else {
                session.term.grid_mut().clear_search();
            }
        }
        win.needs_draw = true;
    }

    /// Handle a key while search mode is active: edit the query incrementally,
    /// step between matches, or exit.
    fn search_key(&mut self, event: &KeyEvent) {
        let win = &mut self.windows[self.focused_window];
        let Some(session) = win.session.as_mut() else {
            return;
        };
        let grid = session.term.grid_mut();
        match event.keysym {
            Keysym::Escape => {
                grid.clear_search();
                win.searching = false;
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
        win.needs_draw = true;
    }

    /// The OSC 8 hyperlink id under the pointer, if any.
    fn link_under_pointer(&self, idx: usize) -> Option<NonZeroU16> {
        let (px, py) = self.windows[idx].pointer_pos;
        let (row, col) = self.cell_at(idx, px, py)?;
        self.windows[idx]
            .session
            .as_ref()?
            .term
            .grid()
            .link_at(row, col)
    }

    /// Recompute the hyperlink under the pointer; when it changes, repaint to
    /// move the hover underline and update the pointer to a hand over a link.
    fn update_hover(&mut self, pointer: &wl_pointer::WlPointer) {
        let idx = self.focused_window;
        let link = self.link_under_pointer(idx);
        if link == self.windows[idx].hovered_link {
            return;
        }
        self.windows[idx].hovered_link = link;
        // The hover underline lives in every buffer's snapshot; drop the ring so
        // the affected rows repaint with (or without) it.
        self.windows[idx].frames.clear();
        self.windows[idx].needs_draw = true;
        let shape = if link.is_some() {
            Shape::Pointer
        } else {
            Shape::Text
        };
        let serial = self.windows[idx].pointer_enter_serial;
        if let Some(device) = self
            .seats
            .iter()
            .find(|s| s.pointer.as_ref() == Some(pointer))
            .and_then(|s| s.cursor_shape_device.as_ref())
        {
            device.set_shape(serial, shape);
        }
    }

    /// If the left button was pressed and released on the same hyperlinked cell
    /// (a click, not a drag), open the link.
    fn maybe_open_clicked_link(&mut self) {
        let idx = self.focused_window;
        let (px, py) = self.windows[idx].pointer_pos;
        let release = self.cell_at(idx, px, py);
        let Some((row, col)) = release else { return };
        if self.windows[idx].press_cell != Some((row, col)) {
            return;
        }
        let uri = self.windows[idx]
            .session
            .as_ref()
            .and_then(|s| s.term.grid().link_at(row, col).map(|id| (s, id)))
            .and_then(|(s, id)| s.term.grid().link_uri(id))
            .map(str::to_owned);
        if let Some(uri) = uri {
            self.open_url(&uri);
        }
    }

    /// Launch the configured opener (default `xdg-open`) on a URL.
    fn open_url(&self, url: &str) {
        let Some((program, args)) = self.config.url.launch.split_first() else {
            tracing::warn!("open url: no [url] launch command configured");
            return;
        };
        let mut cmd = std::process::Command::new(program);
        cmd.args(args)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Err(err) = cmd.spawn() {
            tracing::warn!("open url {url:?}: {err}");
        }
    }

    /// Scale a logical pixel length to physical (buffer) pixels at window `idx`'s
    /// current fractional scale, rounding to nearest.
    fn to_phys(&self, idx: usize, v: u32) -> u32 {
        ((u64::from(v) * u64::from(self.windows[idx].scale120) + 60) / 120) as u32
    }

    /// Physical (buffer) pixel size of window `idx` at its current scale.
    fn phys_dims(&self, idx: usize) -> (u32, u32) {
        let win = &self.windows[idx];
        (
            self.to_phys(idx, win.width).max(1),
            self.to_phys(idx, win.height).max(1),
        )
    }

    /// Columns and rows for window `idx`'s physical size, metrics, and padding.
    /// Scale-invariant: every term scales together so the cell count is stable.
    fn grid_dims(&self, idx: usize) -> (u16, u16) {
        let (pw, ph) = self.phys_dims(idx);
        grid_size(
            self.renderer.metrics(),
            pw,
            ph,
            (
                self.to_phys(idx, self.config.main.pad_x),
                self.to_phys(idx, self.config.main.pad_y),
            ),
        )
    }

    /// Re-rasterize the font and padding at window `idx`'s current scale × the
    /// logical font size, so glyphs are crisp at fractional scales.
    fn rescale_render(&mut self, idx: usize) {
        self.renderer.set_padding(
            self.to_phys(idx, self.config.main.pad_x),
            self.to_phys(idx, self.config.main.pad_y),
        );
        let px = self.to_phys(idx, self.font_size).max(1);
        if let Err(err) = self.renderer.set_font(&self.config.main.font, px) {
            tracing::warn!(
                "rasterize font at scale {}: {err:#}",
                self.windows[idx].scale120
            );
        }
    }

    /// Adopt a new preferred scale (in 120ths) for window `idx`: re-rasterize,
    /// re-derive the grid geometry, and update the viewport so the logical size
    /// stays put.
    fn set_scale(&mut self, idx: usize, scale120: u32) {
        let scale120 = scale120.max(1);
        if scale120 == self.windows[idx].scale120 {
            return;
        }
        self.windows[idx].scale120 = scale120;
        self.rescale_render(idx);
        self.windows[idx].frames.clear();
        self.windows[idx].buf_dims = (0, 0);
        if let Some(vp) = &self.windows[idx].viewport {
            let (w, h) = (self.windows[idx].width, self.windows[idx].height);
            vp.set_destination(w.max(1) as i32, h.max(1) as i32);
        }
        self.resize_grid(idx);
        self.windows[idx].needs_draw = true;
    }

    /// Inner padding `(x, y)` in physical pixels for window `idx` (pointer coords
    /// are converted to physical before use).
    fn padding(&self, idx: usize) -> (f64, f64) {
        (
            f64::from(self.to_phys(idx, self.config.main.pad_x)),
            f64::from(self.to_phys(idx, self.config.main.pad_y)),
        )
    }

    /// Convert a logical surface coordinate to a physical buffer coordinate for
    /// window `idx`.
    fn to_phys_f(&self, idx: usize, v: f64) -> f64 {
        v * f64::from(self.windows[idx].scale120) / 120.0
    }

    /// The active seat (the one that most recently produced input).
    fn active(&self) -> Option<&SeatData> {
        self.seats.get(self.active_seat)
    }

    /// Attach the seat-scoped clipboard and primary-selection devices to seat
    /// `i` if not already present. Idempotent, so it can run from either
    /// `new_seat` or `new_capability` - some compositors surface a pre-existing
    /// seat's capabilities without ever firing `new_seat`.
    fn ensure_clipboard_devices(
        &mut self,
        qh: &QueueHandle<Self>,
        seat: &wl_seat::WlSeat,
        i: usize,
    ) {
        if self.seats[i].data_device.is_none() {
            let dd = self.data_device_manager.get_data_device(qh, seat);
            self.seats[i].data_device = Some(dd);
        }
        if self.seats[i].primary_device.is_none()
            && let Some(m) = self.primary_manager.as_ref()
        {
            let pd = m.get_selection_device(qh, seat);
            self.seats[i].primary_device = Some(pd);
        }
    }

    /// The active seat's clipboard device, if any.
    fn data_device(&self) -> Option<&DataDevice> {
        self.active().and_then(|s| s.data_device.as_ref())
    }

    /// The active seat's primary-selection device, if any.
    fn primary_device(&self) -> Option<&PrimarySelectionDevice> {
        self.active().and_then(|s| s.primary_device.as_ref())
    }

    /// Find (or create) the per-seat entry for `seat`, returning its index.
    fn seat_index(&mut self, seat: &wl_seat::WlSeat) -> usize {
        if let Some(i) = self.seats.iter().position(|s| &s.seat == seat) {
            return i;
        }
        self.seats.push(SeatData {
            seat: seat.clone(),
            keyboard: None,
            pointer: None,
            cursor_shape_device: None,
            data_device: None,
            primary_device: None,
            text_input: None,
            touch: None,
        });
        self.seats.len() - 1
    }

    /// Mark the seat owning `keyboard` as active for clipboard ownership.
    fn activate_keyboard(&mut self, keyboard: &wl_keyboard::WlKeyboard) {
        if let Some(i) = self
            .seats
            .iter()
            .position(|s| s.keyboard.as_ref() == Some(keyboard))
        {
            self.active_seat = i;
        }
    }

    /// Mark the seat owning `pointer` as active for clipboard ownership.
    fn activate_pointer(&mut self, pointer: &wl_pointer::WlPointer) {
        if let Some(i) = self
            .seats
            .iter()
            .position(|s| s.pointer.as_ref() == Some(pointer))
        {
            self.active_seat = i;
        }
    }

    /// Map window `idx` pixel coordinates to an absolute `(row, col)` grid point.
    fn cell_at(&self, idx: usize, px: f64, py: f64) -> Option<(usize, usize)> {
        let session = self.windows[idx].session.as_ref()?;
        let m = self.renderer.metrics();
        let (pad_x, pad_y) = self.padding(idx);
        let (px, py) = (self.to_phys_f(idx, px), self.to_phys_f(idx, py));
        let grid = session.term.grid();
        let col =
            ((px - pad_x).max(0.0) as usize / m.width as usize).min(grid.cols().saturating_sub(1));
        let vrow =
            ((py - pad_y).max(0.0) as usize / m.height as usize).min(grid.rows().saturating_sub(1));
        Some((grid.view_to_abs(vrow), col))
    }

    /// Left-button press: start (or word/line-extend) a selection.
    fn pointer_press(&mut self, time: u32) {
        let idx = self.focused_window;
        let (px, py) = self.windows[idx].pointer_pos;
        let Some((row, col)) = self.cell_at(idx, px, py) else {
            return;
        };
        let count = match self.windows[idx].last_click {
            Some((t, r, c, n))
                if time.wrapping_sub(t) <= MULTI_CLICK_MS && r == row && c == col =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.windows[idx].last_click = Some((time, row, col, count));
        let ctrl = self.modifiers.ctrl;
        let win = &mut self.windows[idx];
        let Some(session) = win.session.as_mut() else {
            return;
        };
        let grid = session.term.grid_mut();
        match count {
            2 => grid.select_word(row, col),
            3 => grid.select_line(row),
            // Holding Ctrl starts a rectangular (block) selection.
            _ if ctrl => grid.start_block_selection(row, col),
            _ => grid.start_selection(row, col),
        }
        win.selecting = true;
        win.needs_draw = true;
    }

    /// Pointer motion during a drag: extend the selection head and, if the
    /// pointer has left the top/bottom edge, start autoscrolling.
    fn pointer_drag(&mut self) {
        let idx = self.focused_window;
        if !self.windows[idx].selecting {
            return;
        }
        let (px, py) = self.windows[idx].pointer_pos;
        let Some((row, col)) = self.cell_at(idx, px, py) else {
            return;
        };
        {
            let win = &mut self.windows[idx];
            if let Some(session) = win.session.as_mut() {
                session.term.grid_mut().extend_selection(row, col);
                win.needs_draw = true;
            }
        }
        self.update_autoscroll();
    }

    /// Arm or disarm edge autoscroll based on the pointer's vertical position.
    fn update_autoscroll(&mut self) {
        let idx = self.focused_window;
        let (py, height) = {
            let win = &self.windows[idx];
            (win.pointer_pos.1, win.height)
        };
        let dir = if py < 0.0 {
            1 // above the top: reveal older lines
        } else if py >= f64::from(height) {
            -1 // below the bottom: advance toward the live screen
        } else {
            0
        };
        self.windows[idx].autoscroll = dir;
        if dir != 0 && self.windows[idx].autoscroll_timer.is_none() {
            let timer = Timer::immediate();
            self.windows[idx].autoscroll_timer = self
                .loop_handle
                .insert_source(timer, |_, _, app: &mut App| app.autoscroll_step())
                .ok();
        }
    }

    /// One autoscroll step: scroll the viewport and drag the selection head to
    /// the edge cell under the pointer. Reschedules until the drag ends or the
    /// pointer returns inside the window.
    fn autoscroll_step(&mut self) -> TimeoutAction {
        let idx = self.focused_window;
        if !self.windows[idx].selecting || self.windows[idx].autoscroll == 0 {
            self.windows[idx].autoscroll_timer = None;
            return TimeoutAction::Drop;
        }
        let ascroll = self.windows[idx].autoscroll;
        {
            let win = &mut self.windows[idx];
            if let Some(session) = win.session.as_mut() {
                session.term.scroll_view(ascroll);
            }
        }
        let height = self.windows[idx].height;
        let edge_y = if ascroll > 0 {
            0.0
        } else {
            f64::from(height) - 1.0
        };
        let px = self.windows[idx].pointer_pos.0;
        if let Some((row, col)) = self.cell_at(idx, px, edge_y) {
            let win = &mut self.windows[idx];
            if let Some(session) = win.session.as_mut() {
                session.term.grid_mut().extend_selection(row, col);
            }
        }
        self.windows[idx].needs_draw = true;
        TimeoutAction::ToDuration(Duration::from_millis(AUTOSCROLL_MS))
    }

    /// Left-button release: stop autoscrolling and publish the primary selection.
    fn pointer_release(&mut self, qh: &QueueHandle<App>) {
        let idx = self.focused_window;
        if !self.windows[idx].selecting {
            return;
        }
        self.windows[idx].selecting = false;
        self.windows[idx].autoscroll = 0;
        if let Some(token) = self.windows[idx].autoscroll_timer.take() {
            self.loop_handle.remove(token);
        }
        self.set_primary(qh);
    }

    /// Whether the application wants mouse reports and the user is not holding
    /// Shift (which forces local selection regardless of mode).
    fn mouse_reporting(&self, idx: usize) -> bool {
        self.windows[idx]
            .session
            .as_ref()
            .is_some_and(|s| s.term.grid().mouse_protocol() != MouseProtocol::Off)
            && !self.modifiers.shift
    }

    /// The viewport cell `(col, row)` under the pointer, clamped to the screen.
    fn report_screen_cell(&self, idx: usize) -> Option<(usize, usize)> {
        let session = self.windows[idx].session.as_ref()?;
        let m = self.renderer.metrics();
        let (pad_x, pad_y) = self.padding(idx);
        let (ppx, ppy) = (
            self.to_phys_f(idx, self.windows[idx].pointer_pos.0),
            self.to_phys_f(idx, self.windows[idx].pointer_pos.1),
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
        let idx = self.focused_window;
        let Some((proto, enc)) = self.windows[idx].session.as_ref().map(|s| {
            let grid = s.term.grid();
            (grid.mouse_protocol(), grid.mouse_encoding())
        }) else {
            return false;
        };
        if proto == MouseProtocol::Off || self.modifiers.shift {
            return false;
        }
        // X10 (mode 9) reports presses only; a release is swallowed, not sent.
        if (pressed || proto != MouseProtocol::X10)
            && let Some((col, row)) = self.report_screen_cell(idx)
        {
            let bytes = beer_protocols::mouse::encode_mouse(
                enc,
                code,
                col,
                row,
                pressed,
                false,
                self.modifiers,
            );
            self.write_to_pty(&bytes);
            self.windows[idx].last_report_cell = Some((col, row));
        }
        true
    }

    /// Report pointer motion to the application when the active mode wants it.
    /// Returns whether reporting consumed the motion (suppressing local drag).
    fn try_report_motion(&mut self) -> bool {
        let idx = self.focused_window;
        let Some((proto, enc)) = self.windows[idx].session.as_ref().map(|s| {
            let grid = s.term.grid();
            (grid.mouse_protocol(), grid.mouse_encoding())
        }) else {
            return false;
        };
        if proto == MouseProtocol::Off || self.modifiers.shift {
            return false;
        }
        let wants = match proto {
            MouseProtocol::Any => true,
            MouseProtocol::Button => self.windows[idx].pressed_button.is_some(),
            _ => false,
        };
        if wants
            && let Some((col, row)) = self.report_screen_cell(idx)
            && self.windows[idx].last_report_cell != Some((col, row))
        {
            // Any-event motion with no button held uses the "no button" code 3.
            let code = self.windows[idx].pressed_button.unwrap_or(3);
            let bytes = beer_protocols::mouse::encode_mouse(
                enc,
                code,
                col,
                row,
                true,
                true,
                self.modifiers,
            );
            self.write_to_pty(&bytes);
            self.windows[idx].last_report_cell = Some((col, row));
        }
        true
    }

    /// Send focus in/out (DECSET 1004) to the application when it asked for it.
    fn report_focus(&mut self, focused: bool) {
        if self.windows[self.focused_window]
            .session
            .as_ref()
            .is_some_and(|s| s.term.grid().focus_events())
        {
            self.write_to_pty(if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// Create or drop the idle inhibitor to match `[main] idle-inhibit` and the
    /// current focus: inhibit only while focused, so a backgrounded terminal
    /// still lets the screen blank. Idempotent; called on every focus change.
    fn sync_idle_inhibit(&mut self) {
        let idx = self.focused_window;
        let want = self.config.main.idle_inhibit && self.windows[idx].focused;
        if want && self.windows[idx].idle_inhibitor.is_none() {
            if let Some(mgr) = &self.idle_inhibit_manager {
                let inhibitor =
                    mgr.create_inhibitor(self.windows[idx].window.wl_surface(), &self.qh, ());
                self.windows[idx].idle_inhibitor = Some(inhibitor);
            }
        } else if !want && let Some(inhibitor) = self.windows[idx].idle_inhibitor.take() {
            inhibitor.destroy();
        }
    }

    /// Write bytes to the focused window's PTY master, logging on failure.
    fn write_to_pty(&mut self, bytes: &[u8]) {
        if let Some(session) = self.windows[self.focused_window].session.as_mut()
            && let Err(err) = write_all(session.pty.master(), bytes)
        {
            tracing::warn!("write to pty: {err}");
        }
    }

    /// The current selection text in window `idx`, if any and non-empty.
    fn selection_text(&self, idx: usize) -> Option<String> {
        let text = self.windows[idx]
            .session
            .as_ref()?
            .term
            .grid()
            .selection_text()?;
        (!text.is_empty()).then_some(text)
    }

    /// Take ownership of the CLIPBOARD selection, serving `text` to pasters.
    fn claim_clipboard(&mut self, text: String, qh: &QueueHandle<App>) {
        let Some(device) = self.data_device() else {
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
        let (Some(manager), Some(device)) = (self.primary_manager.as_ref(), self.primary_device())
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
        if let Some(text) = self.selection_text(self.focused_window) {
            self.claim_clipboard(text, qh);
        }
    }

    /// Claim the primary selection with the current selection (select-to-copy).
    fn set_primary(&mut self, qh: &QueueHandle<App>) {
        if let Some(text) = self.selection_text(self.focused_window) {
            self.claim_primary(text, qh);
        }
    }

    /// Point the IME's candidate popup at window `idx`'s terminal cursor, in
    /// logical surface coordinates (the renderer works in physical pixels, so
    /// divide the physical cell rectangle back down by the scale).
    fn ime_set_cursor_rect(&self, idx: usize, ti: &ZwpTextInputV3) {
        let Some(session) = self.windows[idx].session.as_ref() else {
            return;
        };
        let (cx, cy) = session.term.grid().cursor();
        let m = self.renderer.metrics();
        let s = f64::from(self.windows[idx].scale120) / 120.0;
        let pad_x = f64::from(self.to_phys(idx, self.config.main.pad_x));
        let pad_y = f64::from(self.to_phys(idx, self.config.main.pad_y));
        let x = ((pad_x + cx as f64 * f64::from(m.width)) / s) as i32;
        let y = ((pad_y + cy as f64 * f64::from(m.height)) / s) as i32;
        let w = (f64::from(m.width) / s) as i32;
        let h = (f64::from(m.height) / s) as i32;
        ti.set_cursor_rectangle(x, y, w.max(1), h.max(1));
    }

    /// Apply one IME transaction: commit any committed text to the shell, adopt
    /// the new preedit, then re-commit our state (cursor rectangle) to the IME.
    fn ime_done(&mut self, ti: &ZwpTextInputV3) {
        let idx = self.focused_window;
        let commit = {
            let win = &mut self.windows[idx];
            let commit = std::mem::take(&mut win.ime_commit_pending);
            // Preedit is replaced wholesale each cycle; an absent preedit clears it.
            win.preedit = std::mem::take(&mut win.ime_preedit_pending);
            commit
        };
        if !commit.is_empty() {
            self.send_to_shell(commit.as_bytes());
        }
        self.ime_set_cursor_rect(idx, ti);
        ti.commit();
        self.windows[idx].needs_draw = true;
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
                        beer_protocols::codec::base64_encode(text.as_bytes())
                    );
                    self.write_to_pty(reply.as_bytes());
                }
            }
        }
    }

    /// Paste the CLIPBOARD selection into the shell (Ctrl+Shift+V).
    fn paste_clipboard(&mut self) {
        let Some(offer) = self.data_device().and_then(|d| d.data().selection_offer()) else {
            tracing::debug!("paste: no clipboard selection offer");
            return;
        };
        let Some(mime) = offer.with_mime_types(pick_mime) else {
            tracing::debug!("paste: no acceptable text mime type offered");
            return;
        };
        if let Ok(pipe) = offer.receive(mime) {
            self.read_paste(pipe);
        }
    }

    /// Paste the primary selection into the shell (middle click).
    fn paste_primary(&mut self) {
        let Some(offer) = self
            .primary_device()
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
                        tracing::debug!("paste: read {} bytes from clipboard", data.len());
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
        let win = &mut self.windows[self.focused_window];
        let Some(session) = win.session.as_mut() else {
            return;
        };
        session.term.scroll_to_bottom();
        win.needs_draw = true;
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

    /// After parsing window `idx`'s child output: send any replies, sync the
    /// title, repaint.
    fn after_feed(&mut self, idx: usize) {
        let (rang, ops, notifications) = {
            let win = &mut self.windows[idx];
            let Some(session) = win.session.as_mut() else {
                return;
            };
            let reply = session.term.take_response();
            if !reply.is_empty()
                && let Err(err) = write_all(session.pty.master(), &reply)
            {
                tracing::warn!("write to pty: {err}");
            }

            let new_title = session.term.title().map(str::to_owned);
            if new_title.as_deref() != win.title.as_deref() {
                win.title = new_title;
                win.window.set_title(win.title.clone().unwrap_or_default());
            }
            (
                session.term.take_bell(),
                session.term.take_clipboard_ops(),
                session.term.take_notifications(),
            )
        };
        if !ops.is_empty() {
            self.handle_clipboard_ops(ops);
        }
        for note in notifications {
            self.send_notification(&note);
        }
        if rang {
            self.ring_bell();
        }
        self.windows[idx].needs_draw = true;
    }

    /// React to a `BEL`: optionally flash, run the configured bell command, and
    /// request the compositor's attention when unfocused.
    fn ring_bell(&mut self) {
        if self.config.bell.visual {
            self.start_flash();
        }
        if let Some((program, args)) = self.config.bell.command.split_first() {
            let _ = std::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .inspect_err(|err| tracing::warn!("bell command: {err}"));
        }
        if self.config.bell.urgent && !self.windows[self.focused_window].focused {
            self.request_attention();
        }
    }

    /// Deliver a desktop notification through the configured notifier (default
    /// `notify-send`), appending the title and body as the final arguments.
    fn send_notification(&self, note: &crate::vt::Notification) {
        let Some((program, args)) = self.config.notify.command.split_first() else {
            return;
        };
        let title = note
            .title
            .clone()
            .or_else(|| self.windows[self.focused_window].title.clone())
            .unwrap_or_else(|| "beer".to_string());
        let _ = std::process::Command::new(program)
            .args(args)
            .arg(title)
            .arg(&note.body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .inspect_err(|err| tracing::warn!("notify command: {err}"));
    }

    /// Ask the compositor to draw attention to the window (xdg-activation).
    fn request_attention(&mut self) {
        let Some(activation) = self.activation.as_ref() else {
            return;
        };
        let seat_and_serial = self
            .seats
            .get(self.active_seat)
            .map(|s| (s.seat.clone(), self.serial));
        let data = smithay_client_toolkit::activation::RequestData {
            app_id: Some("dev.notashelf.beer".to_string()),
            seat_and_serial,
            surface: Some(
                self.windows[self.focused_window]
                    .window
                    .wl_surface()
                    .clone(),
            ),
        };
        activation.request_token::<App>(&self.qh, data);
    }

    /// Begin a visual-bell flash: invert the screen for a moment. Clearing the
    /// buffer ring forces a full repaint with the inverted theme.
    fn start_flash(&mut self) {
        let idx = self.focused_window;
        let id = self.windows[idx].id;
        self.windows[idx].flashing = true;
        self.windows[idx].frames.clear();
        self.windows[idx].needs_draw = true;
        if self.windows[idx].flash_timer.is_none() {
            let timer = Timer::from_duration(Duration::from_millis(FLASH_MS));
            self.windows[idx].flash_timer = self
                .loop_handle
                .insert_source(timer, move |_, _, app: &mut App| {
                    // Resolve by id: the window may have closed since arming.
                    if let Some(idx) = app.window_index_by_id(id) {
                        app.windows[idx].flashing = false;
                        app.windows[idx].frames.clear();
                        app.windows[idx].needs_draw = true;
                        app.windows[idx].flash_timer = None;
                    }
                    TimeoutAction::Drop
                })
                .ok();
        }
    }

    /// Recompute window `idx`'s grid size and tell the grid and the PTY about it
    /// if it changed.
    fn resize_grid(&mut self, idx: usize) {
        let (cols, rows) = self.grid_dims(idx);
        let m = self.renderer.metrics();
        let cell = (m.width as u16, m.height as u16);
        let win = &mut self.windows[idx];
        let Some(session) = win.session.as_mut() else {
            return;
        };
        if (cols as usize, rows as usize)
            == (session.term.grid().cols(), session.term.grid().rows())
        {
            return;
        }
        session.term.resize(cols as usize, rows as usize);
        if let Err(err) = session.pty.resize(cols, rows, cell) {
            tracing::warn!("resize pty: {err}");
        }
    }

    /// The child shell of window `id` has gone away; reap it, capture its code
    /// when it is the last window (so a single-window session still propagates
    /// the shell's status), and tear the window down.
    fn child_exited(&mut self, id: WindowId) {
        let Some(idx) = self.window_index_by_id(id) else {
            return;
        };
        let last = self.windows.len() == 1;
        if let Some(session) = self.windows[idx].session.as_mut() {
            match session.pty.wait() {
                Ok(status) => {
                    tracing::info!("shell exited: {status}");
                    // Mirror the shell's status: its code, or 128+signal if killed.
                    if last {
                        let code = status
                            .code()
                            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
                        self.exit_code = ExitCode::from(code as u8);
                    }
                }
                Err(err) => tracing::warn!("reap shell: {err}"),
            }
        }
        self.close_window(id);
    }

    /// Present window `idx`'s frame if one is wanted and the compositor is ready.
    /// Called after every event-loop wake; the frame-callback gate keeps draws
    /// paced to the display instead of one per PTY read. While the app holds
    /// synchronized output (DECSET 2026) we withhold the frame, but arm a
    /// timeout so a stuck `2026h` cannot freeze the window.
    fn flush(&mut self, idx: usize) {
        let sync = self.windows[idx]
            .session
            .as_ref()
            .is_some_and(|s| s.term.grid().sync_active());
        if sync {
            if self.windows[idx].sync_timeout.is_none() {
                let id = self.windows[idx].id;
                let timer = Timer::from_duration(Duration::from_millis(SYNC_TIMEOUT_MS));
                self.windows[idx].sync_timeout = self
                    .loop_handle
                    .insert_source(timer, move |_, _, app: &mut App| {
                        // Resolve by id: the window may have closed since arming.
                        if let Some(idx) = app.window_index_by_id(id) {
                            if let Some(session) = app.windows[idx].session.as_mut() {
                                session.term.grid_mut().set_sync(false);
                            }
                            app.windows[idx].sync_timeout = None;
                            app.windows[idx].needs_draw = true;
                        }
                        TimeoutAction::Drop
                    })
                    .ok();
            }
            return;
        }
        if let Some(token) = self.windows[idx].sync_timeout.take() {
            self.loop_handle.remove(token);
        }
        if self.windows[idx].needs_draw
            && !self.windows[idx].frame_pending
            && self.windows[idx].session.is_some()
        {
            self.present(idx);
        }
    }
}
