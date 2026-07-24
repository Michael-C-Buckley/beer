//! The kitty text-sizing protocol (`OSC 66`): rendering a run of text in a
//! block of cells larger than one, at a multiplied or fractional font size.
//!
//! The escape code is `OSC 66 ; metadata ; text ST`, where `metadata` is a
//! colon-separated list of `key=value` pairs. This module parses that metadata
//! into a [`TextSize`]; the dispatcher supplies the text and the grid lays it
//! out, because splitting a run into cells needs character-width tables and the
//! screen width that live with the terminal, not here.
//!
//! The keys, per the protocol:
//!
//! - `s` (1-7, default 1) - the overall scale. The run occupies a block `s * w`
//!   cells wide and `s` cells high, and the font size is multiplied by `s`.
//! - `w` (0-7, default 0) - the width in scaled cells. `0` means "split the run
//!   into cells as normal", each cell then being an `s` by `s` block.
//! - `n`, `d` (0-15, default 0) - numerator and denominator of a fractional
//!   scale applied on top of `s`. It changes the rendered font size but not the
//!   number of cells occupied. Alignment applies only when `n < d`.
//! - `v` - vertical alignment of the fractional render area in its block.
//! - `h` - horizontal alignment of the fractional render area in its block.

/// Vertical placement of a fractionally-scaled render area within its cell
/// block. Only meaningful when the fraction `n/d` is below 1; `Top` is the
/// default (a superscript), `Bottom` a subscript, `Middle` centred.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum VAlign {
  #[default]
  Top,
  Bottom,
  Middle,
}

/// Horizontal placement of a fractionally-scaled render area within its cell
/// block. `Left` is the default.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum HAlign {
  #[default]
  Left,
  Right,
  Center,
}

/// Parsed `OSC 66` metadata: the size and alignment of one multicell text run.
///
/// A value equal to [`TextSize::default`] (`s=1, w=0, n=0, d=0`) describes
/// ordinary text and needs no special handling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TextSize {
  /// `s`: overall integer scale, clamped to 1-7.
  pub scale:       u8,
  /// `w`: width in scaled cells, clamped to 0-7. `0` means split as normal.
  pub width:       u8,
  /// `n`: fractional-scale numerator, clamped to 0-15.
  pub numerator:   u8,
  /// `d`: fractional-scale denominator, clamped to 0-15.
  pub denominator: u8,
  /// `v`: vertical alignment of the fractional render area.
  pub valign:      VAlign,
  /// `h`: horizontal alignment of the fractional render area.
  pub halign:      HAlign,
}

impl Default for TextSize {
  fn default() -> Self {
    Self {
      scale:       1,
      width:       0,
      numerator:   0,
      denominator: 0,
      valign:      VAlign::default(),
      halign:      HAlign::default(),
    }
  }
}

impl TextSize {
  /// Whether this run is ordinary text needing no scaled layout: scale 1, no
  /// explicit width, and no fractional scale.
  #[must_use]
  pub const fn is_plain(&self) -> bool {
    self.scale == 1 && self.width == 0 && !self.has_fraction()
  }

  /// Whether a fractional scale `n/d` (with `n < d`) is in effect. Only then
  /// does the font shrink within the block and does alignment matter.
  #[must_use]
  pub const fn has_fraction(&self) -> bool {
    self.numerator > 0
      && self.denominator > 0
      && self.numerator < self.denominator
  }

  /// The factor to multiply the base font size by when rendering this run:
  /// the integer scale `s`, reduced by the fraction `n/d` when one is set.
  #[must_use]
  pub fn font_scale(&self) -> f32 {
    if self.has_fraction() {
      f32::from(self.scale) * f32::from(self.numerator)
        / f32::from(self.denominator)
    } else {
      f32::from(self.scale)
    }
  }

  /// The height of the cell block, in cells: `s`.
  #[must_use]
  pub const fn cell_height(&self) -> usize {
    self.scale as usize
  }

  /// Parse from a `&str` of metadata; convenience over [`parse`] when the
  /// caller already holds a string rather than bytes.
  #[must_use]
  pub fn parse_str(meta: &str) -> Self {
    parse(meta.as_bytes())
  }
}

/// Parse the colon-separated `OSC 66` metadata field into a [`TextSize`].
///
/// Unknown keys and malformed pairs are ignored; out-of-range integers are
/// clamped to the protocol's documented bounds. An empty field yields the
/// default (ordinary text).
#[must_use]
pub fn parse(meta: &[u8]) -> TextSize {
  let mut ts = TextSize::default();

  for pair in meta.split(|&b| b == b':') {
    let Some(eq) = pair.iter().position(|&b| b == b'=') else {
      continue;
    };
    let key = pair[0];
    let Some(value) = parse_u32(&pair[eq + 1..]) else {
      continue;
    };
    match key {
      b's' => ts.scale = value.clamp(1, 7) as u8,
      b'w' => ts.width = value.min(7) as u8,
      b'n' => ts.numerator = value.min(15) as u8,
      b'd' => ts.denominator = value.min(15) as u8,
      b'v' => {
        ts.valign = match value {
          1 => VAlign::Bottom,
          2 => VAlign::Middle,
          _ => VAlign::Top,
        }
      },
      b'h' => {
        ts.halign = match value {
          1 => HAlign::Right,
          2 => HAlign::Center,
          _ => HAlign::Left,
        }
      },
      _ => {},
    }
  }
  ts
}

/// Parse a run of ASCII digits into a `u32`, saturating on overflow. Returns
/// `None` if the slice is empty or holds a non-digit.
fn parse_u32(bytes: &[u8]) -> Option<u32> {
  if bytes.is_empty() {
    return None;
  }
  let mut acc: u32 = 0;
  for &b in bytes {
    let d = b.checked_sub(b'0').filter(|&d| d < 10)?;
    acc = acc.saturating_mul(10).saturating_add(u32::from(d));
  }
  Some(acc)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_is_plain_default() {
    let ts = parse(b"");
    assert_eq!(ts, TextSize::default());
    assert!(ts.is_plain());
    assert!((ts.font_scale() - 1.0).abs() < f32::EPSILON);
  }

  #[test]
  fn scale_doubles_font_and_height() {
    let ts = parse(b"s=2");
    assert_eq!(ts.scale, 2);
    assert!((ts.font_scale() - 2.0).abs() < f32::EPSILON);
    assert_eq!(ts.cell_height(), 2);
    assert!(!ts.is_plain());
  }

  #[test]
  fn width_and_scale_together() {
    let ts = parse(b"s=2:w=3");
    assert_eq!((ts.scale, ts.width), (2, 3));
  }

  #[test]
  fn superscript_is_half_size_top_aligned() {
    // `n=1:d=2` with default scale: half-size font, top of the cell.
    let ts = parse(b"n=1:d=2");
    assert!(ts.has_fraction());
    assert!((ts.font_scale() - 0.5).abs() < f32::EPSILON);
    assert_eq!(ts.valign, VAlign::Top);
  }

  #[test]
  fn subscript_is_bottom_aligned() {
    let ts = parse(b"n=1:d=2:v=1");
    assert_eq!(ts.valign, VAlign::Bottom);
    assert!((ts.font_scale() - 0.5).abs() < f32::EPSILON);
  }

  #[test]
  fn centred_normal_size_in_double_block() {
    // `s=2:n=1:d=2:v=2`: full-size font (2 * 1/2) centred in a 2-high block.
    let ts = parse(b"s=2:n=1:d=2:v=2");
    assert!((ts.font_scale() - 1.0).abs() < f32::EPSILON);
    assert_eq!(ts.valign, VAlign::Middle);
    assert_eq!(ts.cell_height(), 2);
  }

  #[test]
  fn fraction_at_or_above_one_does_not_shrink() {
    // n >= d means no reduction and no alignment.
    let ts = parse(b"s=3:n=2:d=2");
    assert!(!ts.has_fraction());
    assert!((ts.font_scale() - 3.0).abs() < f32::EPSILON);
  }

  #[test]
  fn out_of_range_values_clamp() {
    assert_eq!(parse(b"s=99").scale, 7);
    assert_eq!(parse(b"s=0").scale, 1);
    assert_eq!(parse(b"w=42").width, 7);
    assert_eq!(parse(b"n=200:d=200").numerator, 15);
  }

  #[test]
  fn unknown_and_malformed_pairs_are_ignored() {
    let ts = parse(b"s=2:zzz=9:bad:=5:w=");
    assert_eq!(ts.scale, 2);
    assert_eq!(ts.width, 0);
  }

  #[test]
  fn horizontal_alignment() {
    assert_eq!(parse(b"h=1").halign, HAlign::Right);
    assert_eq!(parse(b"h=2").halign, HAlign::Center);
    assert_eq!(parse(b"h=0").halign, HAlign::Left);
  }
}
