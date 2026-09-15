//! VT formatting, mode, and colour parsing helpers.

use crate::{
  grid::{Cell, Color, Flags, MouseEncoding, MouseProtocol, Underline},
  theme::{Rgb, parse_color},
};

/// Which device-attributes query is being answered.
#[derive(Clone, Copy, Debug)]
pub(super) enum DaLevel {
  Primary,
  Secondary,
  Tertiary,
}

/// Which dynamic colour an OSC 10/11/17/19 escape targets.
#[derive(Clone, Copy, Debug)]
pub(super) enum Dynamic {
  Fg,
  Bg,
  SelBg,
  SelFg,
}

/// DECRQM mode-state code: 1 = set, 2 = reset.
pub(super) const fn set_reset(on: bool) -> u8 {
  if on { 1 } else { 2 }
}

/// Serialize a pen into the SGR parameter list that would recreate it, for a
/// DECRQSS `m` reply. Begins with `0` (reset) so the set attributes follow a
/// known baseline.
pub(super) fn sgr_report(cell: &Cell) -> String {
  let mut p: Vec<String> = vec!["0".to_string()];
  let f = cell.flags;
  if f.contains(Flags::BOLD) {
    p.push("1".into());
  }
  if f.contains(Flags::DIM) {
    p.push("2".into());
  }
  if f.contains(Flags::ITALIC) {
    p.push("3".into());
  }
  match cell.underline {
    Underline::None => {},
    Underline::Single => p.push("4".into()),
    Underline::Double => p.push("21".into()),
    Underline::Curly => p.push("4:3".into()),
    Underline::Dotted => p.push("4:4".into()),
    Underline::Dashed => p.push("4:5".into()),
  }
  push_blink_report(&mut p, f);
  if f.contains(Flags::REVERSE) {
    p.push("7".into());
  }
  if f.contains(Flags::HIDDEN) {
    p.push("8".into());
  }
  if f.contains(Flags::STRIKE) {
    p.push("9".into());
  }
  if f.contains(Flags::OVERLINE) {
    p.push("53".into());
  }
  push_color(&mut p, cell.fg, 30, 38);
  push_color(&mut p, cell.bg, 40, 48);
  if let Color::Indexed(i) = cell.underline_color {
    p.push(format!("58;5;{i}"));
  } else if let Color::Rgb(r, g, b) = cell.underline_color {
    p.push(format!("58;2;{r};{g};{b}"));
  }
  p.join(";")
}

pub(super) fn push_blink_report(params: &mut Vec<String>, flags: Flags) {
  if flags.contains(Flags::BLINK) {
    params.push("5".into());
  }
  if flags.contains(Flags::RAPID_BLINK) {
    params.push("6".into());
  }
}

/// Append the SGR parameter for `color` to `p`. `base` is the 8-colour set
/// code (30 fg / 40 bg); `ext` is the extended-colour code (38 / 48).
pub(super) fn push_color(
  p: &mut Vec<String>,
  color: Color,
  base: u16,
  ext: u16,
) {
  match color {
    Color::Default => {},
    Color::Indexed(i) if i < 8 => p.push((base + u16::from(i)).to_string()),
    Color::Indexed(i) if i < 16 => {
      p.push((base + 60 + u16::from(i - 8)).to_string());
    },
    Color::Indexed(i) => p.push(format!("{ext};5;{i}")),
    Color::Rgb(r, g, b) => p.push(format!("{ext};2;{r};{g};{b}")),
  }
}

pub(super) const fn set_pen_blink(pen: &mut Cell, code: u16) -> bool {
  match code {
    5 => {
      pen.flags.remove(Flags::RAPID_BLINK);
      pen.flags.insert(Flags::BLINK);
    },
    6 => {
      pen.flags.remove(Flags::BLINK);
      pen.flags.insert(Flags::RAPID_BLINK);
    },
    25 => pen.flags.remove(Flags::BLINK.union(Flags::RAPID_BLINK)),
    _ => return false,
  }
  true
}

pub(super) const fn change_rect_blink(
  code: u16,
  set: &mut Flags,
  clear: &mut Flags,
) -> bool {
  match code {
    5 => set.insert(Flags::BLINK),
    6 => set.insert(Flags::RAPID_BLINK),
    25 => clear.insert(Flags::BLINK.union(Flags::RAPID_BLINK)),
    _ => return false,
  }
  true
}

/// Parse an OSC colour spec into an [`Rgb`].
pub(super) fn parse_spec(spec: &[u8]) -> Option<Rgb> {
  str::from_utf8(spec).ok().and_then(parse_color)
}

/// Parse a decimal palette index (0-255).
pub(super) fn parse_index(b: &[u8]) -> Option<u8> {
  str::from_utf8(b).ok()?.parse().ok()
}

pub(super) const fn rgb_tuple(rgb: Rgb) -> (u8, u8, u8) {
  (rgb.0, rgb.1, rgb.2)
}

/// Select `protocol` when a mouse mode is set, else turn reporting off.
pub(super) const fn proto(on: bool, protocol: MouseProtocol) -> MouseProtocol {
  if on { protocol } else { MouseProtocol::Off }
}

/// Select `encoding` when its mode is set, else fall back to the default form.
pub(super) const fn enc(on: bool, encoding: MouseEncoding) -> MouseEncoding {
  if on { encoding } else { MouseEncoding::X10 }
}
