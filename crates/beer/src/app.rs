//! The terminal application: implements [`beer_window::App`], driven by a
//! backend. It owns the terminal sessions, rendering, selection/search/url/IME
//! state, and the daemon glue; every platform action (present a frame, set the
//! title, own the clipboard, open/close windows, watch fds, arm timers) goes
//! through the [`beer_window::WindowCtx`] each callback receives. It never
//! names a Wayland type.

use std::{
  collections::{HashMap, HashSet},
  fs,
  io::{self, ErrorKind},
  mem,
  num::NonZeroU16,
  os::{
    fd::OwnedFd,
    unix::{
      net::{UnixListener, UnixStream},
      process::ExitStatusExt as _,
    },
  },
  path::PathBuf,
  process::{Command, ExitStatus, Stdio},
  time::Instant,
};

use beer_protocols::codec;
use beer_window::{Modifiers, WindowCtx, WindowId};

use crate::{
  bindings::Bindings,
  config::{Config, Main},
  font::{CellMetrics, FontOptions, Fonts},
  grid::{Cell, CursorShape, Flags, Grid, UrlHit},
  ipc,
  pty::Pty,
  render::Renderer,
  vt::{ClipboardOp, Notification, Term},
};

mod callbacks;
mod events;
mod keyboard;
mod paint;
mod pointer;
mod session;

mod lifecycle;

const MULTI_CLICK_MS: u32 = 400;
/// Blink half-period.
const BLINK_MS: u64 = 500;
/// SGR 6 rapid-blink half-period.
const RAPID_BLINK_MS: u64 = 250;
/// Visual-bell flash duration.
const FLASH_MS: u64 = 60;
/// Autoscroll step period while dragging past an edge.
const AUTOSCROLL_MS: u64 = 40;
/// Pause before reflowing after a live resize configure.
const RESIZE_REFLOW_MS: u64 = 150;
const TOUCH_HOLD_MS: u64 = 500;
/// Graphics-animation beat while animating.
const ANIM_MS: u32 = 100;
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
const RAPID_BLINK_TOKEN: u64 = 5;
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

struct TouchState {
  id:        i32,
  x:         f64,
  start_y:   f64,
  last_y:    f64,
  acc:       f64,
  selecting: bool,
}

#[derive(Clone, Copy)]
enum WindowTimer {
  Flash,
  Sync,
  Resize,
  Touch,
  Autoscroll,
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
  blink:   u8,
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
  pending_command:     Vec<String>,
  window_number:       u64,
  hold:                bool,
  held_exit:           Option<u8>,
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
  /// Awaiting y/n confirmation to close a window with a running job.
  confirm_close:       bool,
  /// Whether the pointer is currently hidden because the user is typing.
  pointer_hidden:      bool,
  hovered_link:        Option<NonZeroU16>,
  press_cell:          Option<(usize, usize)>,
  pressed_button:      Option<u8>,
  last_report_cell:    Option<(usize, usize)>,
  autoscroll:          isize,
  pointer_pos:         (f64, f64),
  last_click:          Option<(Instant, usize, usize, u32)>,
  searching:           bool,
  url_mode:            bool,
  url_copy:            bool,
  url_hits:            Vec<UrlHit>,
  url_labels:          Vec<String>,
  url_input:           String,
  unicode_input:       Option<String>,
  keys_down:           HashSet<u32>,
  touch:               Option<TouchState>,
  touch_token:         Option<u64>,
  preedit:             String,
  ime_preedit_pending: String,
  ime_commit_pending:  String,
  /// Bytes the IME asked to delete before/after the cursor, applied on Done.
  ime_delete_pending:  (u32, u32),
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
    launch: WindowLaunch,
    window_number: u64,
    pty_token: u64,
  ) -> Self {
    Self {
      id,
      session: None,
      pending_cwd: launch.cwd,
      pending_env: launch.env,
      pending_command: launch.command,
      window_number,
      hold: launch.hold,
      held_exit: None,
      client: launch.client,
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
      confirm_close: false,
      pointer_hidden: false,
      hovered_link: None,
      press_cell: None,
      pressed_button: None,
      last_report_cell: None,
      autoscroll: 0,
      pointer_pos: (0.0, 0.0),
      last_click: None,
      searching: false,
      url_mode: false,
      url_copy: false,
      url_hits: Vec::new(),
      url_labels: Vec::new(),
      url_input: String::new(),
      unicode_input: None,
      keys_down: HashSet::new(),
      touch: None,
      touch_token: None,
      preedit: String::new(),
      ime_preedit_pending: String::new(),
      ime_commit_pending: String::new(),
      ime_delete_pending: (0, 0),
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
#[expect(
  clippy::struct_excessive_bools,
  reason = "independent event-loop lifecycle flags"
)]
pub struct App {
  /// One renderer per output scale (120ths). Windows share the renderer for
  /// their scale, so moving between differently-scaled outputs never
  /// re-rasterizes the font.
  renderers:       HashMap<u32, Renderer>,
  config:          Config,
  config_paths:    Vec<PathBuf>,
  bindings:        Bindings,
  font_size:       u32,
  blink_on:        bool,
  rapid_on:        bool,
  blink_armed:     bool,
  rapid_armed:     bool,
  anim_armed:      bool,
  ipc_sweep_armed: bool,
  modifiers:       Modifiers,
  windows:         Vec<WinState>,
  focused:         usize,
  clipboard:       String,
  primary_clip:    String,
  exit_code:       u8,
  server:          bool,
  resident:        bool,
  /// Monotonic token allocator for per-window/per-client sources.
  next_token:      u64,
  next_window:     u64,
  initial:         ipc::OpenRequest,
  /// Daemon: the listening socket and per-client incremental readers, keyed by
  /// the source token the client is watched under.
  ipc_clients:     HashMap<u64, (UnixStream, ipc::RequestReader, Instant)>,
  ipc_listener:    Option<UnixListener>,
  /// Path of the bound daemon socket, unlinked on shutdown so a later server
  /// can bind the same path.
  ipc_socket:      Option<PathBuf>,
}

impl Drop for App {
  fn drop(&mut self) {
    if let Some(path) = self.ipc_socket.take() {
      let _ = fs::remove_file(path);
    }
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

/// Per-window overrides a daemon client may request; each falls back to config
/// when unset.
#[derive(Default)]
struct WindowOverrides {
  title:  Option<String>,
  app_id: Option<String>,
}

struct WindowLaunch {
  cwd:       Option<PathBuf>,
  env:       Vec<(String, String)>,
  command:   Vec<String>,
  hold:      bool,
  overrides: WindowOverrides,
  client:    Option<UnixStream>,
}

/// A child's exit status folded to a byte: its exit code, or 128 plus the
/// terminating signal.
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  reason = "a process exit code or 128+signal fits a byte"
)]
fn status_code(status: ExitStatus) -> u8 {
  status
    .code()
    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)) as u8
}

/// Filter pasted text before it reaches the shell: normalize newlines to CR
/// (collapsing CRLF) and drop control characters (C0 except tab/newline, DEL,
/// and C1) so a paste cannot inject an escape sequence. The bytes are decoded
/// as UTF-8 first, so multibyte text survives and a stray 8-bit control byte
/// becomes a replacement character rather than reaching the parser as a C1.
fn sanitize_paste(data: &[u8]) -> Vec<u8> {
  let text = String::from_utf8_lossy(data);
  let mut clean = String::with_capacity(text.len());
  let mut prev_cr = false;
  for c in text.chars() {
    match c {
      '\n' if prev_cr => {},
      '\n' => clean.push('\r'),
      '\t' | '\r' => clean.push(c),
      c if c.is_control() => {},
      c => clean.push(c),
    }
    prev_cr = c == '\r';
  }
  clean.into_bytes()
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
  rapid_on: bool,
  overlay: Option<&str>,
  preedit: Option<(usize, &str)>,
) -> RowSnap {
  let abs = grid.view_to_abs(y);
  let cells = grid.view_row(y).to_vec();
  let blink = row_blink_state(&cells, blink_on, rapid_on);
  RowSnap {
    cells,
    cursor: row_cursor(grid, y, focused, blink_on),
    sel: grid.selection_span_on(abs),
    search: grid.search_spans_on(abs),
    overlay: overlay.map(str::to_owned),
    preedit: preedit.map(|(c, t)| (c, t.to_owned())),
    blink,
  }
}

fn row_matches(
  snap: Option<&RowSnap>,
  grid: &Grid,
  y: usize,
  focused: bool,
  phases: (bool, bool),
  overlay: Option<&str>,
  preedit: Option<(usize, &str)>,
) -> bool {
  let Some(snap) = snap else { return false };
  let abs = grid.view_to_abs(y);
  let (blink_on, rapid_on) = phases;
  let blink = row_blink_state(grid.view_row(y), blink_on, rapid_on);
  snap.cells == grid.view_row(y)
    && snap.cursor == row_cursor(grid, y, focused, blink_on)
    && snap.sel == grid.selection_span_on(abs)
    && snap.search == grid.search_spans_on(abs)
    && snap.overlay.as_deref() == overlay
    && snap.preedit.as_ref().map(|(c, t)| (*c, t.as_str())) == preedit
    && snap.blink == blink
}

fn row_blink_state(cells: &[Cell], blink_on: bool, rapid_on: bool) -> u8 {
  let mut state = 0;
  if blink_on && cells.iter().any(|c| c.flags.contains(Flags::BLINK)) {
    state |= 1;
  }
  if rapid_on && cells.iter().any(|c| c.flags.contains(Flags::RAPID_BLINK)) {
    state |= 2;
  }
  state
}

fn sync_timer(
  ctx: &mut dyn WindowCtx,
  armed: &mut bool,
  needed: bool,
  token: u64,
  delay_ms: u64,
) {
  if needed == *armed {
    return;
  }
  *armed = needed;
  if needed {
    ctx.arm_timer(token, delay_ms);
  } else {
    ctx.cancel_timer(token);
  }
}

fn status_bar_text(
  win: &WinState,
  grid: &Grid,
  indicator: bool,
) -> Option<String> {
  if win.confirm_close {
    Some("close terminal with a running job? (y/n)".to_string())
  } else if let Some(hex) = win.unicode_input.as_ref() {
    Some(format!("unicode: U+{}", hex.to_uppercase()))
  } else if win.searching {
    let (n, total) = grid.search_count();
    Some(format!(
      "search: {}  [{n}/{total}]",
      grid.search_query().unwrap_or("")
    ))
  } else if indicator && !grid.view_at_bottom() {
    let (position, total) = grid.scroll_position();
    Some(format!("scrollback: {position}/{total}"))
  } else {
    None
  }
}

impl App {
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
    let (commit, delete) = {
      let win = &mut self.windows[idx];
      let commit = mem::take(&mut win.ime_commit_pending);
      let delete = mem::take(&mut win.ime_delete_pending);
      win.preedit = mem::take(&mut win.ime_preedit_pending);
      (commit, delete)
    };
    // The protocol applies the surrounding-text deletion before the commit.
    self.ime_delete_surrounding(idx, delete);
    if !commit.is_empty() {
      self.send_to_shell(idx, commit.as_bytes());
    }
    self.windows[idx].needs_draw = true;
    self.ime_set_cursor(ctx, idx);
  }

  /// Honor an IME delete-surrounding request by deleting from the shell:
  /// `before` backspaces then `after` forward-deletes. The protocol counts
  /// bytes; without the shell's line buffer this is a best-effort deletion that
  /// is exact for single-byte text, and bounded so a stray request cannot flood
  /// the shell.
  fn ime_delete_surrounding(
    &mut self,
    idx: usize,
    (before, after): (u32, u32),
  ) {
    if before == 0 && after == 0 {
      return;
    }
    let mut bytes = vec![0x7F; before.min(4096) as usize];
    for _ in 0..after.min(4096) {
      bytes.extend_from_slice(b"\x1b[3~");
    }
    self.send_to_shell(idx, &bytes);
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
    idx: usize,
    ops: Vec<ClipboardOp>,
  ) {
    for op in ops {
      let allowed = match &op {
        ClipboardOp::Set { .. } => self.config.security.osc52.allows_copy(),
        ClipboardOp::Query { .. } => self.config.security.osc52.allows_query(),
      };
      if !allowed {
        continue;
      }
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
          self.write_to_pty(idx, reply.as_bytes());
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
    let clean = sanitize_paste(data);
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
  fn send_notification(&self, idx: usize, note: &Notification) {
    let Some((program, args)) = self.config.notify.command.split_first() else {
      return;
    };
    let title = note
      .title
      .clone()
      .or_else(|| self.windows.get(idx).and_then(|w| w.title.clone()))
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

/// Generate `n` distinct same-length keyboard hint labels.
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

#[cfg(test)]
mod tests {
  use super::sanitize_paste;

  #[test]
  fn paste_normalizes_newlines() {
    assert_eq!(sanitize_paste(b"a\nb"), b"a\rb");
    // A CRLF pair collapses to a single carriage return.
    assert_eq!(sanitize_paste(b"a\r\nb"), b"a\rb");
    assert_eq!(sanitize_paste(b"a\tb"), b"a\tb");
  }

  #[test]
  fn paste_strips_control_injection() {
    // ESC, DEL, and a C1 control (U+009B, the 8-bit CSI) are all removed, so a
    // paste cannot break out of bracketed paste or inject a control sequence.
    assert_eq!(sanitize_paste(b"a\x1b[201~b"), b"a[201~b");
    assert_eq!(sanitize_paste(b"a\x7fb"), b"ab");
    assert_eq!(sanitize_paste("a\u{009b}b".as_bytes()), b"ab");
  }

  #[test]
  fn paste_preserves_unicode() {
    // Multibyte text whose bytes fall in 0x80-0x9f must survive intact.
    let s = "héllo — wörld";
    assert_eq!(sanitize_paste(s.as_bytes()), s.as_bytes());
  }
}
