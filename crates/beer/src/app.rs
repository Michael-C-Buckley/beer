//! The terminal application: implements [`beer_window::App`], driven by a
//! backend. It owns the terminal sessions, rendering, selection/search/url/IME
//! state, and the daemon glue; every platform action (present a frame, set the
//! title, own the clipboard, open/close windows, watch fds, arm timers) goes
//! through the [`beer_window::WindowCtx`] each callback receives. It never
//! names a Wayland type.

use std::{
  collections::{HashMap, HashSet},
  fs,
  io::{self, ErrorKind, Write as _},
  mem,
  num::NonZeroU16,
  os::{
    fd::{AsRawFd as _, OwnedFd},
    unix::{
      net::{UnixListener, UnixStream},
      process::ExitStatusExt as _,
    },
  },
  path::PathBuf,
  process::{Command, Stdio},
  time::Instant,
};

use beer_protocols::{codec, key, mouse};
use beer_window::{
  App as WindowApp,
  CursorIcon,
  ImeEvent,
  KeyEvent,
  KeyKind,
  Modifiers,
  PointerButton,
  PointerEvent,
  TouchEvent,
  WindowCtx,
  WindowId,
};

use crate::{
  bindings::{Action, Bindings, MouseButton},
  config::{Config, Main},
  font::{CellMetrics, FontOptions, Fonts},
  grid::{Cell, CursorShape, Flags, Grid, MouseProtocol, UrlHit},
  ipc,
  pty::Pty,
  render::Renderer,
  theme::Theme,
  vt::{ClipboardOp, Notification, Progress, Term},
};

/// Max gap between clicks counted as a multi-click (ms).
const MULTI_CLICK_MS: u32 = 400;
/// Blink half-period.
const BLINK_MS: u64 = 500;
/// Visual-bell flash duration.
const FLASH_MS: u64 = 60;
/// Autoscroll step period while dragging past an edge.
const AUTOSCROLL_MS: u64 = 40;
/// Pause before reflowing after a live resize configure.
const RESIZE_REFLOW_MS: u64 = 150;
/// Graphics-animation beat while animating / idle.
const ANIM_MS: u64 = 100;
const ANIM_IDLE_MS: u64 = 500;
/// Force synchronized output (DECSET 2026) open after this long.
const SYNC_TIMEOUT_MS: u64 = 150;
/// Bound on accepted IPC connections per wake and pending clients.
const MAX_IPC_ACCEPTS_PER_WAKE: usize = 16;
const MAX_PENDING_IPC_CLIENTS: usize = 64;

// Reserved source tokens for the global (non-window) event sources; per-window
// and per-client tokens are allocated above `TOKEN_BASE`.
const BLINK_TOKEN: u64 = 1;
const ANIM_TOKEN: u64 = 2;
const IPC_SWEEP_TOKEN: u64 = 3;
const IPC_LISTEN_TOKEN: u64 = 4;
const TOKEN_BASE: u64 = 16;

/// MIME types offered/accepted for clipboard text (used by the OSC 52 reply).
#[expect(
  clippy::absolute_paths,
  reason = "the fd helper uses rustix's explicit platform io type"
)]
fn write_all(fd: &OwnedFd, mut bytes: &[u8]) -> io::Result<()> {
  while !bytes.is_empty() {
    match rustix::io::write(fd, bytes) {
      Ok(0) => return Err(ErrorKind::WriteZero.into()),
      Ok(n) => bytes = &bytes[n..],
      Err(rustix::io::Errno::INTR) => {},
      Err(e) => return Err(e.into()),
    }
  }
  Ok(())
}

/// Whether `fd` has bytes (or EOF) waiting right now, via a zero-timeout poll.
/// Lets the pty be drained in a loop without a blocking read ever stalling the
/// event loop when the queue runs dry.
fn readable_now(fd: &OwnedFd) -> bool {
  use rustix::event::{PollFd, PollFlags, Timespec, poll};
  let mut fds = [PollFd::new(fd, PollFlags::IN)];
  let zero = Timespec {
    tv_sec:  0,
    tv_nsec: 0,
  };
  poll(&mut fds, Some(&zero)).is_ok_and(|ready| ready > 0)
}

/// The terminal behind one window: its pty, parser, and VT state.
struct Session {
  pty:    Pty,
  parser: vte::Parser,
  term:   Term,
}

/// What determines one rendered row's pixels; equal snapshots render alike, so
/// a buffer holding an equal snapshot needs no repaint.
#[derive(Clone, PartialEq)]
struct RowSnap {
  cells:   Vec<Cell>,
  cursor:  Option<(usize, CursorShape, bool)>,
  sel:     Option<(usize, usize)>,
  search:  Vec<(usize, usize, bool)>,
  overlay: Option<String>,
  preedit: Option<(usize, String)>,
  blink:   bool,
}

/// Per-window terminal state (everything not owned by the platform backend).
#[expect(
  clippy::struct_excessive_bools,
  reason = "independent per-window input/mode flags"
)]
struct WinState {
  id:                  WindowId,
  session:             Option<Session>,
  pending_cwd:         Option<PathBuf>,
  pending_env:         Vec<(String, String)>,
  client:              Option<UnixStream>,
  /// Logical surface size and scale, mirrored from configure/scale so pointer
  /// mapping and the present size are computable app-side.
  width:               u32,
  height:              u32,
  scale120:            u32,
  focused:             bool,
  title:               Option<String>,
  fullscreen:          bool,
  /// Per-buffer damage snapshots, keyed by the backend's buffer id.
  snaps:               HashMap<u64, Vec<RowSnap>>,
  /// Source tokens (into the backend loop) owned by this window.
  pty_token:           u64,
  autoscroll_token:    Option<u64>,
  resize_token:        Option<u64>,
  flash_token:         Option<u64>,
  sync_token:          Option<u64>,
  // Input / mode state.
  selecting:           bool,
  hovered_link:        Option<NonZeroU16>,
  press_cell:          Option<(usize, usize)>,
  pressed_button:      Option<u8>,
  last_report_cell:    Option<(usize, usize)>,
  autoscroll:          isize,
  pointer_pos:         (f64, f64),
  last_click:          Option<(Instant, usize, usize, u32)>,
  searching:           bool,
  url_mode:            bool,
  url_hits:            Vec<UrlHit>,
  url_labels:          Vec<String>,
  url_input:           String,
  unicode_input:       Option<String>,
  keys_down:           HashSet<u32>,
  touch_scroll:        Option<(i32, f64, f64)>,
  preedit:             String,
  ime_preedit_pending: String,
  ime_commit_pending:  String,
  flashing:            bool,
  /// Cell geometry of this window's renderer (physical px), refreshed whenever
  /// its scale's renderer is (re)built. Read by the pointer/grid geometry so
  /// it stays correct no matter which window's renderer was touched last.
  metrics:             CellMetrics,
  /// Displayed state changed; a repaint is requested at the next trait-method
  /// boundary via `WindowCtx::request_redraw`.
  needs_draw:          bool,
}

impl WinState {
  fn new(
    id: WindowId,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    client: Option<UnixStream>,
    pty_token: u64,
  ) -> Self {
    Self {
      id,
      session: None,
      pending_cwd: cwd,
      pending_env: env,
      client,
      width: 1,
      height: 1,
      scale120: 120,
      focused: false,
      title: None,
      fullscreen: false,
      snaps: HashMap::new(),
      pty_token,
      autoscroll_token: None,
      resize_token: None,
      flash_token: None,
      sync_token: None,
      selecting: false,
      hovered_link: None,
      press_cell: None,
      pressed_button: None,
      last_report_cell: None,
      autoscroll: 0,
      pointer_pos: (0.0, 0.0),
      last_click: None,
      searching: false,
      url_mode: false,
      url_hits: Vec::new(),
      url_labels: Vec::new(),
      url_input: String::new(),
      unicode_input: None,
      keys_down: HashSet::new(),
      touch_scroll: None,
      preedit: String::new(),
      ime_preedit_pending: String::new(),
      ime_commit_pending: String::new(),
      flashing: false,
      // Non-zero placeholder until the window's renderer is ensured (before any
      // geometry read); avoids a divide-by-zero if ever read early.
      metrics: CellMetrics {
        width:  1,
        height: 1,
        ascent: 0,
        stroke: 1,
      },
      needs_draw: false,
    }
  }
}

/// The terminal application state shared across all its windows.
pub struct App {
  /// One renderer per output scale (120ths). Windows share the renderer for
  /// their scale, so moving between differently-scaled outputs never
  /// re-rasterizes the font.
  renderers:    HashMap<u32, Renderer>,
  config:       Config,
  config_paths: Vec<PathBuf>,
  bindings:     Bindings,
  font_size:    u32,
  blink_on:     bool,
  modifiers:    Modifiers,
  windows:      Vec<WinState>,
  focused:      usize,
  clipboard:    String,
  primary_clip: String,
  exit_code:    u8,
  server:       bool,
  resident:     bool,
  /// Monotonic token allocator for per-window/per-client sources.
  next_token:   u64,
  next_window:  u64,
  /// Daemon: the listening socket and per-client incremental readers, keyed by
  /// the source token the client is watched under.
  ipc_clients:  HashMap<u64, (UnixStream, ipc::RequestReader, Instant)>,
  ipc_listener: Option<UnixListener>,
  /// Path of the bound daemon socket, unlinked on shutdown so a later server
  /// can bind the same path.
  ipc_socket:   Option<PathBuf>,
}

impl Drop for App {
  fn drop(&mut self) {
    if let Some(path) = self.ipc_socket.take() {
      let _ = fs::remove_file(path);
    }
  }
}

impl App {
  /// Build the terminal application from a loaded config.
  pub fn new(
    config: Config,
    config_paths: Vec<PathBuf>,
    server: bool,
  ) -> anyhow::Result<Self> {
    use anyhow::Context as _;
    // Validate the font up front by building the scale-1.0 renderer; other
    // output scales get their own renderer lazily (never re-rasterized on a
    // window switch).
    let fonts =
      Fonts::new(&font_options(&config.main, config.main.font_size, 120))
        .context("load font")?;
    let mut renderer = Renderer::new(fonts);
    renderer.set_padding(config.main.pad_x, config.main.pad_y);
    renderer.set_alpha_blending(config.colors.alpha_blending);
    let mut renderers = HashMap::new();
    renderers.insert(120, renderer);
    let bindings = Bindings::from_config(
      &config.key_bindings,
      &config.text_bindings,
      &config.mouse_bindings,
    );
    let font_size = config.main.font_size;
    let resident = config.main.server_resident;
    Ok(Self {
      renderers,
      config,
      config_paths,
      bindings,
      font_size,
      blink_on: true,
      modifiers: Modifiers::default(),
      windows: Vec::new(),
      focused: 0,
      clipboard: String::new(),
      primary_clip: String::new(),
      exit_code: 0,
      server,
      resident,
      next_token: TOKEN_BASE,
      next_window: 1,
      ipc_clients: HashMap::new(),
      ipc_listener: None,
      ipc_socket: None,
    })
  }

  const fn alloc_token(&mut self) -> u64 {
    let t = self.next_token;
    self.next_token += 1;
    t
  }

  fn win_index(&self, id: WindowId) -> Option<usize> {
    self.windows.iter().position(|w| w.id == id)
  }

  #[expect(
    clippy::unused_self,
    clippy::cast_possible_truncation,
    reason = "method for call-site symmetry; the 120ths quotient fits u32"
  )]
  fn to_phys(&self, w: &WinState, v: u32) -> u32 {
    ((u64::from(v) * u64::from(w.scale120) + 60) / 120) as u32
  }

  #[expect(clippy::unused_self, reason = "method for call-site symmetry")]
  fn to_phys_f(&self, w: &WinState, v: f64) -> f64 {
    v * f64::from(w.scale120) / 120.0
  }

  fn phys_dims(&self, w: &WinState) -> (u32, u32) {
    (
      self.to_phys(w, w.width).max(1),
      self.to_phys(w, w.height).max(1),
    )
  }

  fn grid_dims(&self, w: &WinState) -> (u16, u16) {
    let (pw, ph) = self.phys_dims(w);
    let m = w.metrics;
    let pad = (
      self.to_phys(w, self.config.main.pad_x),
      self.to_phys(w, self.config.main.pad_y),
    );
    grid_size(m, pw, ph, pad)
  }

  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel→cell mapping is clamped to the grid bounds"
  )]
  fn cell_at(&self, w: &WinState, px: f64, py: f64) -> Option<(usize, usize)> {
    let session = w.session.as_ref()?;
    let m = w.metrics;
    let (pad_x, pad_y) = (
      f64::from(self.to_phys(w, self.config.main.pad_x)),
      f64::from(self.to_phys(w, self.config.main.pad_y)),
    );
    let (px, py) = (self.to_phys_f(w, px), self.to_phys_f(w, py));
    let grid = session.term.grid();
    let col = ((px - pad_x).max(0.0) as usize / m.width as usize)
      .min(grid.cols().saturating_sub(1));
    let vrow = ((py - pad_y).max(0.0) as usize / m.height as usize)
      .min(grid.rows().saturating_sub(1));
    Some((grid.view_to_abs(vrow), col))
  }
}

/// Round a logical value to physical pixels for a 120ths scale (120 = 1.0).
#[expect(
  clippy::cast_possible_truncation,
  reason = "the 120ths quotient of a bounded dimension fits u32"
)]
fn phys_at(v: u32, scale120: u32) -> u32 {
  ((u64::from(v) * u64::from(scale120) + 60) / 120) as u32
}

/// Inverse of [`phys_at`]: a physical value back to logical pixels at a 120ths
/// scale.
#[expect(
  clippy::cast_possible_truncation,
  reason = "the logical quotient of a bounded dimension fits u32"
)]
fn logical_at(v: u32, scale120: u32) -> u32 {
  let s = u64::from(scale120.max(1));
  ((u64::from(v) * 120 + s / 2) / s) as u32
}

/// Scale a signed pixel adjustment to the output scale, preserving its sign.
fn phys_adj(v: i32, scale120: u32) -> i32 {
  let scaled = i32::try_from(phys_at(v.unsigned_abs(), scale120)).unwrap_or(0);
  if v < 0 { -scaled } else { scaled }
}

/// Build [`FontOptions`] from configuration for a font size of `px` physical
/// pixels at `scale120`. Metric adjustments scale with the output so they keep
/// their logical size across fractional-scale outputs.
fn font_options(main: &Main, px: u32, scale120: u32) -> FontOptions<'_> {
  let mut options = FontOptions::new(&main.font, px, main.subpixel);
  options.bold_family = main.font_bold.as_deref();
  options.italic_family = main.font_italic.as_deref();
  options.bold_italic_family = main.font_bold_italic.as_deref();
  options.fallback = &main.font_fallback;
  options.variations = &main.font_variations;
  options.features = &main.font_features;
  options.ligatures = main.ligatures;
  options.hinting = main.hinting;
  options.adjust_width = phys_adj(main.adjust_cell_width, scale120);
  options.adjust_height = phys_adj(main.adjust_cell_height, scale120);
  options.adjust_baseline = phys_adj(main.adjust_baseline, scale120);
  options.thicken = main.thicken;
  options
}

/// Columns/rows for a physical size, metrics, and padding.
#[expect(
  clippy::cast_possible_truncation,
  reason = "cols/rows are clamped to u16::MAX before the cast"
)]
fn grid_size(m: CellMetrics, w: u32, h: u32, pad: (u32, u32)) -> (u16, u16) {
  let cols = (w.saturating_sub(2 * pad.0) / m.width).max(1);
  let rows = (h.saturating_sub(2 * pad.1) / m.height).max(1);
  (
    cols.min(u32::from(u16::MAX)) as u16,
    rows.min(u32::from(u16::MAX)) as u16,
  )
}

fn row_cursor(
  grid: &Grid,
  y: usize,
  focused: bool,
  blink_on: bool,
) -> Option<(usize, CursorShape, bool)> {
  if grid.view_at_bottom() && grid.cursor().1 == y {
    let visible = grid.cursor_visible() && (!grid.cursor_blink() || blink_on);
    visible.then(|| (grid.cursor().0, grid.cursor_shape(), focused))
  } else {
    None
  }
}

fn row_snap(
  grid: &Grid,
  y: usize,
  focused: bool,
  blink_on: bool,
  overlay: Option<&str>,
  preedit: Option<(usize, &str)>,
) -> RowSnap {
  let abs = grid.view_to_abs(y);
  let cells = grid.view_row(y).to_vec();
  let has_blink = cells.iter().any(|c| c.flags.contains(Flags::BLINK));
  RowSnap {
    cells,
    cursor: row_cursor(grid, y, focused, blink_on),
    sel: grid.selection_span_on(abs),
    search: grid.search_spans_on(abs),
    overlay: overlay.map(str::to_owned),
    preedit: preedit.map(|(c, t)| (c, t.to_owned())),
    blink: if has_blink { blink_on } else { true },
  }
}

fn row_matches(
  snap: Option<&RowSnap>,
  grid: &Grid,
  y: usize,
  focused: bool,
  blink_on: bool,
  overlay: Option<&str>,
  preedit: Option<(usize, &str)>,
) -> bool {
  let Some(snap) = snap else { return false };
  let abs = grid.view_to_abs(y);
  let blink = if grid
    .view_row(y)
    .iter()
    .any(|c| c.flags.contains(Flags::BLINK))
  {
    blink_on
  } else {
    true
  };
  snap.cells == grid.view_row(y)
    && snap.cursor == row_cursor(grid, y, focused, blink_on)
    && snap.sel == grid.selection_span_on(abs)
    && snap.search == grid.search_spans_on(abs)
    && snap.overlay.as_deref() == overlay
    && snap.preedit.as_ref().map(|(c, t)| (*c, t.as_str())) == preedit
    && snap.blink == blink
}

impl App {
  /// Size a freshly created window to the configured `initial-cols`/
  /// `initial-rows` when the compositor left the initial size to the client,
  /// leaving the logical size at its 1x1 sentinel. A compositor-dictated size
  /// is kept as-is.
  fn apply_initial_size(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let w = &self.windows[idx];
    if w.width > 1 && w.height > 1 {
      return;
    }
    let (m, scale120) = (w.metrics, w.scale120);
    let pad = (
      phys_at(self.config.main.pad_x, scale120),
      phys_at(self.config.main.pad_y, scale120),
    );
    let phys_w = u32::from(self.config.main.initial_cols) * m.width + 2 * pad.0;
    let phys_h =
      u32::from(self.config.main.initial_rows) * m.height + 2 * pad.1;
    let width = logical_at(phys_w, scale120).max(1);
    let height = logical_at(phys_h, scale120).max(1);
    self.windows[idx].width = width;
    self.windows[idx].height = height;
    ctx.request_size(self.windows[idx].id, width, height);
  }

  /// Spawn the shell for window `idx` at its current size and watch its master.
  #[expect(clippy::cast_possible_truncation, reason = "cell metrics fit u16")]
  fn spawn_session(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    self.ensure_renderer(idx);
    self.apply_initial_size(ctx, idx);
    let (cols, rows) = self.grid_dims(&self.windows[idx]);
    let m = self.windows[idx].metrics;
    let cell = (m.width as u16, m.height as u16);
    let id = self.windows[idx].id;
    let cwd = self.windows[idx].pending_cwd.clone();
    let env = mem::take(&mut self.windows[idx].pending_env);
    let pty = match Pty::spawn(
      cols,
      rows,
      cell,
      &self.config.main.term,
      cwd.as_deref(),
      &env,
    ) {
      Ok(pty) => pty,
      Err(err) => {
        tracing::error!("spawn shell: {err:#}");
        self.close(ctx, id);
        return;
      },
    };
    ctx.watch_readable(pty.master().as_raw_fd(), self.windows[idx].pty_token);
    let mut term = Term::new(cols as usize, rows as usize);
    term.set_theme(Theme::from_config(&self.config.colors));
    let grid = term.grid_mut();
    grid.set_word_delimiters(self.config.main.word_delimiters.clone());
    grid.set_scrollback_cap(self.config.scrollback.lines);
    if let Some(shape) = cursor_shape_from(self.config.cursor.style.as_deref())
    {
      grid.set_cursor_shape(shape);
    }
    grid.set_cursor_blink(self.config.cursor.blink);
    self.windows[idx].session = Some(Session {
      pty,
      parser: vte::Parser::new(),
      term,
    });
  }

  /// Read available bytes from window `idx`'s pty, feed the parser,
  /// post-process.
  #[expect(
    clippy::absolute_paths,
    reason = "the pty read uses rustix's explicit platform io type"
  )]
  fn read_pty(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let id = self.windows[idx].id;
    let cell = self.windows[idx].metrics;
    let mut buf = [0u8; 4096];
    // Drain everything the shell has queued before repainting, so a redraw the
    // application emits as one burst (e.g. a graphics frame swap) is never
    // shown half-applied. Each read follows a zero-timeout poll so the
    // blocking master never stalls the loop; the loop ends the moment no more
    // bytes are waiting, and a later arrival re-fires this level-triggered
    // source.
    let mut fed = false;
    loop {
      let res = {
        let Some(session) = self.windows[idx].session.as_ref() else {
          return;
        };
        if !readable_now(session.pty.master()) {
          break;
        }
        rustix::io::read(session.pty.master(), &mut buf)
      };
      let n = match res {
        Ok(0) => {
          self.child_exited(ctx, id);
          return;
        },
        Ok(n) => n,
        Err(rustix::io::Errno::INTR) => continue,
        Err(rustix::io::Errno::AGAIN) => break,
        Err(_) => {
          self.child_exited(ctx, id);
          return;
        },
      };
      if let Some(session) = self.windows[idx].session.as_mut() {
        let Session { parser, term, .. } = session;
        term.feed(parser, &buf[..n], (cell.width, cell.height));
      }
      fed = true;
    }
    if fed {
      self.after_feed(ctx, idx);
    }
  }

  /// Reap window `id`'s exited shell, mirror its status to any client, close
  /// it.
  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "wait status/signal are byte-ranged exit codes"
  )]
  fn child_exited(&mut self, ctx: &mut dyn WindowCtx, id: WindowId) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    let last = self.windows.len() == 1;
    let mut code = 0u8;
    if let Some(session) = self.windows[idx].session.as_mut() {
      match session.pty.wait() {
        Ok(status) => {
          code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
            as u8;
        },
        Err(err) => tracing::warn!("reap shell: {err}"),
      }
    }
    if last {
      self.exit_code = code;
    }
    if let Some(client) = self.windows[idx].client.take() {
      ipc::send_exit(client, code);
    }
    self.close(ctx, id);
  }

  /// After feeding parsed output: send replies, sync title, apply OSC 52 /
  /// notifications / bell, request a repaint.
  fn after_feed(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let id = self.windows[idx].id;
    let (rang, ops, notes) = {
      let win = &mut self.windows[idx];
      let Some(session) = win.session.as_mut() else {
        return;
      };
      let reply = session.term.take_response();
      if !reply.is_empty() {
        let _ = write_all(session.pty.master(), &reply);
      }
      let new_title = match session.term.progress() {
        Some(Progress::Normal(percent)) => {
          format!("{percent}% - {}", session.term.title().unwrap_or("beer"))
        },
        Some(Progress::Error) => {
          format!("error - {}", session.term.title().unwrap_or("beer"))
        },
        Some(Progress::Paused) => {
          format!("paused - {}", session.term.title().unwrap_or("beer"))
        },
        Some(Progress::Indeterminate) => {
          format!("working - {}", session.term.title().unwrap_or("beer"))
        },
        None => session.term.title().unwrap_or("beer").to_owned(),
      };
      if new_title != win.title.as_deref().unwrap_or("beer") {
        win.title = Some(new_title.clone());
        ctx.set_title(id, &new_title);
      }
      (
        session.term.take_bell(),
        session.term.take_clipboard_ops(),
        session.term.take_notifications(),
      )
    };
    if !ops.is_empty() {
      self.handle_clipboard_ops(ctx, ops);
    }
    for note in notes {
      self.send_notification(&note);
    }
    if rang {
      self.ring_bell(ctx, idx);
    }
    ctx.request_redraw(id);
  }

  /// Tear a window down: unwatch its sources and remove it; exit when the last
  /// window closes unless this is a resident server.
  fn close(&mut self, ctx: &mut dyn WindowCtx, id: WindowId) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    for tok in [
      Some(self.windows[idx].pty_token),
      self.windows[idx].autoscroll_token,
      self.windows[idx].resize_token,
      self.windows[idx].flash_token,
      self.windows[idx].sync_token,
    ]
    .into_iter()
    .flatten()
    {
      ctx.unwatch(tok);
      ctx.cancel_timer(tok);
    }
    if let Some(client) = self.windows[idx].client.take() {
      ipc::send_exit(client, 0);
    }
    self.windows.remove(idx);
    ctx.close_window(id);
    if self.windows.is_empty() {
      if !self.resident {
        ctx.exit(self.exit_code);
      }
    } else if self.focused >= self.windows.len() {
      self.focused = self.windows.len() - 1;
    }
  }
}

impl App {
  /// Repaint window `idx` if its displayed state changed: diff rows against the
  /// acquired buffer's snapshot, render the dirty ones, present.
  #[expect(
    clippy::cast_possible_truncation,
    reason = "row indices are bounded by the grid height"
  )]
  fn paint(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let id = self.windows[idx].id;
    if self.windows[idx].session.is_none() {
      return;
    }
    self.ensure_renderer(idx);
    // Synchronized output (DECSET 2026) withholds frames; arm a timeout so a
    // stuck `2026h` cannot freeze the window.
    let sync = self.windows[idx]
      .session
      .as_ref()
      .is_some_and(|s| s.term.grid().sync_active());
    if sync {
      if self.windows[idx].sync_token.is_none() {
        let tok = self.alloc_token();
        self.windows[idx].sync_token = Some(tok);
        ctx.arm_timer(tok, SYNC_TIMEOUT_MS);
      }
      return;
    }
    if let Some(tok) = self.windows[idx].sync_token.take() {
      ctx.cancel_timer(tok);
    }

    let (w, h) = self.phys_dims(&self.windows[idx]);
    let m = self.windows[idx].metrics;
    let scale = self.windows[idx].scale120;
    let pad_y = self.to_phys(&self.windows[idx], self.config.main.pad_y);
    let focused = self.windows[idx].focused;
    let blink_on = self.blink_on;
    // URL labels overlay the grid but are not in the row snapshot, so force a
    // full redraw while labels show by clearing the snapshot cache.
    if self.windows[idx].url_mode {
      self.windows[idx].snaps.clear();
    }

    let Some(frame) = ctx.acquire(id, w, h) else {
      // Every buffer is still held by the compositor; keep the redraw pending
      // so a buffer release re-drives this paint.
      ctx.request_redraw(id);
      return;
    };
    let buf_id = frame.id;
    let pixels = frame.pixels;
    let dims = (w as usize, h as usize);

    let win = &self.windows[idx];
    let Some(session) = win.session.as_ref() else {
      return;
    };
    let grid = session.term.grid();
    let flashed = win.flashing.then(|| session.term.theme().inverted());
    let theme = flashed.as_ref().unwrap_or_else(|| session.term.theme());
    let rows = grid.rows();
    let bar_text = win.unicode_input.as_ref().map_or_else(
      || {
        win.searching.then(|| {
          let (n, total) = grid.search_count();
          format!(
            "search: {}  [{n}/{total}]",
            grid.search_query().unwrap_or("")
          )
        })
      },
      |hex| Some(format!("unicode: U+{}", hex.to_uppercase())),
    );
    let preedit = if !win.preedit.is_empty() && grid.view_at_bottom() {
      let (cx, cy) = grid.cursor();
      (cy < rows).then_some((cy, cx, win.preedit.as_str()))
    } else {
      None
    };

    let empty = Vec::new();
    let prev = win.snaps.get(&buf_id).unwrap_or(&empty);
    let fresh = frame.fresh || prev.is_empty();
    let dirty: Vec<usize> = (0..rows)
      .filter(|&y| {
        let overlay = (y + 1 == rows).then_some(bar_text.as_deref()).flatten();
        let pe = preedit.filter(|&(r, ..)| r == y).map(|(_, c, t)| (c, t));
        fresh
          || !row_matches(prev.get(y), grid, y, focused, blink_on, overlay, pe)
      })
      .collect();
    if dirty.is_empty() {
      return;
    }

    let rframe = crate::render::Frame {
      theme,
      focused,
      blink_on,
      hovered_link: win.hovered_link,
      images: session.term.graphics(),
    };
    // Render through this window's scale renderer (disjoint from `windows`).
    if let Some(renderer) = self.renderers.get_mut(&scale) {
      if fresh {
        renderer.clear(pixels, dims, theme);
      }
      for &y in &dirty {
        renderer.render_row(pixels, dims, grid, &rframe, y);
      }
      if let Some(text) = &bar_text
        && dirty.contains(&(rows - 1))
      {
        renderer.render_search_bar(pixels, dims, theme, rows - 1, text);
      }
      for &y in &dirty {
        if let Some((r, c, t)) = preedit
          && r == y
        {
          renderer.render_preedit(pixels, dims, theme, y, c, t);
        }
      }
      if win.url_mode {
        for (hit, label) in win.url_hits.iter().zip(&win.url_labels) {
          if label.starts_with(&win.url_input) {
            renderer.render_label(pixels, dims, theme, hit.row, hit.col, label);
          }
        }
      }
    }

    // Own the preedit text so the snapshot update can take a mutable borrow of
    // the window (the `&str` above borrows it).
    let preedit_owned = preedit.map(|(r, c, t)| (r, c, t.to_owned()));
    // Update this buffer's snapshot for the next diff.
    let mut snaps =
      mem::take(self.windows[idx].snaps.entry(buf_id).or_default());
    let win = &self.windows[idx];
    let Some(session) = win.session.as_ref() else {
      return;
    };
    let grid = session.term.grid();
    for &y in &dirty {
      let overlay = (y + 1 == rows).then_some(bar_text.as_deref()).flatten();
      let pe = preedit_owned
        .as_ref()
        .filter(|(r, ..)| *r == y)
        .map(|(_, c, t)| (*c, t.as_str()));
      let s = row_snap(grid, y, focused, blink_on, overlay, pe);
      if y < snaps.len() {
        snaps[y] = s;
      } else {
        snaps.push(s);
      }
    }
    snaps.truncate(rows);
    self.windows[idx].snaps.insert(buf_id, snaps);

    let dirty_u32: Vec<u32> = dirty.iter().map(|&y| y as u32).collect();
    ctx.present(id, buf_id, &dirty_u32, m.height, pad_y, fresh);
  }
}

impl App {
  fn write_to_pty(&mut self, idx: usize, bytes: &[u8]) {
    if let Some(session) = self.windows[idx].session.as_mut()
      && let Err(err) = write_all(session.pty.master(), bytes)
    {
      tracing::warn!("write to pty: {err}");
    }
  }

  fn send_to_shell(&mut self, idx: usize, bytes: &[u8]) {
    if let Some(session) = self.windows[idx].session.as_mut() {
      session.term.scroll_to_bottom();
      session.term.grid_mut().clear_selection();
      let _ = write_all(session.pty.master(), bytes);
    }
    self.windows[idx].needs_draw = true;
  }

  fn handle_key(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    event: &KeyEvent,
  ) {
    let kind = if self.windows[idx].keys_down.insert(event.raw_code) {
      KeyKind::Press
    } else {
      KeyKind::Repeat
    };
    if self.windows[idx].unicode_input.is_some() {
      self.unicode_key(idx, event);
      return;
    }
    if self.windows[idx].url_mode {
      self.url_key(idx, event);
      return;
    }
    if self.windows[idx].searching {
      self.search_key(idx, event);
      return;
    }
    if let Some(action) = self.bindings.action(event, self.modifiers) {
      self.dispatch_action(ctx, idx, action);
      return;
    }
    if let Some(text) = self.bindings.text(event, self.modifiers) {
      let bytes = text.to_vec();
      self.send_to_shell(idx, &bytes);
      return;
    }
    let (app_cursor, app_keypad, kitty) = self.windows[idx]
      .session
      .as_ref()
      .map_or((false, false, 0), |s| {
        let g = s.term.grid();
        (g.app_cursor(), g.app_keypad(), g.kitty_flags())
      });
    let bytes = if kitty != 0 {
      key::kitty_encode(event, self.modifiers, kitty, kind, app_cursor)
    } else {
      key::encode(event, self.modifiers, app_cursor, app_keypad)
    };
    if let Some(bytes) = bytes {
      self.send_to_shell(idx, &bytes);
    }
  }

  fn handle_key_release(&mut self, idx: usize, event: &KeyEvent) {
    self.windows[idx].keys_down.remove(&event.raw_code);
    let (app_cursor, kitty) =
      self.windows[idx].session.as_ref().map_or((false, 0), |s| {
        (s.term.grid().app_cursor(), s.term.grid().kitty_flags())
      });
    if kitty == 0 {
      return;
    }
    if let Some(bytes) = key::kitty_encode(
      event,
      self.modifiers,
      kitty,
      KeyKind::Release,
      app_cursor,
    ) {
      self.send_to_shell(idx, &bytes);
    }
  }

  fn unicode_key(&mut self, idx: usize, event: &KeyEvent) {
    use beer_window::Keysym;
    match event.keysym {
      Keysym::Escape => self.windows[idx].unicode_input = None,
      Keysym::BackSpace => {
        if let Some(buf) = self.windows[idx].unicode_input.as_mut() {
          buf.pop();
        }
      },
      Keysym::Return | Keysym::KP_Enter | Keysym::space => {
        let buf = self.windows[idx].unicode_input.take().unwrap_or_default();
        if let Some(c) = u32::from_str_radix(buf.trim(), 16)
          .ok()
          .and_then(char::from_u32)
        {
          let mut bytes = [0u8; 4];
          let s = c.encode_utf8(&mut bytes).as_bytes().to_vec();
          self.send_to_shell(idx, &s);
        }
      },
      _ => {
        if let Some(text) = event.utf8.as_ref() {
          let hex: String =
            text.chars().filter(char::is_ascii_hexdigit).collect();
          if let Some(buf) = self.windows[idx].unicode_input.as_mut()
            && buf.len() + hex.len() <= 6
          {
            buf.push_str(&hex);
          }
        }
      },
    }
    self.windows[idx].needs_draw = true;
  }

  fn url_key(&mut self, idx: usize, event: &KeyEvent) {
    use beer_window::Keysym;
    match event.keysym {
      Keysym::Escape => self.exit_url_mode(idx),
      Keysym::BackSpace => {
        self.windows[idx].url_input.pop();
        self.windows[idx].needs_draw = true;
      },
      _ => {
        let Some(text) = event.utf8.as_ref() else {
          return;
        };
        for c in text.chars().filter(char::is_ascii_alphabetic) {
          self.windows[idx].url_input.push(c.to_ascii_lowercase());
        }
        let win = &self.windows[idx];
        if let Some(i) = win.url_labels.iter().position(|l| *l == win.url_input)
        {
          let url = win.url_hits[i].url.clone();
          self.exit_url_mode(idx);
          self.open_url(&url);
        } else if !win.url_labels.iter().any(|l| l.starts_with(&win.url_input))
        {
          self.exit_url_mode(idx);
        } else {
          self.windows[idx].needs_draw = true;
        }
      },
    }
  }

  fn search_key(&mut self, idx: usize, event: &KeyEvent) {
    use beer_window::Keysym;
    let win = &mut self.windows[idx];
    let Some(session) = win.session.as_mut() else {
      return;
    };
    let grid = session.term.grid_mut();
    match event.keysym {
      Keysym::Escape => {
        grid.clear_search();
        win.searching = false;
      },
      Keysym::Return | Keysym::KP_Enter | Keysym::Up | Keysym::Page_Up => {
        grid.search_step(false);
      },
      Keysym::Down | Keysym::Page_Down => grid.search_step(true),
      Keysym::BackSpace => {
        let mut q = grid.search_query().unwrap_or("").to_string();
        q.pop();
        grid.set_search(&q);
      },
      _ => {
        if let Some(text) = event.utf8.as_ref() {
          let printable: String =
            text.chars().filter(|c| !c.is_control()).collect();
          if !printable.is_empty() {
            let mut q = grid.search_query().unwrap_or("").to_string();
            q.push_str(&printable);
            grid.set_search(&q);
          }
        }
      },
    }
    win.needs_draw = true;
  }

  fn dispatch_action(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    action: Action,
  ) {
    match action {
      Action::Copy => self.set_clipboard(ctx, idx),
      Action::Paste => ctx.request_paste(self.windows[idx].id, false),
      Action::PastePrimary => ctx.request_paste(self.windows[idx].id, true),
      Action::ScrollPageUp => self.scroll_page(idx, true),
      Action::ScrollPageDown => self.scroll_page(idx, false),
      Action::ScrollTop => self.scroll_view(idx, isize::MAX),
      Action::ScrollBottom => {
        if let Some(s) = self.windows[idx].session.as_mut() {
          s.term.scroll_to_bottom();
          self.windows[idx].needs_draw = true;
        }
      },
      Action::SearchStart => self.toggle_search(idx),
      Action::FontIncrease => {
        self.change_font_size(ctx, idx, self.font_size + 1);
      },
      Action::FontDecrease => {
        self.change_font_size(ctx, idx, self.font_size.saturating_sub(1));
      },
      Action::FontReset => {
        self.change_font_size(ctx, idx, self.config.main.font_size);
      },
      Action::Fullscreen => {
        let win = &mut self.windows[idx];
        win.fullscreen = !win.fullscreen;
        ctx.set_fullscreen(win.id, win.fullscreen);
      },
      Action::NewWindow => self.spawn_new_window(ctx, idx),
      Action::JumpPromptUp => self.jump_prompt(idx, true),
      Action::JumpPromptDown => self.jump_prompt(idx, false),
      Action::PipeCommandOutput => self.pipe_command_output(idx),
      Action::UrlMode => self.enter_url_mode(idx),
      Action::UnicodeInput => {
        self.windows[idx].unicode_input = Some(String::new());
        self.windows[idx].needs_draw = true;
      },
    }
  }

  fn scroll_view(&mut self, idx: usize, delta: isize) {
    if let Some(s) = self.windows[idx].session.as_mut() {
      s.term.scroll_view(delta);
      self.windows[idx].needs_draw = true;
    }
  }

  #[expect(
    clippy::cast_possible_wrap,
    reason = "a page height never approaches isize::MAX"
  )]
  fn scroll_page(&mut self, idx: usize, up: bool) {
    if let Some(s) = self.windows[idx].session.as_mut() {
      let page = s.term.page() as isize;
      s.term.scroll_view(if up { page } else { -page });
      self.windows[idx].needs_draw = true;
    }
  }

  fn toggle_search(&mut self, idx: usize) {
    let win = &mut self.windows[idx];
    win.searching = !win.searching;
    if let Some(s) = win.session.as_mut() {
      if win.searching {
        s.term.grid_mut().set_search("");
      } else {
        s.term.grid_mut().clear_search();
      }
    }
    win.needs_draw = true;
  }

  fn jump_prompt(&mut self, idx: usize, up: bool) {
    if let Some(s) = self.windows[idx].session.as_mut() {
      s.term.grid_mut().jump_prompt(up);
      self.windows[idx].needs_draw = true;
    }
  }

  fn enter_url_mode(&mut self, idx: usize) {
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

  fn exit_url_mode(&mut self, idx: usize) {
    let win = &mut self.windows[idx];
    win.url_mode = false;
    win.url_hits.clear();
    win.url_labels.clear();
    win.url_input.clear();
    win.snaps.clear();
    win.needs_draw = true;
  }

  fn spawn_new_window(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let cwd = self.windows[idx]
      .session
      .as_ref()
      .and_then(|s| s.term.cwd())
      .map(PathBuf::from);
    self.open(ctx, cwd, Vec::new(), None);
  }

  fn alternate_scroll(&mut self, idx: usize, up: bool, count: isize) {
    let app_cursor = self.windows[idx]
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
      self.write_to_pty(idx, seq);
    }
  }

  #[expect(
    clippy::disallowed_methods,
    reason = "configured URL opener is a user feature"
  )]
  fn open_url(&self, url: &str) {
    let Some((program, args)) = self.config.url.launch.split_first() else {
      return;
    };
    let _ = Command::new(program)
      .args(args)
      .arg(url)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .inspect_err(|err| tracing::warn!("open url {url:?}: {err}"));
  }

  #[expect(
    clippy::disallowed_methods,
    reason = "configured pipe command is a user feature"
  )]
  fn pipe_command_output(&self, idx: usize) {
    let argv = &self.config.shell_integration.pipe_command;
    let Some((program, args)) = argv.split_first() else {
      return;
    };
    let session = self.windows[idx].session.as_ref();
    let Some(text) = session.and_then(|s| s.term.grid().last_command_output())
    else {
      return;
    };
    let mut cmd = Command::new(program);
    cmd
      .args(args)
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::null());
    if let Some(cwd) = session.and_then(|s| s.term.cwd()) {
      cmd.current_dir(cwd);
    }
    if let Ok(mut child) = cmd.spawn()
      && let Some(mut stdin) = child.stdin.take()
    {
      let _ = stdin.write_all(text.as_bytes());
    }
  }
}

/// The terminal mouse base code for a button, if reportable.
const fn button_code(b: PointerButton) -> Option<u8> {
  match b {
    PointerButton::Left => Some(0),
    PointerButton::Middle => Some(1),
    PointerButton::Right => Some(2),
    PointerButton::Other(_) => None,
  }
}

const fn mouse_button(b: PointerButton) -> Option<MouseButton> {
  match b {
    PointerButton::Left => Some(MouseButton::Left),
    PointerButton::Middle => Some(MouseButton::Middle),
    PointerButton::Right => Some(MouseButton::Right),
    PointerButton::Other(_) => None,
  }
}

impl App {
  #[expect(
    clippy::cast_possible_truncation,
    reason = "scroll-line counts are clamped to a small range"
  )]
  fn on_pointer_event(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    event: PointerEvent,
  ) {
    self.ensure_renderer(idx);
    let cell_h = f64::from(self.windows[idx].metrics.height);
    match event {
      PointerEvent::Enter { x, y, .. } => {
        self.windows[idx].pointer_pos = (x, y);
        self.update_hover(ctx, idx);
        self.pointer_drag(ctx, idx);
      },
      PointerEvent::Leave => {},
      PointerEvent::Motion { x, y } => {
        self.windows[idx].pointer_pos = (x, y);
        if self.try_report_motion(idx) {
          return;
        }
        if !self.windows[idx].selecting {
          self.update_hover(ctx, idx);
        }
        self.pointer_drag(ctx, idx);
      },
      PointerEvent::Press { x, y, button, .. } => {
        self.windows[idx].pointer_pos = (x, y);
        if let Some(code) = button_code(button)
          && self.try_report_button(idx, code, true)
        {
          self.windows[idx].pressed_button = Some(code);
          return;
        }
        if let Some(mb) = mouse_button(button)
          && let Some(action) = self.bindings.mouse_action(mb, self.modifiers)
        {
          self.dispatch_action(ctx, idx, action);
          return;
        }
        if button == PointerButton::Left {
          let cell = self.cell_at(&self.windows[idx], x, y);
          self.windows[idx].press_cell = cell;
          self.pointer_press(idx);
        }
      },
      PointerEvent::Release { x, y, button } => {
        self.windows[idx].pointer_pos = (x, y);
        if let Some(code) = button_code(button)
          && self.try_report_button(idx, code, false)
        {
          if self.windows[idx].pressed_button == Some(code) {
            self.windows[idx].pressed_button = None;
          }
          return;
        }
        if button == PointerButton::Left {
          self.maybe_open_clicked_link(idx);
          self.pointer_release(ctx, idx);
        }
      },
      PointerEvent::Axis { scroll, .. } => {
        if scroll.dy == 0.0 {
          return;
        }
        let mult = self.config.mouse.scroll_multiplier.max(0.0);
        let per = if scroll.discrete {
          scroll.dy.abs() * 3.0
        } else if cell_h > 0.0 {
          scroll.dy.abs() / cell_h
        } else {
          return;
        };
        let lines = (per * mult).ceil().max(1.0) as isize;
        let up = scroll.dy < 0.0;
        if self.mouse_reporting(idx) {
          let code = if up { 64 } else { 65 };
          for _ in 0..lines.clamp(1, 8) {
            self.try_report_button(idx, code, true);
          }
          return;
        }
        let alt = self.windows[idx]
          .session
          .as_ref()
          .is_some_and(|s| s.term.grid().alt_active());
        if alt && self.config.mouse.alternate_scroll {
          self.alternate_scroll(idx, up, lines.clamp(1, 8));
          return;
        }
        self.scroll_view(idx, if up { lines } else { -lines });
      },
    }
  }

  #[expect(
    clippy::cast_possible_truncation,
    reason = "a click interval in ms is far below u32::MAX"
  )]
  fn pointer_press(&mut self, idx: usize) {
    let (px, py) = self.windows[idx].pointer_pos;
    let Some((row, col)) = self.cell_at(&self.windows[idx], px, py) else {
      return;
    };
    let count = match self.windows[idx].last_click {
      Some((t, r, c, n))
        if t.elapsed().as_millis() as u32 <= MULTI_CLICK_MS
          && r == row
          && c == col =>
      {
        n % 3 + 1
      },
      _ => 1,
    };
    self.windows[idx].last_click = Some((Instant::now(), row, col, count));
    let ctrl = self.modifiers.ctrl;
    let win = &mut self.windows[idx];
    let Some(session) = win.session.as_mut() else {
      return;
    };
    let grid = session.term.grid_mut();
    match count {
      2 => grid.select_word(row, col),
      3 => grid.select_line(row),
      _ if ctrl => grid.start_block_selection(row, col),
      _ => grid.start_selection(row, col),
    }
    win.selecting = true;
    win.needs_draw = true;
  }

  fn pointer_drag(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if !self.windows[idx].selecting {
      return;
    }
    let (px, py) = self.windows[idx].pointer_pos;
    let cell = self.cell_at(&self.windows[idx], px, py);
    if let Some((row, col)) = cell
      && let Some(session) = self.windows[idx].session.as_mut()
    {
      session.term.grid_mut().extend_selection(row, col);
      self.windows[idx].needs_draw = true;
    }
    // Arm/disarm edge autoscroll.
    let (py, height) =
      (self.windows[idx].pointer_pos.1, self.windows[idx].height);
    let dir = if py < 0.0 {
      1
    } else if py >= f64::from(height) {
      -1
    } else {
      0
    };
    self.windows[idx].autoscroll = dir;
    if dir != 0 && self.windows[idx].autoscroll_token.is_none() {
      let tok = self.alloc_token();
      self.windows[idx].autoscroll_token = Some(tok);
      ctx.arm_timer(tok, 0);
    }
  }

  fn autoscroll_step(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if !self.windows[idx].selecting || self.windows[idx].autoscroll == 0 {
      if let Some(tok) = self.windows[idx].autoscroll_token.take() {
        ctx.cancel_timer(tok);
      }
      return;
    }
    let dir = self.windows[idx].autoscroll;
    if let Some(session) = self.windows[idx].session.as_mut() {
      session.term.scroll_view(dir);
    }
    let height = self.windows[idx].height;
    let edge_y = if dir > 0 {
      0.0
    } else {
      f64::from(height) - 1.0
    };
    let px = self.windows[idx].pointer_pos.0;
    let cell = self.cell_at(&self.windows[idx], px, edge_y);
    if let Some((row, col)) = cell
      && let Some(session) = self.windows[idx].session.as_mut()
    {
      session.term.grid_mut().extend_selection(row, col);
    }
    self.windows[idx].needs_draw = true;
    if let Some(tok) = self.windows[idx].autoscroll_token {
      ctx.arm_timer(tok, AUTOSCROLL_MS);
    }
  }

  fn pointer_release(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if !self.windows[idx].selecting {
      return;
    }
    self.windows[idx].selecting = false;
    self.windows[idx].autoscroll = 0;
    if let Some(tok) = self.windows[idx].autoscroll_token.take() {
      ctx.cancel_timer(tok);
    }
    self.set_primary(ctx, idx);
  }

  fn mouse_reporting(&self, idx: usize) -> bool {
    self.windows[idx]
      .session
      .as_ref()
      .is_some_and(|s| s.term.grid().mouse_protocol() != MouseProtocol::Off)
      && !self.modifiers.shift
  }

  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel→cell mapping is clamped to the grid bounds"
  )]
  fn report_screen_cell(&self, idx: usize) -> Option<(usize, usize)> {
    let w = &self.windows[idx];
    let session = w.session.as_ref()?;
    let m = w.metrics;
    let (pad_x, pad_y) = (
      f64::from(self.to_phys(w, self.config.main.pad_x)),
      f64::from(self.to_phys(w, self.config.main.pad_y)),
    );
    let (ppx, ppy) = (
      self.to_phys_f(w, w.pointer_pos.0),
      self.to_phys_f(w, w.pointer_pos.1),
    );
    let grid = session.term.grid();
    let col = ((ppx - pad_x).max(0.0) as usize / m.width as usize)
      .min(grid.cols().saturating_sub(1));
    let row = ((ppy - pad_y).max(0.0) as usize / m.height as usize)
      .min(grid.rows().saturating_sub(1));
    Some((col, row))
  }

  fn try_report_button(&mut self, idx: usize, code: u8, pressed: bool) -> bool {
    let Some((proto, enc)) = self.windows[idx].session.as_ref().map(|s| {
      (
        s.term.grid().mouse_protocol(),
        s.term.grid().mouse_encoding(),
      )
    }) else {
      return false;
    };
    if proto == MouseProtocol::Off || self.modifiers.shift {
      return false;
    }
    if (pressed || proto != MouseProtocol::X10)
      && let Some((col, row)) = self.report_screen_cell(idx)
    {
      let bytes = mouse::encode_mouse(
        enc,
        code,
        col,
        row,
        pressed,
        false,
        self.modifiers,
      );
      self.write_to_pty(idx, &bytes);
      self.windows[idx].last_report_cell = Some((col, row));
    }
    true
  }

  fn try_report_motion(&mut self, idx: usize) -> bool {
    let Some((proto, enc)) = self.windows[idx].session.as_ref().map(|s| {
      (
        s.term.grid().mouse_protocol(),
        s.term.grid().mouse_encoding(),
      )
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
      let code = self.windows[idx].pressed_button.unwrap_or(3);
      let bytes =
        mouse::encode_mouse(enc, code, col, row, true, true, self.modifiers);
      self.write_to_pty(idx, &bytes);
      self.windows[idx].last_report_cell = Some((col, row));
    }
    true
  }

  fn report_focus(&mut self, idx: usize, focused: bool) {
    if self.windows[idx]
      .session
      .as_ref()
      .is_some_and(|s| s.term.grid().focus_events())
    {
      self.write_to_pty(idx, if focused { b"\x1b[I" } else { b"\x1b[O" });
    }
  }

  fn link_under_pointer(&self, idx: usize) -> Option<NonZeroU16> {
    let (px, py) = self.windows[idx].pointer_pos;
    let (row, col) = self.cell_at(&self.windows[idx], px, py)?;
    self.windows[idx]
      .session
      .as_ref()?
      .term
      .grid()
      .link_at(row, col)
  }

  fn update_hover(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let link = self.link_under_pointer(idx);
    let id = self.windows[idx].id;
    if link != self.windows[idx].hovered_link {
      self.windows[idx].hovered_link = link;
      self.windows[idx].snaps.clear();
      self.windows[idx].needs_draw = true;
    }
    // A hand over hyperlinks, the I-beam otherwise. Set it every hover update
    // so entering the window applies it even when the hovered link is
    // unchanged.
    ctx.set_cursor(
      id,
      if link.is_some() {
        CursorIcon::Pointer
      } else {
        CursorIcon::Text
      },
    );
  }

  fn maybe_open_clicked_link(&self, idx: usize) {
    let (px, py) = self.windows[idx].pointer_pos;
    let Some((row, col)) = self.cell_at(&self.windows[idx], px, py) else {
      return;
    };
    if self.windows[idx].press_cell != Some((row, col)) {
      return;
    }
    let uri = self.windows[idx].session.as_ref().and_then(|s| {
      s.term
        .grid()
        .link_at(row, col)
        .and_then(|id| s.term.grid().link_uri(id))
        .map(str::to_owned)
    });
    if let Some(uri) = uri {
      self.open_url(&uri);
    }
  }

  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "touch scroll deltas are small line counts"
  )]
  fn on_touch_event(&mut self, idx: usize, event: TouchEvent) {
    self.ensure_renderer(idx);
    let cell_h = f64::from(self.windows[idx].metrics.height);
    match event {
      TouchEvent::Down { id, y, .. } => {
        if self.windows[idx].touch_scroll.is_none() {
          self.windows[idx].touch_scroll = Some((id, y, 0.0));
        }
      },
      TouchEvent::Up { id } => {
        if self.windows[idx].touch_scroll.is_some_and(|t| t.0 == id) {
          self.windows[idx].touch_scroll = None;
        }
      },
      TouchEvent::Cancel => self.windows[idx].touch_scroll = None,
      TouchEvent::Motion { id, y, .. } => {
        let Some((tid, last_y, acc)) = self.windows[idx].touch_scroll.as_mut()
        else {
          return;
        };
        if *tid != id {
          return;
        }
        *acc += y - *last_y;
        *last_y = y;
        let lines = (*acc / cell_h) as isize;
        if lines != 0 {
          *acc = (lines as f64).mul_add(-cell_h, *acc);
          if let Some(session) = self.windows[idx].session.as_mut() {
            session.term.scroll_view(lines);
            self.windows[idx].needs_draw = true;
          }
        }
      },
    }
  }

  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "cursor rect coordinates are small logical pixels"
  )]
  fn ime_set_cursor(&self, ctx: &mut dyn WindowCtx, idx: usize) {
    let w = &self.windows[idx];
    let Some(session) = w.session.as_ref() else {
      return;
    };
    let (cx, cy) = session.term.grid().cursor();
    let m = w.metrics;
    let scale = f64::from(w.scale120) / 120.0;
    let pad_x = f64::from(self.to_phys(w, self.config.main.pad_x));
    let pad_y = f64::from(self.to_phys(w, self.config.main.pad_y));
    let x = ((cx as f64).mul_add(f64::from(m.width), pad_x) / scale) as i32;
    let y = ((cy as f64).mul_add(f64::from(m.height), pad_y) / scale) as i32;
    let cw = (f64::from(m.width) / scale) as i32;
    let ch = (f64::from(m.height) / scale) as i32;
    ctx.set_ime_cursor(w.id, x, y, cw, ch);
  }

  fn ime_done(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let commit = {
      let win = &mut self.windows[idx];
      let commit = mem::take(&mut win.ime_commit_pending);
      win.preedit = mem::take(&mut win.ime_preedit_pending);
      commit
    };
    if !commit.is_empty() {
      self.send_to_shell(idx, commit.as_bytes());
    }
    self.windows[idx].needs_draw = true;
    self.ime_set_cursor(ctx, idx);
  }

  fn selection_text(&self, idx: usize) -> Option<String> {
    let text = self.windows[idx]
      .session
      .as_ref()?
      .term
      .grid()
      .selection_text()?;
    (!text.is_empty()).then_some(text)
  }

  fn set_clipboard(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if let Some(text) = self.selection_text(idx) {
      self.clipboard.clone_from(&text);
      ctx.claim_clipboard(text);
    }
  }

  fn set_primary(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if let Some(text) = self.selection_text(idx) {
      self.primary_clip.clone_from(&text);
      ctx.claim_primary(text);
    }
  }

  fn handle_clipboard_ops(
    &mut self,
    ctx: &mut dyn WindowCtx,
    ops: Vec<ClipboardOp>,
  ) {
    for op in ops {
      match op {
        ClipboardOp::Set {
          primary: true,
          text,
        } => {
          self.primary_clip.clone_from(&text);
          ctx.claim_primary(text);
        },
        ClipboardOp::Set {
          primary: false,
          text,
        } => {
          self.clipboard.clone_from(&text);
          ctx.claim_clipboard(text);
        },
        ClipboardOp::Query { primary } => {
          let text = if primary {
            &self.primary_clip
          } else {
            &self.clipboard
          };
          let kind = if primary { 'p' } else { 'c' };
          let reply = format!(
            "\x1b]52;{kind};{}\x07",
            codec::base64_encode(text.as_bytes())
          );
          self.write_to_pty(self.focused, reply.as_bytes());
        },
      }
    }
  }

  fn paste_bytes(&mut self, idx: usize, data: &[u8]) {
    let win = &mut self.windows[idx];
    let Some(session) = win.session.as_mut() else {
      return;
    };
    session.term.scroll_to_bottom();
    win.needs_draw = true;
    let bracketed = session.term.grid().bracketed_paste();
    let mut clean: Vec<u8> = Vec::with_capacity(data.len());
    let mut prev = 0u8;
    for &b in data {
      match b {
        // Newlines become carriage returns, but a CRLF pair collapses to one
        // CR (the CR was already pushed) so a Windows-style paste does not
        // submit a blank line after every line.
        b'\n' if prev == b'\r' => {},
        b'\n' => clean.push(b'\r'),
        b'\t' | b'\r' | 0x20..=0xFF => clean.push(b),
        _ => {},
      }
      prev = b;
    }
    let fd = session.pty.master();
    if bracketed {
      let _ = write_all(fd, b"\x1b[200~");
      let _ = write_all(fd, &clean);
      let _ = write_all(fd, b"\x1b[201~");
    } else {
      let _ = write_all(fd, &clean);
    }
  }

  #[expect(
    clippy::disallowed_methods,
    reason = "configured bell command is a user feature"
  )]
  fn ring_bell(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if self.config.bell.visual {
      self.start_flash(ctx, idx);
    }
    if let Some((program, args)) = self.config.bell.command.split_first() {
      let _ = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    }
    if self.config.bell.urgent && !self.windows[idx].focused {
      ctx.request_attention(self.windows[idx].id);
    }
  }

  fn start_flash(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    self.windows[idx].flashing = true;
    self.windows[idx].snaps.clear();
    self.windows[idx].needs_draw = true;
    if self.windows[idx].flash_token.is_none() {
      let tok = self.alloc_token();
      self.windows[idx].flash_token = Some(tok);
      ctx.arm_timer(tok, FLASH_MS);
    }
  }

  #[expect(
    clippy::disallowed_methods,
    reason = "configured notifier is a user feature"
  )]
  fn send_notification(&self, note: &Notification) {
    let Some((program, args)) = self.config.notify.command.split_first() else {
      return;
    };
    let title = note
      .title
      .clone()
      .or_else(|| self.windows.get(self.focused).and_then(|w| w.title.clone()))
      .unwrap_or_else(|| "beer".to_string());
    let _ = Command::new(program)
      .args(args)
      .arg(title)
      .arg(&note.body)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn();
  }

  /// Build a renderer rasterized for `scale120` at the current font size.
  fn build_renderer(&self, scale120: u32) -> anyhow::Result<Renderer> {
    use anyhow::Context as _;
    let px = phys_at(self.font_size, scale120).max(1);
    let fonts = Fonts::new(&font_options(&self.config.main, px, scale120))
      .context("load font")?;
    let mut renderer = Renderer::new(fonts);
    renderer.set_padding(
      phys_at(self.config.main.pad_x, scale120),
      phys_at(self.config.main.pad_y, scale120),
    );
    renderer.set_alpha_blending(self.config.colors.alpha_blending);
    Ok(renderer)
  }

  /// Ensure a renderer exists for window `idx`'s scale (building it once) and
  /// refresh the window's cached cell metrics from it. Cheap when cached, so a
  /// window switch never re-rasterizes the font.
  fn ensure_renderer(&mut self, idx: usize) {
    let scale = self.windows[idx].scale120;
    if !self.renderers.contains_key(&scale) {
      match self.build_renderer(scale) {
        Ok(renderer) => {
          self.renderers.insert(scale, renderer);
        },
        Err(err) => {
          tracing::warn!("rasterize font: {err:#}");
          return;
        },
      }
    }
    if let Some(renderer) = self.renderers.get(&scale) {
      self.windows[idx].metrics = renderer.metrics();
    }
  }

  #[expect(clippy::cast_possible_truncation, reason = "cell metrics fit u16")]
  fn resize_grid(&mut self, idx: usize) {
    self.ensure_renderer(idx);
    let (cols, rows) = self.grid_dims(&self.windows[idx]);
    let m = self.windows[idx].metrics;
    let cell = (m.width as u16, m.height as u16);
    let Some(session) = self.windows[idx].session.as_mut() else {
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

  fn set_scale(&mut self, idx: usize, scale120: u32) {
    let scale120 = scale120.max(1);
    if scale120 == self.windows[idx].scale120 {
      return;
    }
    self.windows[idx].scale120 = scale120;
    self.ensure_renderer(idx);
    self.windows[idx].snaps.clear();
    self.resize_grid(idx);
    self.windows[idx].needs_draw = true;
  }

  fn change_font_size(
    &mut self,
    _ctx: &mut dyn WindowCtx,
    _idx: usize,
    new_size: u32,
  ) {
    let new_size = new_size.clamp(6, 200);
    if new_size == self.font_size {
      return;
    }
    self.font_size = new_size;
    // Every cached renderer was rasterized at the old size; drop them all and
    // refresh each window so they rebuild at the new size.
    self.renderers.clear();
    for i in 0..self.windows.len() {
      self.resize_grid(i);
      self.windows[i].snaps.clear();
      self.windows[i].needs_draw = true;
    }
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
    },
  }
}

impl App {
  fn open(
    &mut self,
    ctx: &mut dyn WindowCtx,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    client: Option<UnixStream>,
  ) -> WindowId {
    let id = ctx.open_window();
    let pty_token = self.alloc_token();
    self
      .windows
      .push(WinState::new(id, cwd, env, client, pty_token));
    self.next_window += 1;
    id
  }

  fn accept_clients(&mut self, ctx: &mut dyn WindowCtx) {
    for _ in 0..MAX_IPC_ACCEPTS_PER_WAKE {
      let Some(listener) = self.ipc_listener.as_ref() else {
        return;
      };
      match listener.accept() {
        Ok((stream, _)) => self.register_client(ctx, stream),
        Err(e) if e.kind() == ErrorKind::WouldBlock => return,
        Err(e) => {
          tracing::warn!("ipc accept: {e}");
          return;
        },
      }
    }
  }

  fn register_client(&mut self, ctx: &mut dyn WindowCtx, stream: UnixStream) {
    if self.ipc_clients.len() >= MAX_PENDING_IPC_CLIENTS {
      return;
    }
    if stream.set_nonblocking(true).is_err() {
      return;
    }
    let token = self.alloc_token();
    ctx.watch_readable(stream.as_raw_fd(), token);
    self.ipc_clients.insert(
      token,
      (stream, ipc::RequestReader::default(), Instant::now()),
    );
  }

  fn read_client(&mut self, ctx: &mut dyn WindowCtx, token: u64) {
    let Some((stream, reader, _)) = self.ipc_clients.get_mut(&token) else {
      return;
    };
    match reader.read_from(&*stream) {
      Ok(None) => {},
      Ok(Some(req)) => {
        let client = stream.try_clone().ok();
        self.ipc_clients.remove(&token);
        ctx.unwatch(token);
        self.open(ctx, req.cwd.map(PathBuf::from), req.env, client);
      },
      Err(err) => {
        tracing::warn!("read ipc request: {err}");
        self.ipc_clients.remove(&token);
        ctx.unwatch(token);
      },
    }
  }

  fn expire_clients(&mut self, ctx: &mut dyn WindowCtx) {
    let stale: Vec<u64> = self
      .ipc_clients
      .iter()
      .filter(|(_, (_, _, t))| t.elapsed().as_secs() >= 5)
      .map(|(&tok, _)| tok)
      .collect();
    for tok in stale {
      self.ipc_clients.remove(&tok);
      ctx.unwatch(tok);
    }
  }

  fn reload_config(&mut self, idx: usize) {
    let new = Config::load(&self.config_paths);
    self.bindings = Bindings::from_config(
      &new.key_bindings,
      &new.text_bindings,
      &new.mouse_bindings,
    );
    if new.main.font != self.config.main.font
      || new.main.font_size != self.config.main.font_size
    {
      self.font_size = new.main.font_size;
    }
    if let Some(session) = self.windows[idx].session.as_mut() {
      session.term.set_theme(Theme::from_config(&new.colors));
      let grid = session.term.grid_mut();
      grid.set_word_delimiters(new.main.word_delimiters.clone());
      grid.set_scrollback_cap(new.scrollback.lines);
      if let Some(shape) = cursor_shape_from(new.cursor.style.as_deref()) {
        grid.set_cursor_shape(shape);
      }
      grid.set_cursor_blink(new.cursor.blink);
    }
    self.config = new;
    // Font, padding, and blending may all have changed; drop every cached
    // renderer so each rebuilds from the new config.
    self.renderers.clear();
    self.windows[idx].snaps.clear();
    self.resize_grid(idx);
    self.windows[idx].needs_draw = true;
  }

  /// Request a repaint for every window whose displayed state changed since the
  /// last flush; called at each trait-method boundary.
  fn flush_redraw(&mut self, ctx: &mut dyn WindowCtx) {
    for w in &mut self.windows {
      if w.needs_draw {
        w.needs_draw = false;
        ctx.request_redraw(w.id);
      }
    }
  }
}

/// Generate `n` distinct same-length keyboard hint labels (a, b, …, aa, ab, …).
fn hint_labels(n: usize) -> Vec<String> {
  const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
  if n == 0 {
    return Vec::new();
  }
  let (mut width, mut capacity) = (1usize, 26usize);
  while capacity < n {
    width += 1;
    capacity *= 26;
  }
  (0..n)
    .map(|i| {
      let mut idx = i;
      let mut chars = vec![b'a'; width];
      for slot in chars.iter_mut().rev() {
        *slot = ALPHABET[idx % 26];
        idx /= 26;
      }
      String::from_utf8(chars).unwrap_or_default()
    })
    .collect()
}

impl WindowApp for App {
  fn start(&mut self, ctx: &mut dyn WindowCtx) {
    ctx.arm_timer(BLINK_TOKEN, BLINK_MS);
    ctx.arm_timer(ANIM_TOKEN, ANIM_IDLE_MS);
    if self.server {
      match ipc::bind_listener() {
        Ok((listener, path)) => {
          let _ = listener.set_nonblocking(true);
          ctx.watch_readable(listener.as_raw_fd(), IPC_LISTEN_TOKEN);
          self.ipc_listener = Some(listener);
          self.ipc_socket = Some(path);
          ctx.arm_timer(IPC_SWEEP_TOKEN, 1000);
        },
        Err(err) => {
          tracing::error!("bind daemon socket: {err}");
          ctx.exit(1);
        },
      }
    } else {
      self.open(ctx, None, Vec::new(), None);
    }
    self.flush_redraw(ctx);
  }

  fn on_configure(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    width: u32,
    height: u32,
    activated: bool,
    resizing: bool,
  ) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    // A pixel-size change makes the backend reallocate the buffer ring under
    // new ids; drop the snapshots keyed by the old ids so they cannot linger.
    if (width, height) != (self.windows[idx].width, self.windows[idx].height) {
      self.windows[idx].snaps.clear();
    }
    self.windows[idx].width = width;
    self.windows[idx].height = height;
    self.windows[idx].focused = activated;
    if activated {
      self.focused = idx;
    }
    if self.windows[idx].session.is_none() {
      self.spawn_session(ctx, idx);
    } else if resizing {
      // Some compositors never send a final configure without RESIZING. Reflow
      // after a quiet interval, or immediately when one does arrive.
      let token = if let Some(token) = self.windows[idx].resize_token {
        ctx.cancel_timer(token);
        token
      } else {
        let token = self.alloc_token();
        self.windows[idx].resize_token = Some(token);
        token
      };
      ctx.arm_timer(token, RESIZE_REFLOW_MS);
    } else {
      if let Some(token) = self.windows[idx].resize_token.take() {
        ctx.cancel_timer(token);
      }
      self.resize_grid(idx);
    }
    if let Some(idx) = self.win_index(id) {
      self.windows[idx].needs_draw = true;
    }
    self.flush_redraw(ctx);
  }

  fn on_scale(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, scale120: u32) {
    if let Some(idx) = self.win_index(id) {
      self.set_scale(idx, scale120);
    }
    self.flush_redraw(ctx);
  }

  fn on_focus(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, focused: bool) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    self.windows[idx].focused = focused;
    if focused {
      self.focused = idx;
    } else {
      self.windows[idx].keys_down.clear();
    }
    self.report_focus(idx, focused);
    self.windows[idx].needs_draw = true;
    self.flush_redraw(ctx);
  }

  fn on_close(&mut self, ctx: &mut dyn WindowCtx, id: WindowId) {
    self.close(ctx, id);
    self.flush_redraw(ctx);
  }

  fn on_key(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: &KeyEvent,
    mods: Modifiers,
  ) {
    self.modifiers = mods;
    if let Some(idx) = self.win_index(id) {
      self.handle_key(ctx, idx, event);
    }
    self.flush_redraw(ctx);
  }

  fn on_key_release(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: &KeyEvent,
    mods: Modifiers,
  ) {
    self.modifiers = mods;
    if let Some(idx) = self.win_index(id) {
      self.handle_key_release(idx, event);
    }
    self.flush_redraw(ctx);
  }

  fn on_ime(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, event: ImeEvent) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    match event {
      ImeEvent::Enable => self.ime_set_cursor(ctx, idx),
      ImeEvent::Disable => {
        let win = &mut self.windows[idx];
        win.preedit.clear();
        win.ime_preedit_pending.clear();
        win.ime_commit_pending.clear();
        win.needs_draw = true;
      },
      ImeEvent::Preedit(text) => self.windows[idx].ime_preedit_pending = text,
      ImeEvent::Commit(text) => {
        self.windows[idx].ime_commit_pending.push_str(&text);
      },
      ImeEvent::Done => self.ime_done(ctx, idx),
    }
    self.flush_redraw(ctx);
  }

  fn on_pointer(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: PointerEvent,
    mods: Modifiers,
  ) {
    self.modifiers = mods;
    if let Some(idx) = self.win_index(id) {
      self.on_pointer_event(ctx, idx, event);
    }
    self.flush_redraw(ctx);
  }

  fn on_touch(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: TouchEvent,
  ) {
    if let Some(idx) = self.win_index(id) {
      self.on_touch_event(idx, event);
    }
    self.flush_redraw(ctx);
  }

  fn on_paste(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    data: &[u8],
    _primary: bool,
  ) {
    if let Some(idx) = self.win_index(id) {
      self.paste_bytes(idx, data);
    }
    self.flush_redraw(ctx);
  }

  fn clipboard_text(&self, primary: bool) -> Option<String> {
    let text = if primary {
      &self.primary_clip
    } else {
      &self.clipboard
    };
    Some(text.clone())
  }

  fn on_readable(&mut self, ctx: &mut dyn WindowCtx, token: u64) {
    if token == IPC_LISTEN_TOKEN {
      self.accept_clients(ctx);
    } else if let Some(idx) =
      self.windows.iter().position(|w| w.pty_token == token)
    {
      self.read_pty(ctx, idx);
    } else if self.ipc_clients.contains_key(&token) {
      self.read_client(ctx, token);
    }
    self.flush_redraw(ctx);
  }

  #[expect(
    clippy::cast_possible_truncation,
    reason = "the animation beat is a small millisecond constant"
  )]
  fn on_timer(&mut self, ctx: &mut dyn WindowCtx, token: u64) {
    match token {
      BLINK_TOKEN => {
        self.blink_on = !self.blink_on;
        for w in &mut self.windows {
          w.needs_draw = true;
        }
        ctx.arm_timer(BLINK_TOKEN, BLINK_MS);
      },
      ANIM_TOKEN => {
        let mut animating = false;
        for w in &mut self.windows {
          let (changed, anim) = match w.session.as_mut() {
            Some(s) => {
              (s.term.animation_tick(ANIM_MS as u32), s.term.is_animating())
            },
            None => (false, false),
          };
          if changed {
            w.snaps.clear();
            w.needs_draw = true;
          }
          animating |= anim;
        }
        ctx.arm_timer(
          ANIM_TOKEN,
          if animating { ANIM_MS } else { ANIM_IDLE_MS },
        );
      },
      IPC_SWEEP_TOKEN => {
        self.expire_clients(ctx);
        ctx.arm_timer(IPC_SWEEP_TOKEN, 1000);
      },
      _ => {
        if let Some(idx) = self
          .windows
          .iter()
          .position(|w| w.flash_token == Some(token))
        {
          let win = &mut self.windows[idx];
          win.flashing = false;
          win.snaps.clear();
          win.needs_draw = true;
          win.flash_token = None;
          // The one-shot timer already fired; drop its source-map entry.
          ctx.cancel_timer(token);
        } else if let Some(idx) = self
          .windows
          .iter()
          .position(|w| w.sync_token == Some(token))
        {
          if let Some(s) = self.windows[idx].session.as_mut() {
            s.term.grid_mut().set_sync(false);
          }
          self.windows[idx].sync_token = None;
          self.windows[idx].needs_draw = true;
          ctx.cancel_timer(token);
        } else if let Some(idx) = self
          .windows
          .iter()
          .position(|w| w.resize_token == Some(token))
        {
          self.windows[idx].resize_token = None;
          self.resize_grid(idx);
          self.windows[idx].needs_draw = true;
          ctx.cancel_timer(token);
        } else if let Some(idx) = self
          .windows
          .iter()
          .position(|w| w.autoscroll_token == Some(token))
        {
          self.autoscroll_step(ctx, idx);
        }
      },
    }
    self.flush_redraw(ctx);
  }

  fn on_reload(&mut self, ctx: &mut dyn WindowCtx) {
    let idx = self.focused;
    if idx < self.windows.len() {
      self.reload_config(idx);
    }
    self.flush_redraw(ctx);
  }

  fn render(&mut self, ctx: &mut dyn WindowCtx, id: WindowId) {
    if let Some(idx) = self.win_index(id) {
      self.paint(ctx, idx);
    }
  }
}
