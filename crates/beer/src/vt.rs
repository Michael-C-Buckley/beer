//! VT emulation: feed bytes through `vte` and drive the [`Grid`].

mod helpers;
mod perform;

#[cfg(test)] mod conformance;

use std::{collections::HashMap, io::Write as _, str};

use beer_protocols::{
  caps::cap_value,
  charset::{Charset, charset, translate},
  codec::{base64_decode, decode_hex, file_uri_path},
  sgr::{ext_color, underline_from},
  style::prompt_kind,
};
use helpers::{
  DaLevel,
  Dynamic,
  change_rect_blink,
  enc,
  parse_index,
  parse_spec,
  proto,
  rgb_tuple,
  set_pen_blink,
  set_reset,
  sgr_report,
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
    Rect,
    Underline,
  },
  theme::{Rgb, Theme},
};

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

#[derive(Debug, Default)]
struct PendingNotification {
  title: String,
  body:  String,
}

/// A terminal progress state reported through `OSC 9;4`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  Normal(u8),
  Error,
  Paused,
  Indeterminate,
}

/// The terminal model: a grid plus the escape-sequence state around it.
#[derive(Debug)]
pub struct Term {
  grid:                  Grid,
  title:                 Option<String>,
  title_stack:           Vec<Option<String>>,
  response:              Vec<u8>,
  g0:                    Charset,
  g1:                    Charset,
  g2:                    Charset,
  g3:                    Charset,
  /// Which G-set GL maps to (0-3), set by SI/SO and the locking shifts.
  gl:                    u8,
  /// One-shot G-set for the next printed character (SS2/SS3), then cleared.
  single_shift:          Option<u8>,
  /// Accumulated payload of an in-progress `DCS + q` (XTGETTCAP) query.
  xtgettcap:             Option<Vec<u8>>,
  /// Accumulated payload of an in-progress `DCS $ q` (DECRQSS) query.
  decrqss:               Option<Vec<u8>>,
  /// Pending OSC 52 clipboard requests, drained by the front-end.
  clipboard_ops:         Vec<ClipboardOp>,
  /// The active colour scheme (seeded from config, mutated by OSC escapes).
  theme:                 Theme,
  /// Set when the child rings the bell (`BEL`); cleared by the front-end.
  bell:                  bool,
  /// Working directory reported by the shell via OSC 7, for new windows.
  cwd:                   Option<String>,
  /// Desktop notifications requested via OSC 9/777/99, drained by the
  /// front-end.
  notifications:         Vec<Notification>,
  pending_notifications: HashMap<String, PendingNotification>,
  /// Progress reported through OSC 9;4, shown by the front-end in the title.
  progress:              Option<Progress>,
  /// Kitty graphics protocol state (images, placements, transmissions).
  graphics:              Graphics,
  /// APC capture state, since `vte` does not surface APC sequences.
  apc:                   ApcScan,
  /// Payload of an APC being captured, between `ESC _` and its terminator.
  apc_buf:               Vec<u8>,
  /// Current cell size in pixels, for translating image sizes into cells.
  cell_px:               (u32, u32),
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
      grid:                  Grid::new(cols, rows),
      title:                 None,
      title_stack:           Vec::new(),
      response:              Vec::new(),
      g0:                    Charset::Ascii,
      g1:                    Charset::Ascii,
      g2:                    Charset::Ascii,
      g3:                    Charset::Ascii,
      gl:                    0,
      single_shift:          None,
      xtgettcap:             None,
      decrqss:               None,
      clipboard_ops:         Vec::new(),
      theme:                 Theme::default(),
      bell:                  false,
      cwd:                   None,
      notifications:         Vec::new(),
      pending_notifications: HashMap::new(),
      progress:              None,
      graphics:              Graphics::new(),
      apc:                   ApcScan::default(),
      apc_buf:               Vec::new(),
      cell_px:               (1, 1),
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
        parent,
        replace_old,
      } => {
        if replace_old {
          self.grid.clear_images(|reference| reference.image == image);
        }
        if let Some(parent) = parent {
          self
            .grid
            .place_image_relative(image, placement, cols, rows, parent);
        } else {
          self
            .grid
            .place_image(image, placement, cols, rows, keep_cursor);
        }
      },
      GridOp::Clear { spec, free } => {
        let removed = match spec {
          ClearSpec::All => self.grid.clear_images(|_| true),
          ClearSpec::Image(id) => self.grid.clear_images(|r| r.image == id),
          ClearSpec::Placement(id, p) => {
            self
              .grid
              .clear_images(|r| r.image == id && r.placement == p)
          },
          ClearSpec::AtCursor => {
            let targets = self.grid.images_at_cursor();
            self
              .grid
              .clear_images(|r| targets.contains(&(r.image, r.placement)))
          },
          ClearSpec::Cell { x, y, z } => {
            self.grid.clear_screen_images(|col, row, reference| {
              col + 1 == x as usize
                && row + 1 == y as usize
                && z.is_none_or(|wanted| {
                  self
                    .graphics
                    .placement(reference.image, reference.placement)
                    .is_some_and(|placement| placement.z == wanted)
                })
            })
          },
          ClearSpec::Column(x) => {
            self
              .grid
              .clear_screen_images(|col, _, _| col + 1 == x as usize)
          },
          ClearSpec::Row(y) => {
            self
              .grid
              .clear_screen_images(|_, row, _| row + 1 == y as usize)
          },
          ClearSpec::Z(z) => {
            self.grid.clear_images(|reference| {
              self
                .graphics
                .placement(reference.image, reference.placement)
                .is_some_and(|placement| placement.z == z)
            })
          },
          ClearSpec::ImageRange(lo, hi) => {
            self.grid.clear_images(|reference| {
              reference.image >= lo && reference.image <= hi
            })
          },
        };
        let targets = self.graphics.delete_targets(spec, &removed);
        self.grid.clear_images(|reference| {
          targets.contains(&(reference.image, reference.placement))
        });
        let grid = &self.grid;
        self
          .graphics
          .finish_delete(&targets, free, |image| grid.image_referenced(image));
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

  /// DECRQSS (`DCS $ q <request> ST`): report a setting's current value. The
  /// request names a control by its intermediate and final bytes; a valid one
  /// is answered with `DCS 1 $ r <setting> ST`, an unknown one with `DCS 0 $
  /// r`.
  fn answer_decrqss(&mut self, request: &[u8]) {
    let body = match request {
      b" q" => Some(format!("{} q", self.grid.cursor_style_code())),
      b"r" => {
        let (top, bottom) = self.grid.scroll_region();
        Some(format!("{};{}r", top + 1, bottom + 1))
      },
      b"m" => Some(format!("{}m", sgr_report(self.grid.pen()))),
      _ => None,
    };
    match body {
      Some(body) => {
        let _ = write!(self.response, "\x1bP1$r{body}\x1b\\");
      },
      None => self.response.extend_from_slice(b"\x1bP0$r\x1b\\"),
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
    let idx = match self.single_shift {
      Some(i) => i,
      None => self.gl,
    };
    self.charset_at(idx)
  }

  const fn charset_at(&self, idx: u8) -> Charset {
    match idx {
      1 => self.g1,
      2 => self.g2,
      3 => self.g3,
      _ => self.g0,
    }
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
        (true, 1015) => {
          self.grid.set_mouse_encoding(enc(on, MouseEncoding::Urxvt));
        },
        (true, 1016) => {
          self
            .grid
            .set_mouse_encoding(enc(on, MouseEncoding::SgrPixel));
        },
        (true, 2004) => self.grid.set_bracketed_paste(on),
        (true, 2026) => self.grid.set_sync(on),
        (true, 69) => self.grid.set_lr_margins_mode(on),
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
      if set_pen_blink(pen, code) {
        i += step;
        continue;
      }
      match code {
        0 => *pen = Cell::default(),
        1 => pen.flags.insert(Flags::BOLD),
        2 => pen.flags.insert(Flags::DIM),
        3 => pen.flags.insert(Flags::ITALIC),
        4 => pen.underline = underline_from(p),
        7 => pen.flags.insert(Flags::REVERSE),
        8 => pen.flags.insert(Flags::HIDDEN),
        9 => pen.flags.insert(Flags::STRIKE),
        21 => pen.underline = Underline::Double,
        22 => pen.flags.remove(Flags::BOLD.union(Flags::DIM)),
        23 => pen.flags.remove(Flags::ITALIC),
        24 => pen.underline = Underline::None,
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

  /// Resolve a rectangle-op parameter block starting at `base`. Absent or zero
  /// bottom/right default to the far edge.
  fn rect_bounds(&self, p: &Params, base: usize) -> Rect {
    let val = |i: usize| {
      p.iter()
        .nth(i)
        .and_then(|x| x.first().copied())
        .filter(|&v| v != 0)
    };
    Rect {
      top:    val(base).map_or(0, |v| v as usize - 1),
      left:   val(base + 1).map_or(0, |v| v as usize - 1),
      bottom: val(base + 2)
        .map_or_else(|| self.grid.rows() - 1, |v| v as usize - 1),
      right:  val(base + 3)
        .map_or_else(|| self.grid.cols() - 1, |v| v as usize - 1),
    }
  }

  /// DECFRA (`CSI Pch ; Pt ; Pl ; Pb ; Pr $ x`): fill a rectangle with a glyph.
  fn decfra(&mut self, p: &Params) {
    let ch = char::from_u32(u32::from(raw(p, 0)))
      .filter(|c| !c.is_control())
      .unwrap_or(' ');
    let rect = self.rect_bounds(p, 1);
    self.grid.fill_rect(ch, rect);
  }

  /// DECERA (`CSI Pt ; Pl ; Pb ; Pr $ z`): erase a rectangle.
  fn decera(&mut self, p: &Params) {
    let rect = self.rect_bounds(p, 0);
    self.grid.erase_rect(rect);
  }

  /// DECCRA (`... $ v`): copy a source rectangle to a destination top-left.
  fn deccra(&mut self, p: &Params) {
    let src = self.rect_bounds(p, 0);
    let val = |i: usize| {
      p.iter()
        .nth(i)
        .and_then(|x| x.first().copied())
        .filter(|&v| v != 0)
    };
    let dt = val(5).map_or(0, |v| v as usize - 1);
    let dl = val(6).map_or(0, |v| v as usize - 1);
    self.grid.copy_rect(src, dt, dl);
  }

  /// DECCARA (`CSI Pt ; Pl ; Pb ; Pr ; Ps... $ r`): change attributes in a
  /// rectangle, leaving the characters in place.
  fn deccara(&mut self, p: &Params) {
    let rect = self.rect_bounds(p, 0);
    let (mut set, mut clear) = (Flags::empty(), Flags::empty());
    let mut underline = None;
    for item in p.iter().skip(4) {
      let code = item.first().copied().unwrap_or(0);
      if change_rect_blink(code, &mut set, &mut clear) {
        continue;
      }
      match code {
        0 => {
          set = Flags::empty();
          clear = Flags::BOLD
            .union(Flags::BLINK)
            .union(Flags::RAPID_BLINK)
            .union(Flags::REVERSE)
            .union(Flags::STRIKE);
          underline = Some(Underline::None);
        },
        1 => set.insert(Flags::BOLD),
        4 => underline = Some(Underline::Single),
        7 => set.insert(Flags::REVERSE),
        9 => set.insert(Flags::STRIKE),
        22 => clear.insert(Flags::BOLD),
        24 => underline = Some(Underline::None),
        27 => clear.insert(Flags::REVERSE),
        29 => clear.insert(Flags::STRIKE),
        _ => {},
      }
    }
    self.grid.change_attrs_rect(rect, set, clear, underline);
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
      (true, 1015) => {
        set_reset(self.grid.mouse_encoding() == MouseEncoding::Urxvt)
      },
      (true, 1016) => {
        set_reset(self.grid.mouse_encoding() == MouseEncoding::SgrPixel)
      },
      (true, 2004) => set_reset(self.grid.bracketed_paste()),
      (true, 2026) => set_reset(self.grid.sync_active()),
      (true, 69) => set_reset(self.grid.lr_margins_enabled()),
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

#[cfg(test)] mod tests;
