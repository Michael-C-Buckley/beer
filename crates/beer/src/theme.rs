//! Runtime colours: the foreground/background, the 256-entry palette, and the
//! selection/search/cursor accents. Seeded from config and mutated at runtime
//! by OSC colour escapes (4/104, 10/11, 17/19, and their resets).

use crate::config::Colors as ColorConfig;

/// An 8-bit-per-channel RGB colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Where a `Color::Default` resolves to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plane {
  Fg,
  Bg,
}

/// The active colour scheme.
#[derive(Clone, Debug)]
pub struct Theme {
  pub fg:               Rgb,
  pub bg:               Rgb,
  /// Cursor body colour; `None` follows the cell under the cursor.
  pub cursor:           Option<Rgb>,
  pub selection_fg:     Option<Rgb>,
  pub selection_bg:     Rgb,
  pub match_bg:         Rgb,
  pub current_match_bg: Rgb,
  pub search_bar_bg:    Rgb,
  pub palette:          [Rgb; 256],
  pub bold_as_bright:   bool,
  /// Background opacity, 0 (transparent) - 255 (opaque).
  pub alpha:            u8,
  /// Configured defaults, restored by OSC reset escapes (110/111/104).
  default_fg:           Rgb,
  default_bg:           Rgb,
  default_palette:      [Rgb; 256],
}

impl Default for Theme {
  fn default() -> Self {
    let palette = default_palette();
    Self {
      fg: DEFAULT_FG,
      bg: DEFAULT_BG,
      cursor: None,
      selection_fg: None,
      selection_bg: SELECTION_BG,
      match_bg: MATCH_BG,
      current_match_bg: CURRENT_MATCH_BG,
      search_bar_bg: SEARCH_BAR_BG,
      palette,
      bold_as_bright: true,
      alpha: 255,
      default_fg: DEFAULT_FG,
      default_bg: DEFAULT_BG,
      default_palette: palette,
    }
  }
}

#[expect(
  clippy::absolute_paths,
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  reason = "theme parsing and palette construction use bounded colour \
            protocol values"
)]
impl Theme {
  /// Build a theme from the `[colors]` config table, falling back to the
  /// built-in defaults for any unset entry.
  pub fn from_config(c: &ColorConfig) -> Self {
    let mut theme = Self::default();
    let set = |dst: &mut Rgb, spec: &Option<String>| {
      if let Some(rgb) = spec.as_deref().and_then(parse_color) {
        *dst = rgb;
      }
    };
    set(&mut theme.fg, &c.foreground);
    set(&mut theme.bg, &c.background);
    set(&mut theme.selection_bg, &c.selection_background);
    set(&mut theme.match_bg, &c.match_background);
    set(&mut theme.current_match_bg, &c.match_current_background);
    theme.cursor = c.cursor.as_deref().and_then(parse_color);
    theme.selection_fg =
      c.selection_foreground.as_deref().and_then(parse_color);
    if let Some(regular) = &c.regular {
      apply_palette(&mut theme.palette, 0, regular);
    }
    if let Some(bright) = &c.bright {
      apply_palette(&mut theme.palette, 8, bright);
    }
    if let Some(a) = c.alpha {
      theme.alpha = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    if let Some(b) = c.bold_as_bright {
      theme.bold_as_bright = b;
    }
    // Remember the resolved scheme so OSC resets return here, not to the
    // compiled-in defaults.
    theme.default_fg = theme.fg;
    theme.default_bg = theme.bg;
    theme.default_palette = theme.palette;
    theme
  }

  /// Resolve a cell colour to RGB on the given plane, applying bold-as-bright
  /// to the low 8 palette indices.
  pub const fn resolve(
    &self,
    color: crate::grid::Color,
    plane: Plane,
    bold: bool,
  ) -> Rgb {
    use crate::grid::Color;
    match color {
      Color::Default => {
        match plane {
          Plane::Fg => self.fg,
          Plane::Bg => self.bg,
        }
      },
      Color::Indexed(i) => {
        let idx = if bold && self.bold_as_bright && i < 8 {
          i + 8
        } else {
          i
        };
        self.palette[idx as usize]
      },
      Color::Rgb(r, g, b) => Rgb(r, g, b),
    }
  }

  pub const fn set_palette(&mut self, index: u8, rgb: Rgb) {
    self.palette[index as usize] = rgb;
  }

  /// Reset palette entry `index` to its configured value.
  pub const fn reset_palette_index(&mut self, index: u8) {
    self.palette[index as usize] = self.default_palette[index as usize];
  }

  /// Reset the whole palette to its configured values.
  pub const fn reset_palette(&mut self) {
    self.palette = self.default_palette;
  }

  pub const fn reset_fg(&mut self) {
    self.fg = self.default_fg;
  }

  pub const fn reset_bg(&mut self) {
    self.bg = self.default_bg;
  }

  /// A copy with foreground and background swapped, for the visual bell flash.
  pub fn inverted(&self) -> Self {
    let mut t = self.clone();
    std::mem::swap(&mut t.fg, &mut t.bg);
    t
  }
}

/// Foreground/background used for `Color::Default`.
const DEFAULT_FG: Rgb = Rgb(0xC5, 0xC8, 0xC6);
const DEFAULT_BG: Rgb = Rgb(0x18, 0x18, 0x18);
const SELECTION_BG: Rgb = Rgb(0x44, 0x47, 0x5A);
const MATCH_BG: Rgb = Rgb(0x5A, 0x51, 0x2A);
const CURRENT_MATCH_BG: Rgb = Rgb(0xB8, 0x8A, 0x2A);
const SEARCH_BAR_BG: Rgb = Rgb(0x30, 0x30, 0x40);

/// Overwrite palette entries `[base, base+specs.len())` from hex specs.
fn apply_palette(palette: &mut [Rgb; 256], base: usize, specs: &[String]) {
  for (i, spec) in specs.iter().take(8).enumerate() {
    if let Some(rgb) = parse_color(spec) {
      palette[base + i] = rgb;
    }
  }
}

/// The xterm 256-colour palette: 16 base, a 6×6×6 cube, then 24 greys.
#[expect(
  clippy::absolute_paths,
  clippy::cast_possible_truncation,
  reason = "the xterm palette indices and channel values are explicitly \
            bounded"
)]
fn default_palette() -> [Rgb; 256] {
  const BASE: [Rgb; 16] = [
    Rgb(0x00, 0x00, 0x00),
    Rgb(0xCD, 0x00, 0x00),
    Rgb(0x00, 0xCD, 0x00),
    Rgb(0xCD, 0xCD, 0x00),
    Rgb(0x00, 0x00, 0xEE),
    Rgb(0xCD, 0x00, 0xCD),
    Rgb(0x00, 0xCD, 0xCD),
    Rgb(0xE5, 0xE5, 0xE5),
    Rgb(0x7F, 0x7F, 0x7F),
    Rgb(0xFF, 0x00, 0x00),
    Rgb(0x00, 0xFF, 0x00),
    Rgb(0xFF, 0xFF, 0x00),
    Rgb(0x5C, 0x5C, 0xFF),
    Rgb(0xFF, 0x00, 0xFF),
    Rgb(0x00, 0xFF, 0xFF),
    Rgb(0xFF, 0xFF, 0xFF),
  ];
  let cube = |step: u8| -> u8 { if step == 0 { 0 } else { 55 + 40 * step } };
  std::array::from_fn(|i| {
    match i {
      0..=15 => BASE[i],
      16..=231 => {
        let i = i as u8 - 16;
        Rgb(cube(i / 36), cube((i / 6) % 6), cube(i % 6))
      },
      _ => {
        let v = 8 + 10 * (i as u8 - 232);
        Rgb(v, v, v)
      },
    }
  })
}

/// Parse an X11 colour spec: `rgb:rr/gg/bb` (1-4 hex digits per channel) or
/// `#rrggbb`.
#[expect(
  clippy::cast_possible_truncation,
  reason = "parsed colour channels are normalized into the u8 range"
)]
pub fn parse_color(spec: &str) -> Option<Rgb> {
  if let Some(rest) = spec.strip_prefix("rgb:") {
    let mut it = rest.split('/');
    let chan = |s: &str| -> Option<u8> {
      let v = u32::from_str_radix(s, 16).ok()?;
      let max = (1u32 << (4 * s.len() as u32)) - 1;
      Some((v * 255 / max) as u8)
    };
    return Some(Rgb(chan(it.next()?)?, chan(it.next()?)?, chan(it.next()?)?));
  }
  let hex = spec.strip_prefix('#')?;
  if hex.len() == 6 {
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    return Some(Rgb(byte(0)?, byte(2)?, byte(4)?));
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_color_forms() {
    assert_eq!(parse_color("#ff0080"), Some(Rgb(255, 0, 128)));
    assert_eq!(parse_color("rgb:ff/00/80"), Some(Rgb(255, 0, 128)));
    assert_eq!(parse_color("rgb:ffff/0000/8080"), Some(Rgb(255, 0, 128)));
    assert_eq!(parse_color("nope"), None);
  }

  #[test]
  fn palette_cube_and_grey() {
    let p = default_palette();
    assert_eq!(p[16], Rgb(0, 0, 0));
    assert_eq!(p[231], Rgb(255, 255, 255));
    assert_eq!(p[232], Rgb(8, 8, 8));
  }

  #[test]
  fn config_overrides_and_resets() {
    let cfg = ColorConfig {
      background: Some("#000000".to_string()),
      regular: Some(vec!["#111111".to_string()]),
      ..ColorConfig::default()
    };
    let mut theme = Theme::from_config(&cfg);
    assert_eq!(theme.bg, Rgb(0, 0, 0));
    assert_eq!(theme.palette[0], Rgb(0x11, 0x11, 0x11));
    // OSC reset returns to the configured value, not the compiled default.
    theme.set_palette(0, Rgb(9, 9, 9));
    theme.reset_palette_index(0);
    assert_eq!(theme.palette[0], Rgb(0x11, 0x11, 0x11));
  }
}
