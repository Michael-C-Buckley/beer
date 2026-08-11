//! VT emulation: feed bytes through `vte` and drive the [`Grid`].

mod perform;

#[cfg(test)] mod conformance;

use std::{io::Write as _, str};

use beer_protocols::{
  caps::cap_value,
  charset::{Charset, charset, dec_special},
  codec::{base64_decode, decode_hex, file_uri_path},
  sgr::{ext_color, underline_from},
  style::prompt_kind,
};
use vte::Params;

use crate::{
  graphics::Graphics,
  grid::{
    Cell,
    Color,
    CursorShape,
    Flags,
    Grid,
    MouseEncoding,
    MouseProtocol,
    Underline,
  },
  theme::{Rgb, Theme, parse_color},
};

/// Which device-attributes query is being answered.
#[derive(Clone, Copy, Debug)]
enum DaLevel {
  Primary,
  Secondary,
  Tertiary,
}

/// A clipboard request from the application (OSC 52), for the front-end to act
/// on since it owns the Wayland selections.
#[derive(Clone, Debug)]
pub enum ClipboardOp {
  /// Set the clipboard (or primary) to `text`.
  Set { primary: bool, text: String },
  /// Report the current clipboard (or primary) contents back to the app.
  Query { primary: bool },
}

/// A desktop notification an application requested (OSC 9 / 777 / 99), for the
/// front-end to deliver via the configured notifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
  pub title: Option<String>,
  pub body:  String,
}

/// A terminal progress state reported through `OSC 9;4`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  Normal(u8),
  Error,
  Paused,
  Indeterminate,
}

/// Which dynamic colour an OSC 10/11/17/19 escape targets.
#[derive(Clone, Copy, Debug)]
enum Dynamic {
  Fg,
  Bg,
  SelBg,
  SelFg,
}

/// DECRQM mode-state code: 1 = set, 2 = reset.
const fn set_reset(on: bool) -> u8 {
  if on { 1 } else { 2 }
}

/// Parse an OSC colour spec into an [`Rgb`].
fn parse_spec(spec: &[u8]) -> Option<Rgb> {
  str::from_utf8(spec).ok().and_then(parse_color)
}

/// Parse a decimal palette index (0-255).
fn parse_index(b: &[u8]) -> Option<u8> {
  str::from_utf8(b).ok()?.parse().ok()
}

const fn rgb_tuple(rgb: Rgb) -> (u8, u8, u8) {
  (rgb.0, rgb.1, rgb.2)
}

/// Select `protocol` when a mouse mode is set, else turn reporting off.
const fn proto(on: bool, protocol: MouseProtocol) -> MouseProtocol {
  if on { protocol } else { MouseProtocol::Off }
}

/// Select `encoding` when its mode is set, else fall back to the default form.
const fn enc(on: bool, encoding: MouseEncoding) -> MouseEncoding {
  if on { encoding } else { MouseEncoding::X10 }
}

/// The terminal model: a grid plus the escape-sequence state around it.
#[derive(Debug)]
pub struct Term {
  grid:          Grid,
  title:         Option<String>,
  title_stack:   Vec<Option<String>>,
  response:      Vec<u8>,
  g0:            Charset,
  g1:            Charset,
  shift_out:     bool,
  /// Accumulated payload of an in-progress `DCS + q` (XTGETTCAP) query.
  xtgettcap:     Option<Vec<u8>>,
  /// Pending OSC 52 clipboard requests, drained by the front-end.
  clipboard_ops: Vec<ClipboardOp>,
  /// The active colour scheme (seeded from config, mutated by OSC escapes).
  theme:         Theme,
  /// Set when the child rings the bell (`BEL`); cleared by the front-end.
  bell:          bool,
  /// Working directory reported by the shell via OSC 7, for new windows.
  cwd:           Option<String>,
  /// Desktop notifications requested via OSC 9/777/99, drained by the
  /// front-end.
  notifications: Vec<Notification>,
  /// Progress reported through OSC 9;4, shown by the front-end in the title.
  progress:      Option<Progress>,
  /// Kitty graphics protocol state (images, placements, transmissions).
  graphics:      Graphics,
  /// APC capture state, since `vte` does not surface APC sequences.
  apc:           ApcScan,
  /// Payload of an APC being captured, between `ESC _` and its terminator.
  apc_buf:       Vec<u8>,
  /// Current cell size in pixels, for translating image sizes into cells.
  cell_px:       (u32, u32),
}

/// Where the APC capture splitter is in the byte stream. `vte` consumes APC
/// (`ESC _ ... ST`) silently, so [`Term::feed`] runs this small machine in
/// front of it: graphics payloads are diverted, everything else flows to `vte`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ApcScan {
  /// Forwarding bytes to `vte`.
  #[default]
  Normal,
  /// Saw `ESC`; the next byte decides APC vs an ordinary escape.
  Esc,
  /// Collecting an APC payload.
  Apc,
  /// Saw `ESC` inside an APC; `\` ends it (ST), else it stays in the payload.
  ApcEsc,
}

/// Cap on a captured APC payload (one chunk is at most ~4 KiB of base64).
const APC_MAX: usize = 1 << 20;

#[expect(
  clippy::absolute_paths,
  clippy::cast_possible_truncation,
  clippy::needless_pass_by_value,
  reason = "the terminal state machine is the boundary between protocol \
            values and bounded grid state"
)]
impl Term {
  pub fn new(cols: usize, rows: usize) -> Self {
    Self {
      grid:          Grid::new(cols, rows),
      title:         None,
      title_stack:   Vec::new(),
      response:      Vec::new(),
      g0:            Charset::Ascii,
      g1:            Charset::Ascii,
      shift_out:     false,
      xtgettcap:     None,
      clipboard_ops: Vec::new(),
      theme:         Theme::default(),
      bell:          false,
      cwd:           None,
      notifications: Vec::new(),
      progress:      None,
      graphics:      Graphics::new(),
      apc:           ApcScan::default(),
      apc_buf:       Vec::new(),
      cell_px:       (1, 1),
    }
  }

  /// Feed PTY bytes to the terminal. Graphics APC sequences (`ESC _ G ... ST`)
  /// are split out and handled here; all other bytes go to the `vte` parser.
  /// `cell_px` is the current cell size, recorded for graphics layout.
  pub fn feed(
    &mut self,
    parser: &mut vte::Parser,
    bytes: &[u8],
    cell_px: (u32, u32),
  ) {
    self.cell_px = cell_px;
    let mut i = 0;
    while i < bytes.len() {
      match self.apc {
        ApcScan::Normal => {
          let start = i;
          while i < bytes.len() && bytes[i] != 0x1B {
            i += 1;
          }
          if i > start {
            parser.advance(self, &bytes[start..i]);
          }
          if i < bytes.len() {
            self.apc = ApcScan::Esc;
            i += 1;
          }
        },
        ApcScan::Esc => {
          if bytes[i] == b'_' {
            self.apc = ApcScan::Apc;
            self.apc_buf.clear();
            i += 1;
          } else {
            // Not APC: hand the lone ESC to vte and let it pair with
            // the following bytes as an ordinary escape sequence.
            parser.advance(self, &[0x1B]);
            self.apc = ApcScan::Normal;
          }
        },
        ApcScan::Apc => {
          match bytes[i] {
            0x07 => {
              self.finish_apc();
              self.apc = ApcScan::Normal;
              i += 1;
            },
            0x1B => {
              self.apc = ApcScan::ApcEsc;
              i += 1;
            },
            b => {
              if self.apc_buf.len() < APC_MAX {
                self.apc_buf.push(b);
              }
              i += 1;
            },
          }
        },
        ApcScan::ApcEsc => {
          if bytes[i] == b'\\' {
            self.finish_apc();
            self.apc = ApcScan::Normal;
            i += 1;
          } else {
            // A stray ESC inside the payload: keep it and re-read this
            // byte in APC state.
            if self.apc_buf.len() < APC_MAX {
              self.apc_buf.push(0x1B);
            }
            self.apc = ApcScan::Apc;
          }
        },
      }
    }
  }

  /// Dispatch a captured APC payload. Only graphics commands (`G...`) are
  /// handled; other APC strings are ignored as `vte` would have.
  fn finish_apc(&mut self) {
    let buf = std::mem::take(&mut self.apc_buf);
    let Some((&b'G', body)) = buf.split_first() else {
      return;
    };
    let (control, payload) = body
      .iter()
      .position(|&b| b == b';')
      .map_or_else(|| (body, &[][..]), |p| (&body[..p], &body[p + 1..]));
    let cmd = beer_protocols::graphics::parse(control);
    let outcome = self.graphics.handle(cmd, payload, self.cell_px);
    if let Some(resp) = outcome.response {
      self.response.extend_from_slice(&resp);
    }
    if let Some(op) = outcome.grid_op {
      self.apply_grid_op(op);
    }
  }

  /// Apply a graphics grid mutation: stamp a placement or clear image cells.
  fn apply_grid_op(&mut self, op: crate::graphics::GridOp) {
    use crate::graphics::{ClearSpec, GridOp};
    match op {
      GridOp::Place {
        image,
        placement,
        cols,
        rows,
        keep_cursor,
      } => {
        self
          .grid
          .place_image(image, placement, cols, rows, keep_cursor);
      },
      GridOp::Clear(spec) => {
        match spec {
          ClearSpec::All => self.grid.clear_images(|_| true),
          ClearSpec::Image(id) => self.grid.clear_images(|r| r.image == id),
          ClearSpec::Placement(id, p) => {
            self
              .grid
              .clear_images(|r| r.image == id && r.placement == p);
          },
          ClearSpec::AtCursor => {
            let targets = self.grid.images_at_cursor();
            self
              .grid
              .clear_images(|r| targets.contains(&(r.image, r.placement)));
          },
        }
      },
    }
  }

  /// The graphics engine, for the renderer to read images and placements from.
  pub const fn graphics(&self) -> &Graphics {
    &self.graphics
  }

  /// Advance any playing graphics-protocol animations by `dt_ms`; returns
  /// whether a frame changed and the screen needs repainting.
  pub fn animation_tick(&mut self, dt_ms: u32) -> bool {
    self.graphics.tick(dt_ms)
  }

  /// Whether any image is currently playing a multi-frame animation, so the
  /// front-end knows to keep ticking quickly.
  pub fn is_animating(&self) -> bool {
    self.graphics.is_animating()
  }

  /// Answer a `CSI 14/16/18 t` geometry query: `14` reports the text area in
  /// pixels (`CSI 4 ; h ; w t`), `16` the cell size in pixels (`CSI 6 ; …`),
  /// `18` the text area in characters (`CSI 8 ; …`). Graphics clients read
  /// these to size and place images.
  fn report_geometry(&mut self, kind: u16) {
    let (cw, ch) = self.cell_px;
    let (cols, rows) = (self.grid.cols() as u32, self.grid.rows() as u32);
    let _ = match kind {
      14 => write!(self.response, "\x1b[4;{};{}t", rows * ch, cols * cw),
      16 => write!(self.response, "\x1b[6;{ch};{cw}t"),
      _ => write!(self.response, "\x1b[8;{rows};{cols}t"),
    };
  }

  /// The working directory last reported by the shell (OSC 7), if any.
  pub fn cwd(&self) -> Option<&str> {
    self.cwd.as_deref()
  }

  /// Drain the desktop notifications requested since the last call.
  pub fn take_notifications(&mut self) -> Vec<Notification> {
    std::mem::take(&mut self.notifications)
  }

  /// The progress state last reported by the application, if any.
  pub const fn progress(&self) -> Option<Progress> {
    self.progress
  }

  /// Take and clear the pending bell flag.
  pub fn take_bell(&mut self) -> bool {
    std::mem::take(&mut self.bell)
  }

  /// Drain the OSC 52 clipboard requests accumulated since the last call.
  pub fn take_clipboard_ops(&mut self) -> Vec<ClipboardOp> {
    std::mem::take(&mut self.clipboard_ops)
  }

  pub const fn theme(&self) -> &Theme {
    &self.theme
  }

  /// Replace the colour scheme (config load / reload).
  pub const fn set_theme(&mut self, theme: Theme) {
    self.theme = theme;
  }

  /// Answer an XTGETTCAP query: for each hex-encoded capability name, reply
  /// with `DCS 1 + r name=value ST` if known, else `DCS 0 + r name ST`.
  #[expect(
    clippy::branches_sharing_code,
    reason = "the response protocol has two branches with a shared terminator"
  )]
  fn answer_xtgettcap(&mut self, payload: &[u8]) {
    for name_hex in payload.split(|&b| b == b';') {
      let value = decode_hex(name_hex).and_then(|name| cap_value(&name));
      if let Some(value) = value {
        self.response.extend_from_slice(b"\x1bP1+r");
        self.response.extend_from_slice(name_hex);
        self.response.push(b'=');
        for byte in value.bytes() {
          let _ = write!(self.response, "{byte:02x}");
        }
        self.response.extend_from_slice(b"\x1b\\");
      } else {
        self.response.extend_from_slice(b"\x1bP0+r");
        self.response.extend_from_slice(name_hex);
        self.response.extend_from_slice(b"\x1b\\");
      }
    }
  }

  pub const fn grid(&self) -> &Grid {
    &self.grid
  }

  pub const fn grid_mut(&mut self) -> &mut Grid {
    &mut self.grid
  }

  pub fn resize(&mut self, cols: usize, rows: usize) {
    self.grid.resize(cols, rows);
  }

  pub fn scroll_view(&mut self, delta: isize) {
    self.grid.scroll_view(delta);
  }

  pub const fn scroll_to_bottom(&mut self) {
    self.grid.scroll_to_bottom();
  }

  /// Lines per page, for page-scroll bindings.
  pub fn page(&self) -> usize {
    self.grid.page()
  }

  pub fn title(&self) -> Option<&str> {
    self.title.as_deref()
  }

  /// Bytes the terminal needs to send back to the application (DA, CPR, ...).
  pub fn take_response(&mut self) -> Vec<u8> {
    std::mem::take(&mut self.response)
  }

  const fn active_charset(&self) -> Charset {
    if self.shift_out { self.g1 } else { self.g0 }
  }

  fn set_mode(&mut self, params: &Params, private: bool, on: bool) {
    for p in params {
      let Some(&code) = p.first() else { continue };
      match (private, code) {
        (true, 6) => self.grid.set_origin(on),
        (true, 7) => self.grid.set_autowrap(on),
        (true, 1049) => {
          if on {
            self.grid.save_cursor();
            self.grid.enter_alt_screen();
            self.grid.erase_display(2);
          } else {
            self.grid.leave_alt_screen();
            self.grid.restore_cursor();
          }
        },
        (true, 47 | 1047) => {
          if on {
            self.grid.enter_alt_screen();
          } else {
            self.grid.leave_alt_screen();
          }
        },
        (false, 4) => self.grid.set_insert(on),
        (true, 1) => self.grid.set_app_cursor(on),
        (true, 25) => self.grid.set_cursor_visible(on),
        (true, 9) => {
          self.grid.set_mouse_protocol(proto(on, MouseProtocol::X10));
        },
        (true, 1000) => {
          self
            .grid
            .set_mouse_protocol(proto(on, MouseProtocol::Normal));
        },
        (true, 1002) => {
          self
            .grid
            .set_mouse_protocol(proto(on, MouseProtocol::Button));
        },
        (true, 1003) => {
          self.grid.set_mouse_protocol(proto(on, MouseProtocol::Any));
        },
        (true, 1004) => self.grid.set_focus_events(on),
        (true, 1005) => {
          self.grid.set_mouse_encoding(enc(on, MouseEncoding::Utf8));
        },
        (true, 1006) => {
          self.grid.set_mouse_encoding(enc(on, MouseEncoding::Sgr));
        },
        (true, 2004) => self.grid.set_bracketed_paste(on),
        (true, 2026) => self.grid.set_sync(on),
        _ => tracing::trace!("unhandled mode {code} private={private} on={on}"),
      }
    }
  }

  fn sgr(&mut self, params: &Params) {
    let items: Vec<&[u16]> = params.iter().collect();
    if items.is_empty() {
      self.grid.reset_pen();
      return;
    }
    let mut i = 0;
    while i < items.len() {
      let p = items[i];
      let code = p.first().copied().unwrap_or(0);
      let pen = self.grid.pen_mut();
      let mut step = 1;
      match code {
        0 => *pen = Cell::default(),
        1 => pen.flags.insert(Flags::BOLD),
        2 => pen.flags.insert(Flags::DIM),
        3 => pen.flags.insert(Flags::ITALIC),
        4 => pen.underline = underline_from(p),
        5 | 6 => pen.flags.insert(Flags::BLINK),
        7 => pen.flags.insert(Flags::REVERSE),
        8 => pen.flags.insert(Flags::HIDDEN),
        9 => pen.flags.insert(Flags::STRIKE),
        21 => pen.underline = Underline::Double,
        22 => pen.flags.remove(Flags::BOLD.union(Flags::DIM)),
        23 => pen.flags.remove(Flags::ITALIC),
        24 => pen.underline = Underline::None,
        25 => pen.flags.remove(Flags::BLINK),
        27 => pen.flags.remove(Flags::REVERSE),
        28 => pen.flags.remove(Flags::HIDDEN),
        29 => pen.flags.remove(Flags::STRIKE),
        30..=37 => pen.fg = Color::Indexed((code - 30) as u8),
        39 => pen.fg = Color::Default,
        40..=47 => pen.bg = Color::Indexed((code - 40) as u8),
        49 => pen.bg = Color::Default,
        53 => pen.flags.insert(Flags::OVERLINE),
        55 => pen.flags.remove(Flags::OVERLINE),
        90..=97 => pen.fg = Color::Indexed((code - 90 + 8) as u8),
        100..=107 => pen.bg = Color::Indexed((code - 100 + 8) as u8),
        38 | 48 | 58 => {
          let (color, consumed) = ext_color(&items, i);
          if let Some(color) = color {
            match code {
              38 => pen.fg = color,
              48 => pen.bg = color,
              _ => pen.underline_color = color,
            }
          }
          step = consumed;
        },
        59 => pen.underline_color = Color::Default,
        _ => {},
      }
      i += step;
    }
  }

  /// Device attributes. DA1 claims a VT220 with ANSI colour; DA2 a generic
  /// firmware level; DA3 a (zero) unit ID.
  fn device_attrs(&mut self, level: DaLevel) {
    match level {
      DaLevel::Primary => self.response.extend_from_slice(b"\x1b[?62;22c"),
      DaLevel::Secondary => self.response.extend_from_slice(b"\x1b[>0;276;0c"),
      DaLevel::Tertiary => {
        self.response.extend_from_slice(b"\x1bP!|00000000\x1b\\");
      },
    }
  }

  /// Reply to a kitty-keyboard flags query (`CSI ? u`) with `CSI ? flags u`.
  fn report_kitty_flags(&mut self) {
    let _ = write!(self.response, "\x1b[?{}u", self.grid.kitty_flags());
  }

  /// XTVERSION (`CSI > q`): report the terminal name and version.
  fn report_version(&mut self) {
    let _ = write!(
      self.response,
      "\x1bP>|beer({})\x1b\\",
      env!("CARGO_PKG_VERSION")
    );
  }

  /// Emit an OSC colour reply (`OSC code ; rgb:rrrr/gggg/bbbb` + terminator).
  fn reply_color(&mut self, code: &str, rgb: Rgb, bell: bool) {
    let Rgb(r, g, b) = rgb;
    let _ = write!(
      self.response,
      "\x1b]{code};rgb:{:04x}/{:04x}/{:04x}",
      u16::from(r) * 0x101,
      u16::from(g) * 0x101,
      u16::from(b) * 0x101,
    );
    self
      .response
      .extend_from_slice(if bell { b"\x07" } else { b"\x1b\\" });
  }

  /// OSC 4: set or query palette entries, given as `index;spec` pairs.
  fn osc_palette(&mut self, params: &[&[u8]], bell: bool) {
    let mut rest = params[1..].iter();
    while let (Some(idx_raw), Some(spec)) = (rest.next(), rest.next()) {
      let Some(idx) = parse_index(idx_raw) else {
        continue;
      };
      if *spec == b"?" {
        let rgb = self.theme.palette[idx as usize];
        self.reply_color(&format!("4;{idx}"), rgb, bell);
      } else if let Some(rgb) = parse_spec(spec) {
        self.theme.set_palette(idx, rgb);
      }
    }
  }

  /// OSC 10/11/17/19: set or query a dynamic colour.
  fn osc_dynamic_color(
    &mut self,
    kind: Dynamic,
    spec: Option<&&[u8]>,
    bell: bool,
  ) {
    let Some(spec) = spec else { return };
    if **spec == b"?"[..] {
      let rgb = match kind {
        Dynamic::Fg => self.theme.fg,
        Dynamic::Bg => self.theme.bg,
        Dynamic::SelBg => self.theme.selection_bg,
        Dynamic::SelFg => self.theme.selection_fg.unwrap_or(self.theme.fg),
      };
      let code = match kind {
        Dynamic::Fg => "10",
        Dynamic::Bg => "11",
        Dynamic::SelBg => "17",
        Dynamic::SelFg => "19",
      };
      self.reply_color(code, rgb, bell);
    } else if let Some(rgb) = parse_spec(spec) {
      match kind {
        Dynamic::Fg => self.theme.fg = rgb,
        Dynamic::Bg => self.theme.bg = rgb,
        Dynamic::SelBg => self.theme.selection_bg = rgb,
        Dynamic::SelFg => self.theme.selection_fg = Some(rgb),
      }
    }
  }

  fn device_status(&mut self, params: &Params) {
    match params.iter().next().and_then(|p| p.first().copied()) {
      Some(5) => self.response.extend_from_slice(b"\x1b[0n"),
      Some(6) => {
        let (x, y) = self.grid.cursor();
        let _ = write!(self.response, "\x1b[{};{}R", y + 1, x + 1);
      },
      _ => {},
    }
  }

  /// DECRQM (`CSI [?] Ps $ p`): report whether a mode is set (1), reset (2),
  /// or unrecognized (0). Only the modes we actually track are reported.
  fn report_mode(&mut self, params: &Params, private: bool) {
    let code = raw(params, 0);
    let state = match (private, code) {
      (true, 6) => set_reset(self.grid.origin()),
      (true, 7) => set_reset(self.grid.autowrap()),
      (true, 47 | 1047 | 1049) => set_reset(self.grid.alt_active()),
      (true, 9) => set_reset(self.grid.mouse_protocol() == MouseProtocol::X10),
      (true, 1000) => {
        set_reset(self.grid.mouse_protocol() == MouseProtocol::Normal)
      },
      (true, 1002) => {
        set_reset(self.grid.mouse_protocol() == MouseProtocol::Button)
      },
      (true, 1003) => {
        set_reset(self.grid.mouse_protocol() == MouseProtocol::Any)
      },
      (true, 1004) => set_reset(self.grid.focus_events()),
      (true, 1005) => {
        set_reset(self.grid.mouse_encoding() == MouseEncoding::Utf8)
      },
      (true, 1006) => {
        set_reset(self.grid.mouse_encoding() == MouseEncoding::Sgr)
      },
      (true, 2004) => set_reset(self.grid.bracketed_paste()),
      (true, 2026) => set_reset(self.grid.sync_active()),
      (false, 4) => set_reset(self.grid.insert()),
      _ => 0,
    };
    let prefix = if private { "?" } else { "" };
    let _ = write!(self.response, "\x1b[{prefix}{code};{state}$y");
  }

  /// Title stack (`CSI 22/23 ; Ps t`): push or pop the window title.
  fn title_stack_op(&mut self, params: &Params) {
    match raw(params, 0) {
      22 => self.title_stack.push(self.title.clone()),
      23 => {
        if let Some(title) = self.title_stack.pop() {
          self.title = title;
        }
      },
      _ => {},
    }
  }
}

/// First param value, with 0/absent folded to `default` (xterm convention for
/// cursor movement and counts).
fn n(params: &Params, idx: usize, default: usize) -> usize {
  match params.iter().nth(idx).and_then(|p| p.first().copied()) {
    Some(0) | None => default,
    Some(v) => v as usize,
  }
}

/// Raw first param value (0 is meaningful), defaulting to 0 when absent.
fn raw(params: &Params, idx: usize) -> u16 {
  params
    .iter()
    .nth(idx)
    .and_then(|p| p.first().copied())
    .unwrap_or(0)
}

/// Decode an OSC string field to UTF-8 (lossy), or `None` if absent.
#[expect(
  clippy::single_option_map,
  reason = "this named adapter documents the OSC field conversion at its call \
            sites"
)]
fn osc_text(field: Option<&&[u8]>) -> Option<String> {
  field.map(|b| String::from_utf8_lossy(b).into_owned())
}

#[cfg(test)]
mod tests {
  use beer_protocols::codec::base64_encode;

  use super::*;
  use crate::grid::{Flags, MouseEncoding, MouseProtocol};

  fn feed(term: &mut Term, bytes: &[u8]) {
    let mut parser = vte::Parser::new();
    term.feed(&mut parser, bytes, (8, 16));
  }

  #[test]
  fn plain_text_lands_in_the_grid() {
    let mut t = Term::new(20, 4);
    feed(&mut t, b"hello");
    assert_eq!(t.grid().row_text(0), "hello");
  }

  #[test]
  fn cursor_position_and_erase() {
    let mut t = Term::new(20, 4);
    feed(&mut t, b"abcde\x1b[Hxyz");
    assert_eq!(t.grid().row_text(0), "xyzde");
  }

  #[test]
  fn newline_sequence() {
    let mut t = Term::new(20, 4);
    feed(&mut t, b"one\r\ntwo");
    assert_eq!(t.grid().row_text(0), "one");
    assert_eq!(t.grid().row_text(1), "two");
  }

  #[test]
  fn kitty_graphics_apc_transmits_and_displays() {
    // ESC _ G a=T,f=32,s=2,v=2,i=1 ; <base64 RGBA> ESC \: a 2x2 image,
    // transmitted and displayed at the cursor.
    let mut t = Term::new(20, 4);
    let px = vec![0xFFu8; 2 * 2 * 4];
    let b64 = base64_encode(&px);
    let seq = format!("\x1b_Ga=T,f=32,s=2,v=2,i=1;{b64}\x1b\\");
    feed(&mut t, seq.as_bytes());
    // With an 8x16 cell the 2x2 image occupies one cell, stamped at (0,0).
    let cell = t.grid().cell(0, 0);
    assert_eq!(cell.image.map(|r| r.image), Some(1));
    let resp = t.take_response();
    assert!(resp.windows(2).any(|w| w == b"OK"), "expected OK response");
  }

  #[test]
  fn kitty_graphics_scrolls_to_fit_image() {
    // 20x4 terminal, cursor at the last row. An image 3 cells tall needs
    // 3 rows; only 1 is available, so the grid should scroll up 2 rows to
    // make room, then place the image in the bottom 3 rows.
    let mut t = Term::new(20, 4);
    // Move cursor to last row.
    feed(&mut t, b"\x1b[4;1H"); // CSI 4;1 H = row 4, col 1 (1-based)
    assert_eq!(t.grid().cursor(), (0, 3));

    // A 16x48 RGBA image: 2 cells wide, 3 cells tall at 8x16 cell size.
    let px = vec![0xFFu8; 16 * 48 * 4];
    let b64 = base64_encode(&px);
    let seq = format!("\x1b_Ga=T,f=32,s=16,v=48,i=2;{b64}\x1b\\");
    feed(&mut t, seq.as_bytes());

    // The grid should have scrolled 2 rows; cursor is now on row 3 (last),
    // and image rows start at row 1 (dy=0 there, dy=2 at row 3).
    let top_cell = t.grid().cell(0, 1);
    assert_eq!(
      top_cell.image.map(|r| (r.image, r.dy)),
      Some((2, 0)),
      "top row of image should be at grid row 1 after scroll"
    );
    let bot_cell = t.grid().cell(0, 3);
    assert_eq!(
      bot_cell.image.map(|r| (r.image, r.dy)),
      Some((2, 2)),
      "bottom row of image should be at grid row 3"
    );
  }

  #[test]
  fn reports_pixel_geometry_for_graphics_clients() {
    // The test harness feeds with an 8x16 cell. A 20x4 grid is then 160x64
    // pixels. These answers are what an image client needs to size images.
    let mut t = Term::new(20, 4);
    feed(&mut t, b"\x1b[16t"); // cell size in pixels
    assert_eq!(t.take_response(), b"\x1b[6;16;8t");
    feed(&mut t, b"\x1b[14t"); // text area in pixels
    assert_eq!(t.take_response(), b"\x1b[4;64;160t");
    feed(&mut t, b"\x1b[18t"); // text area in cells
    assert_eq!(t.take_response(), b"\x1b[8;4;20t");
  }

  #[test]
  fn kitty_unicode_placeholder_virtual_placement() {
    // Transmit + a virtual placement (U=1): no cells are stamped, but the
    // placement is registered for placeholder cells to reference.
    let mut t = Term::new(20, 4);
    let px = base64_encode(&[0xFF; 4]);
    let seq = format!("\x1b_Ga=T,U=1,i=7,c=1,r=1,f=32,s=1,v=1;{px}\x1b\\");
    feed(&mut t, seq.as_bytes());
    assert!(
      t.grid().cell(0, 0).image.is_none(),
      "virtual placement stamps nothing"
    );
    assert!(t.graphics().placement(7, 0).is_some());
    // The app prints a placeholder carrying image id 7 in its fg colour.
    feed(&mut t, "\x1b[38;5;7m\u{10EEEE}\u{0305}\u{0305}".as_bytes());
    assert_eq!(t.grid().cell(0, 0).c, '\u{10EEEE}');
  }

  #[test]
  fn apc_does_not_disturb_surrounding_text() {
    // Text, then a graphics query APC, then more text: the text is intact and
    // the APC did not leak bytes into the grid.
    let mut t = Term::new(20, 2);
    let px = base64_encode(&[0u8; 4]);
    let seq = format!("ab\x1b_Ga=q,f=32,s=1,v=1,i=2;{px}\x1b\\cd");
    feed(&mut t, seq.as_bytes());
    assert_eq!(t.grid().row_text(0), "abcd");
  }

  #[test]
  fn text_sizing_osc66_lays_out_a_scaled_block() {
    // `OSC 66 ; s=2 ; X BEL`: a 2x2 scaled block, cursor advances two cells.
    let mut t = Term::new(20, 4);
    feed(&mut t, b"\x1b]66;s=2;X\x07");
    let g = t.grid();
    assert_eq!(g.cell(0, 0).c, 'X');
    let s = g.cell(0, 0).sized.as_ref().expect("leading cell is scaled");
    assert_eq!((s.cols, s.rows), (2, 2));
    assert!(g.cell(1, 1).flags.contains(Flags::SIZED_CONT));
    assert_eq!(g.cursor(), (2, 0));
  }

  #[test]
  fn device_attributes_levels() {
    let mut t = Term::new(20, 4);
    feed(&mut t, b"\x1b[c");
    assert_eq!(t.take_response(), b"\x1b[?62;22c");
    feed(&mut t, b"\x1b[>c");
    assert_eq!(t.take_response(), b"\x1b[>0;276;0c");
    feed(&mut t, b"\x1b[=c");
    assert_eq!(t.take_response(), b"\x1bP!|00000000\x1b\\");
  }

  #[test]
  fn xtversion_reports_name() {
    let mut t = Term::new(20, 4);
    feed(&mut t, b"\x1b[>q");
    let resp = t.take_response();
    assert!(resp.starts_with(b"\x1bP>|beer("));
    assert!(resp.ends_with(b")\x1b\\"));
  }

  #[test]
  fn decrqm_reports_known_modes() {
    let mut t = Term::new(20, 4);
    feed(&mut t, b"\x1b[?7$p"); // autowrap, on by default
    assert_eq!(t.take_response(), b"\x1b[?7;1$y");
    feed(&mut t, b"\x1b[?7l\x1b[?7$p"); // turn it off, re-query
    assert_eq!(t.take_response(), b"\x1b[?7;2$y");
    feed(&mut t, b"\x1b[?9999$p"); // unknown mode
    assert_eq!(t.take_response(), b"\x1b[?9999;0$y");
  }

  #[test]
  fn sgr_underline_styles_and_lines() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b[4:3;58;5;1;53mX");
    let cell = t.grid().cell(0, 0);
    assert_eq!(cell.underline, Underline::Curly);
    assert_eq!(cell.underline_color, Color::Indexed(1));
    assert!(cell.flags.contains(Flags::OVERLINE));
    // 4:0 turns the underline back off.
    feed(&mut t, b"\x1b[4:0mY");
    assert_eq!(t.grid().cell(1, 0).underline, Underline::None);
  }

  #[test]
  fn decscusr_and_cursor_visibility() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b[4 q");
    assert_eq!(t.grid().cursor_shape(), CursorShape::Underline);
    feed(&mut t, b"\x1b[6 q");
    assert_eq!(t.grid().cursor_shape(), CursorShape::Beam);
    feed(&mut t, b"\x1b[0 q");
    assert_eq!(t.grid().cursor_shape(), CursorShape::Block);

    feed(&mut t, b"\x1b[?25l");
    assert!(!t.grid().cursor_visible());
    feed(&mut t, b"\x1b[?25h");
    assert!(t.grid().cursor_visible());
  }

  #[test]
  fn osc12_sets_and_resets_cursor_color() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b]12;#ff0000\x07");
    assert_eq!(t.grid().cursor_color(), Some((255, 0, 0)));
    feed(&mut t, b"\x1b]12;rgb:00/80/ff\x07");
    assert_eq!(t.grid().cursor_color(), Some((0, 0x80, 0xFF)));
    feed(&mut t, b"\x1b]112\x07");
    assert_eq!(t.grid().cursor_color(), None);
  }

  #[test]
  fn osc12_queries_cursor_color() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b]12;#ff0000\x07");
    feed(&mut t, b"\x1b]12;?\x07");
    let resp = t.take_response();
    assert!(resp.starts_with(b"\x1b]12;rgb:"), "{resp:?}");
    // A query must report, not reset, the cursor colour.
    assert_eq!(t.grid().cursor_color(), Some((255, 0, 0)));
  }

  #[test]
  fn decscusr_and_cursor_color() {
    use crate::grid::CursorShape;
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b[5 q"); // blinking bar
    assert_eq!(t.grid().cursor_shape(), CursorShape::Beam);
    feed(&mut t, b"\x1b[4 q"); // steady underline
    assert_eq!(t.grid().cursor_shape(), CursorShape::Underline);
    feed(&mut t, b"\x1b]12;#ff3030\x07");
    assert_eq!(t.grid().cursor_color(), Some((0xFF, 0x30, 0x30)));
    feed(&mut t, b"\x1b]112\x07");
    assert_eq!(t.grid().cursor_color(), None);
    feed(&mut t, b"\x1b[?25l"); // hide cursor
    assert!(!t.grid().cursor_visible());
  }

  #[test]
  fn osc_palette_and_dynamic_colors() {
    use crate::theme::Rgb;
    let mut t = Term::new(20, 2);
    // Set palette index 1 and foreground via OSC, then query them back.
    feed(&mut t, b"\x1b]4;1;#ff0000\x1b\\");
    assert_eq!(t.theme().palette[1], Rgb(0xFF, 0, 0));
    feed(&mut t, b"\x1b]10;rgb:00/80/ff\x1b\\");
    assert_eq!(t.theme().fg, Rgb(0, 0x80, 0xFF));
    feed(&mut t, b"\x1b]11;?\x07");
    let resp = t.take_response();
    assert!(resp.starts_with(b"\x1b]11;rgb:"));
    // Reset returns the palette entry to its default.
    feed(&mut t, b"\x1b]104;1\x1b\\");
    assert_ne!(t.theme().palette[1], Rgb(0xFF, 0, 0));
  }

  #[test]
  fn xtgettcap_known_and_unknown() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1bP+q544e\x1b\\"); // "TN"
    assert_eq!(t.take_response(), b"\x1bP1+r544e=62656572\x1b\\"); // = "beer"
    feed(&mut t, b"\x1bP+q6162\x1b\\"); // "ab", unknown
    assert_eq!(t.take_response(), b"\x1bP0+r6162\x1b\\");
  }

  #[test]
  fn bracketed_paste_and_sync_modes() {
    let mut t = Term::new(20, 2);
    feed(&mut t, b"\x1b[?2004h");
    assert!(t.grid().bracketed_paste());
    feed(&mut t, b"\x1b[?2004$p");
    assert_eq!(t.take_response(), b"\x1b[?2004;1$y");
    feed(&mut t, b"\x1b[?2026h");
    assert!(t.grid().sync_active());
    feed(&mut t, b"\x1b[?2026l\x1b[?2026$p");
    assert!(!t.grid().sync_active());
    assert_eq!(t.take_response(), b"\x1b[?2026;2$y");
  }

  #[test]
  fn osc52_set_and_query() {
    let mut t = Term::new(20, 2);
    // Set clipboard to "hi" (base64 "aGk=").
    feed(&mut t, b"\x1b]52;c;aGk=\x07");
    let ops = t.take_clipboard_ops();
    match ops.as_slice() {
      [
        ClipboardOp::Set {
          primary: false,
          text,
        },
      ] => assert_eq!(text, "hi"),
      other => panic!("unexpected ops: {other:?}"),
    }
    // Query the primary selection.
    feed(&mut t, b"\x1b]52;p;?\x07");
    let ops = t.take_clipboard_ops();
    assert!(matches!(ops.as_slice(), [ClipboardOp::Query {
      primary: true,
    }]));
  }

  #[test]
  fn mouse_modes_track_protocol_and_encoding() {
    let mut t = Term::new(20, 4);
    feed(&mut t, b"\x1b[?1002h\x1b[?1006h");
    assert_eq!(t.grid().mouse_protocol(), MouseProtocol::Button);
    assert_eq!(t.grid().mouse_encoding(), MouseEncoding::Sgr);
    feed(&mut t, b"\x1b[?1002$p");
    assert_eq!(t.take_response(), b"\x1b[?1002;1$y");
    feed(&mut t, b"\x1b[?1003h"); // any-event supersedes button-event
    assert_eq!(t.grid().mouse_protocol(), MouseProtocol::Any);
    feed(&mut t, b"\x1b[?1000l"); // turning a mouse mode off clears reporting
    assert_eq!(t.grid().mouse_protocol(), MouseProtocol::Off);
    feed(&mut t, b"\x1b[?1004h");
    assert!(t.grid().focus_events());
  }

  #[test]
  fn title_stack_push_pop() {
    let mut t = Term::new(20, 4);
    feed(&mut t, b"\x1b]0;first\x07");
    feed(&mut t, b"\x1b[22t"); // push "first"
    feed(&mut t, b"\x1b]0;second\x07");
    assert_eq!(t.title(), Some("second"));
    feed(&mut t, b"\x1b[23t"); // pop -> "first"
    assert_eq!(t.title(), Some("first"));
  }

  #[test]
  fn sgr_sets_pen_colours() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b[31;1mX");
    let cell = t.grid().cell(0, 0);
    assert_eq!(cell.fg, Color::Indexed(1));
    assert!(cell.flags.contains(Flags::BOLD));
  }

  #[test]
  fn truecolor_semicolon_and_colon() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b[38;2;10;20;30mA");
    assert_eq!(t.grid().cell(0, 0).fg, Color::Rgb(10, 20, 30));
    feed(&mut t, b"\x1b[38:2:40:50:60mB");
    assert_eq!(t.grid().cell(1, 0).fg, Color::Rgb(40, 50, 60));
  }

  #[test]
  fn device_status_reports_cursor() {
    let mut t = Term::new(20, 4);
    feed(&mut t, b"\x1b[3;5H\x1b[6n");
    assert_eq!(t.take_response(), b"\x1b[3;5R");
  }

  #[test]
  fn line_drawing_charset() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b(0qx\x1b(B");
    assert_eq!(t.grid().row_text(0), "─│");
  }

  #[test]
  fn osc133_marks_capture_last_command_output() {
    let mut t = Term::new(12, 6);
    feed(&mut t, b"\x1b]133;A\x07$ echo hi\r\n"); // prompt + typed command
    feed(&mut t, b"\x1b]133;C\x07hi\r\n"); // output start, then output
    feed(&mut t, b"\x1b]133;D\x07"); // command finished
    assert_eq!(t.grid().last_command_output().as_deref(), Some("hi\n"));
  }

  #[test]
  fn osc_notifications_collected() {
    let mut t = Term::new(20, 2);
    // Neovim uses OSC 9;4 for its native progress indicator. This is not an
    // iTerm-style OSC 9 notification body.
    feed(&mut t, b"\x1b]9;4;1;0\x1b\\");
    feed(&mut t, b"\x1b]9;hello\x07");
    feed(&mut t, b"\x1b]777;notify;Title;Body\x07");
    let n = t.take_notifications();
    assert_eq!(n, vec![
      Notification {
        title: None,
        body:  "hello".into(),
      },
      Notification {
        title: Some("Title".into()),
        body:  "Body".into(),
      },
    ]);
    assert!(t.take_notifications().is_empty());
  }

  #[test]
  fn osc_progress_tracks_neovim_state_without_notifying() {
    let mut t = Term::new(20, 2);
    feed(&mut t, b"\x1b]9;4;1;42\x1b\\");
    assert_eq!(t.progress(), Some(Progress::Normal(42)));
    assert!(t.take_notifications().is_empty());
    feed(&mut t, b"\x1b]9;4;0;0\x1b\\");
    assert_eq!(t.progress(), None);
  }

  #[test]
  fn osc7_tracks_cwd_and_decodes_percent() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b]7;file://hermes/home/user/my%20dir\x07");
    assert_eq!(t.cwd(), Some("/home/user/my dir"));
    // A non-file or relative URI leaves the previous value untouched? It
    // simply does not match a path, so cwd stays None here.
    let mut t2 = Term::new(20, 1);
    feed(&mut t2, b"\x1b]7;file://host\x07");
    assert_eq!(t2.cwd(), None);
  }

  #[test]
  fn title_via_osc() {
    let mut t = Term::new(20, 1);
    feed(&mut t, b"\x1b]0;hello\x07");
    assert_eq!(t.title(), Some("hello"));
  }

  #[test]
  fn alt_screen_preserves_primary() {
    let mut t = Term::new(20, 2);
    feed(&mut t, b"main");
    feed(&mut t, b"\x1b[?1049h");
    assert_eq!(t.grid().row_text(0), "");
    feed(&mut t, b"\x1b[?1049l");
    assert_eq!(t.grid().row_text(0), "main");
  }
}
