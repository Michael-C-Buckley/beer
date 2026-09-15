//! Direct rasterization for braille, Powerline, box drawing, and block glyphs.

use super::canvas::Canvas;
use crate::{font::CellMetrics, theme::Rgb};
pub(super) fn is_braille(c: char) -> bool {
  ('\u{2800}'..='\u{28ff}').contains(&c)
}

/// Braille dot geometry for a `width`×`height` cell: the square dot side `w`,
/// the two column origins, and the four row origins. Ported verbatim from
/// foot's `box-drawing.c` `draw_braille` - base size and spacing from the cell,
/// then leftover pixels distributed (dot → margin → spacing → margin → dot) so
/// dots land on exact pixels with no rounding drift.
#[expect(clippy::cast_sign_loss, reason = "cell geometry is non-negative")]
pub(super) fn braille_geometry(
  width: i32,
  height: i32,
) -> (u32, [i32; 2], [i32; 4]) {
  let mut w = (width / 4).min(height / 8);
  let mut x_spacing = width / 4;
  let mut y_spacing = height / 8;
  let mut x_margin = x_spacing / 2;
  let mut y_margin = y_spacing / 2;

  let mut x_left = width - 2 * x_margin - x_spacing - 2 * w;
  let mut y_left = height - 2 * y_margin - 3 * y_spacing - 4 * w;

  // First, try hard to ensure a non-zero dot width.
  if x_left >= 2 && y_left >= 4 && w == 0 {
    w += 1;
    x_left -= 2;
    y_left -= 4;
  }
  // Second, prefer a non-zero margin.
  if x_left >= 2 && x_margin == 0 {
    x_margin = 1;
    x_left -= 2;
  }
  if y_left >= 2 && y_margin == 0 {
    y_margin = 1;
    y_left -= 2;
  }
  // Third, increase spacing.
  if x_left >= 1 {
    x_spacing += 1;
    x_left -= 1;
  }
  if y_left >= 3 {
    y_spacing += 1;
    y_left -= 3;
  }
  // Fourth, the side margins.
  if x_left >= 2 {
    x_margin += 1;
    x_left -= 2;
  }
  if y_left >= 2 {
    y_margin += 1;
    y_left -= 2;
  }
  // Last, increase the dot width.
  if x_left >= 2 && y_left >= 4 {
    w += 1;
  }

  let xs = [x_margin, x_margin + w + x_spacing];
  let ys = [
    y_margin,
    y_margin + w + y_spacing,
    y_margin + 2 * (w + y_spacing),
    y_margin + 3 * (w + y_spacing),
  ];
  (w.max(0) as u32, xs, ys)
}

/// Draw a braille pattern as a 2×4 grid of `w`×`w` square dots. Geometry ported
/// from foot's `box-drawing.c` `draw_braille`: a dot size and base spacing are
/// derived from the cell, then leftover pixels are distributed (dot width →
/// margins → spacing → …) so dots land on exact pixels with no rounding drift.
/// The low eight bits of the codepoint select dots: bits 0-2 are the left
/// column rows 0-2, bits 3-5 the right column rows 0-2, bits 6-7 the bottom
/// row.
#[expect(
  clippy::cast_possible_wrap,
  clippy::cast_possible_truncation,
  reason = "braille geometry is bounded by the terminal cell"
)]
pub(super) fn draw_braille(
  canvas: &mut Canvas,
  c: char,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) {
  // (bit mask, column index, row index).
  const DOTS: [(u8, usize, usize); 8] = [
    (0x01, 0, 0),
    (0x02, 0, 1),
    (0x04, 0, 2),
    (0x08, 1, 0),
    (0x10, 1, 1),
    (0x20, 1, 2),
    (0x40, 0, 3),
    (0x80, 1, 3),
  ];
  let (w, xs, ys) = braille_geometry(m.width as i32, m.height as i32);
  let sym = ((c as u32) - 0x2800) as u8;
  for (mask, col, row) in DOTS {
    if sym & mask != 0 {
      canvas.fill_rect(x0 + xs[col], top + ys[row], w, w, fg);
    }
  }
}

/// Whether `c` is drawn geometrically by [`draw_box`]: box drawing
/// (U+2500-257F), block elements (U+2580-259F), or the legacy-computing
/// sextants (U+1FB00-1FB3B) and octants (U+1CD00-1CDE5).
pub(super) const fn is_box_draw(c: char) -> bool {
  matches!(c as u32, 0x2500..=0x259F | 0x1FB00..=0x1FB3B | 0x1CD00..=0x1CDE5)
}

pub(super) const fn is_geometric(c: char) -> bool {
  is_box_draw(c)
    || matches!(
      c as u32,
      0xE0B0..=0xE0B7 | 0xE0B8 | 0xE0BA | 0xE0BC | 0xE0BE
    )
}

pub(super) fn draw_geometric(
  canvas: &mut Canvas,
  c: char,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) -> bool {
  if matches!(
    c as u32,
    0xE0B0..=0xE0B7 | 0xE0B8 | 0xE0BA | 0xE0BC | 0xE0BE
  ) {
    draw_powerline(canvas, c as u32, x0, top, m, fg);
    true
  } else {
    draw_box(canvas, c, x0, top, m, fg)
  }
}

fn draw_box(
  canvas: &mut Canvas,
  c: char,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) -> bool {
  match c as u32 {
    0x2580..=0x259F => {
      draw_block_element(canvas, c as u32, x0, top, m, fg);
      true
    },
    0x1FB00..=0x1FB3B => {
      draw_sextant(canvas, c as u32, x0, top, m, fg);
      true
    },
    0x1CD00..=0x1CDE5 => {
      draw_octant(canvas, c as u32, x0, top, m, fg);
      true
    },
    0x2500..=0x257F => draw_box_line(canvas, c as u32, x0, top, m, fg),
    _ => false,
  }
}

/// Blend `fg` over a rectangle at fractional `alpha` (the 25/50/75% shades).
fn blend_shade(
  canvas: &mut Canvas,
  x0: i32,
  top: i32,
  w: i32,
  h: i32,
  fg: Rgb,
  alpha: u8,
) {
  for y in top..top + h {
    for x in x0..x0 + w {
      canvas.blend(x, y, fg, alpha);
    }
  }
}

/// Block elements U+2580-259F: half/eighth blocks, shades, and quadrants. All
/// boundaries are floor-divided from the cell so adjacent cells share the exact
/// same edge and tile without seams.
#[expect(
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "block element geometry is bounded by the terminal cell"
)]
fn draw_block_element(
  canvas: &mut Canvas,
  cp: u32,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) {
  let (width, height) = (m.width as i32, m.height as i32);
  // Horizontal and vertical eighth boundaries.
  let edge_x = |n: i32| (n * width) / 8;
  let edge_y = |n: i32| (n * height) / 8;
  match cp {
    // Upper half.
    0x2580 => canvas.fill_rect(x0, top, width as u32, edge_y(4) as u32, fg),
    // Lower one-eighth (2581) through full block (2588).
    0x2581..=0x2588 => {
      let segment = (cp - 0x2580) as i32;
      let y = top + edge_y(8 - segment);
      canvas.fill_rect(
        x0,
        y,
        width as u32,
        (height - edge_y(8 - segment)) as u32,
        fg,
      );
    },
    // Left seven-eighths (2589) through left one-eighth (258F).
    0x2589..=0x258F => {
      let segment = 8 - (cp - 0x2588) as i32;
      canvas.fill_rect(x0, top, edge_x(segment) as u32, height as u32, fg);
    },
    // Right half.
    0x2590 => {
      canvas.fill_rect(
        x0 + edge_x(4),
        top,
        (width - edge_x(4)) as u32,
        height as u32,
        fg,
      );
    },
    0x2591 => blend_shade(canvas, x0, top, width, height, fg, 0x40),
    0x2592 => blend_shade(canvas, x0, top, width, height, fg, 0x80),
    0x2593 => blend_shade(canvas, x0, top, width, height, fg, 0xC0),
    // Upper one-eighth.
    0x2594 => canvas.fill_rect(x0, top, width as u32, edge_y(1) as u32, fg),
    // Right one-eighth.
    0x2595 => {
      canvas.fill_rect(
        x0 + edge_x(7),
        top,
        (width - edge_x(7)) as u32,
        height as u32,
        fg,
      );
    },
    0x2596..=0x259F => draw_quadrants(canvas, cp, x0, top, width, height, fg),
    _ => {},
  }
}

/// The quadrant block elements U+2596-259F, as a 2x2 grid of half-cells.
#[expect(
  clippy::cast_sign_loss,
  reason = "quadrant geometry is bounded by the terminal cell"
)]
fn draw_quadrants(
  canvas: &mut Canvas,
  cp: u32,
  x0: i32,
  top: i32,
  w: i32,
  h: i32,
  fg: Rgb,
) {
  // Bit 0 = upper-left, 1 = upper-right, 2 = lower-left, 3 = lower-right.
  let mask: u8 = match cp {
    0x2596 => 0b0100,
    0x2597 => 0b1000,
    0x2598 => 0b0001,
    0x2599 => 0b1101,
    0x259A => 0b1001,
    0x259B => 0b0111,
    0x259C => 0b1011,
    0x259D => 0b0010,
    0x259E => 0b0110,
    0x259F => 0b1110,
    _ => 0,
  };
  let (hx, hy) = (w / 2, h / 2);
  if mask & 0b0001 != 0 {
    canvas.fill_rect(x0, top, hx as u32, hy as u32, fg);
  }
  if mask & 0b0010 != 0 {
    canvas.fill_rect(x0 + hx, top, (w - hx) as u32, hy as u32, fg);
  }
  if mask & 0b0100 != 0 {
    canvas.fill_rect(x0, top + hy, hx as u32, (h - hy) as u32, fg);
  }
  if mask & 0b1000 != 0 {
    canvas.fill_rect(x0 + hx, top + hy, (w - hx) as u32, (h - hy) as u32, fg);
  }
}

/// The bit pattern (positions 1-6, LSB = top-left) for sextant codepoint `cp`.
/// The range U+1FB00-1FB3B enumerates the 60 sextant combinations, skipping
/// blank, full, and the two that duplicate the left/right half blocks (bit
/// patterns 21 and 42).
#[expect(
  clippy::cast_possible_truncation,
  reason = "the sextant codepoint range is explicitly bounded"
)]
pub(super) const fn sextant_pattern(cp: u32) -> u8 {
  let idx = (cp - 0x1FB00) as u8;
  let mut seen = 0u8;
  let mut pat = 1u8;
  while pat <= 62 {
    if pat != 21 && pat != 42 {
      if seen == idx {
        return pat;
      }
      seen += 1;
    }
    pat += 1;
  }
  0
}

/// Legacy-computing sextants U+1FB00-1FB3B: a 2-column, 3-row block mosaic.
#[expect(
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "sextant geometry is bounded by the terminal cell"
)]
fn draw_sextant(
  canvas: &mut Canvas,
  cp: u32,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) {
  let (w, h) = (m.width as i32, m.height as i32);
  let pat = sextant_pattern(cp);
  let cx = w / 2;
  let ry = |r: i32| top + (r * h) / 3;
  for bit in 0..6u8 {
    if pat & (1 << bit) == 0 {
      continue;
    }
    let (col, row) = (i32::from(bit % 2), i32::from(bit / 2));
    let xa = x0 + col * cx;
    let xw = if col == 0 { cx } else { w - cx };
    let (ya, yb) = (ry(row), ry(row + 1));
    canvas.fill_rect(xa, ya, xw as u32, (yb - ya) as u32, fg);
  }
}

/// Fill pattern per octant codepoint (index = `cp - 0x1CD00`, 0..229). Bits
/// 0-3 are the left column top-to-bottom, bits 4-7 the right column. Transcribed
/// verbatim from Kitty's `decorations.c` `octant()` mapping table.
#[rustfmt::skip]
pub(super) const OCTANTS: [u8; 230] = [
  0x02, 0x12, 0x13, 0x20, 0x21, 0x31, 0x22, 0x23, 0x32, 0x04, 0x05, 0x14, 0x15, 0x07, 0x16, 0x17,
  0x24, 0x25, 0x34, 0x35, 0x26, 0x27, 0x36, 0x37, 0x40, 0x41, 0x50, 0x51, 0x42, 0x43, 0x52, 0x53,
  0x61, 0x70, 0x71, 0x62, 0x63, 0x72, 0x73, 0x44, 0x45, 0x54, 0x55, 0x46, 0x47, 0x56, 0x57, 0x64,
  0x65, 0x74, 0x75, 0x66, 0x67, 0x76, 0x09, 0x18, 0x19, 0x0a, 0x0b, 0x1a, 0x1b, 0x28, 0x29, 0x38,
  0x39, 0x2a, 0x2b, 0x3a, 0x3b, 0x0d, 0x1c, 0x1d, 0x0e, 0x1e, 0x1f, 0x2c, 0x2d, 0x3d, 0x2e, 0x2f,
  0x3e, 0x48, 0x49, 0x58, 0x59, 0x4a, 0x4b, 0x5a, 0x5b, 0x68, 0x69, 0x78, 0x79, 0x6a, 0x6b, 0x7a,
  0x7b, 0x4c, 0x4d, 0x5c, 0x5d, 0x4e, 0x4f, 0x5e, 0x5f, 0x6c, 0x6d, 0x7c, 0x7d, 0x6e, 0x6f, 0x7e,
  0x7f, 0x81, 0x90, 0x91, 0x82, 0x83, 0x92, 0x93, 0xa0, 0xa1, 0xb0, 0xb1, 0xa2, 0xa3, 0xb2, 0xb3,
  0x84, 0x85, 0x94, 0x95, 0x86, 0x87, 0x96, 0x97, 0xa4, 0xa5, 0xb4, 0xb5, 0xa6, 0xa7, 0xb6, 0xb7,
  0xc1, 0xd0, 0xd1, 0xc2, 0xd2, 0xd3, 0xe0, 0xe1, 0xf1, 0xe2, 0xe3, 0xf2, 0xc4, 0xc5, 0xd4, 0xd5,
  0xc6, 0xc7, 0xd6, 0xd7, 0xe4, 0xe5, 0xf4, 0xf5, 0xe6, 0xe7, 0xf6, 0xf7, 0x89, 0x98, 0x99, 0x8a,
  0x8b, 0x9a, 0x9b, 0xa8, 0xa9, 0xb8, 0xb9, 0xaa, 0xab, 0xba, 0xbb, 0x8c, 0x8d, 0x9c, 0x9d, 0x8e,
  0x8f, 0x9e, 0x9f, 0xac, 0xad, 0xbc, 0xbd, 0xae, 0xaf, 0xbe, 0xbf, 0xc8, 0xc9, 0xd8, 0xd9, 0xca,
  0xcb, 0xda, 0xdb, 0xe8, 0xe9, 0xf8, 0xf9, 0xea, 0xeb, 0xfa, 0xfb, 0xcd, 0xdc, 0xdd, 0xce, 0xde,
  0xdf, 0xec, 0xed, 0xfd, 0xef, 0xfe,
];

/// Legacy-computing octants U+1CD00-1CDE5: a 2-column, 4-row block mosaic.
#[expect(
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "octant geometry is bounded by the terminal cell"
)]
fn draw_octant(
  canvas: &mut Canvas,
  cp: u32,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) {
  let (w, h) = (m.width as i32, m.height as i32);
  let pat = OCTANTS[(cp - 0x1CD00) as usize];
  let cx = w / 2;
  let ry = |r: i32| top + (r * h) / 4;
  for bit in 0..8u8 {
    if pat & (1 << bit) == 0 {
      continue;
    }
    let left = bit < 4;
    let row = i32::from(bit & 3);
    let xa = if left { x0 } else { x0 + cx };
    let xw = if left { cx } else { w - cx };
    let (ya, yb) = (ry(row), ry(row + 1));
    canvas.fill_rect(xa, ya, xw as u32, (yb - ya) as u32, fg);
  }
}

/// Box drawing U+2500-257F. Lines are drawn as arms from the cell centre to its
/// edges at a weight per side (light/heavy/double); dashes, rounded arcs, and
/// diagonals are handled specially. Returns `false` for an unhandled codepoint.
#[expect(
  clippy::cast_possible_wrap,
  reason = "box-line geometry is bounded by the terminal cell"
)]
fn draw_box_line(
  canvas: &mut Canvas,
  cp: u32,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) -> bool {
  let (w, h) = (m.width as i32, m.height as i32);
  // Light lines use the font's stroke weight; heavy lines are ~2x, clamped so
  // they stay distinct from light even at small sizes.
  let thin = (m.stroke as i32).clamp(1, (h / 3).max(1));
  let heavy = (thin * 2).max(thin + 1);
  let (midx, midy) = (x0 + w / 2, top + h / 2);
  let (right, bottom) = (x0 + w, top + h);

  // Diagonals.
  match cp {
    0x2571 => {
      draw_diagonal(canvas, x0, bottom, right, top, thin, fg);
      return true;
    },
    0x2572 => {
      draw_diagonal(canvas, x0, top, right, bottom, thin, fg);
      return true;
    },
    0x2573 => {
      draw_diagonal(canvas, x0, bottom, right, top, thin, fg);
      draw_diagonal(canvas, x0, top, right, bottom, thin, fg);
      return true;
    },
    _ => {},
  }

  // Dashed lines: a run of 2/3/4 dashes spanning the full width or height.
  let dash: Option<(bool, i32, i32)> = match cp {
    0x2504 => Some((true, thin, 3)),
    0x2505 => Some((true, heavy, 3)),
    0x2506 => Some((false, thin, 3)),
    0x2507 => Some((false, heavy, 3)),
    0x2508 => Some((true, thin, 4)),
    0x2509 => Some((true, heavy, 4)),
    0x250A => Some((false, thin, 4)),
    0x250B => Some((false, heavy, 4)),
    0x254C => Some((true, thin, 2)),
    0x254D => Some((true, heavy, 2)),
    0x254E => Some((false, thin, 2)),
    0x254F => Some((false, heavy, 2)),
    _ => None,
  };
  if let Some((horizontal, t, n)) = dash {
    if horizontal {
      draw_dashes(canvas, x0, w, midy, t, n, fg, true);
    } else {
      draw_dashes(canvas, top, h, midx, t, n, fg, false);
    }
    return true;
  }

  // Rounded arcs.
  if (0x256D..=0x2570).contains(&cp) {
    draw_arc(canvas, cp, x0, top, w, h, thin, fg);
    return true;
  }

  // Straight arms. `[up, down, left, right]`, weight 0 none / 1 light / 2 heavy
  // / 3 double.
  let Some(arms) = box_arms(cp) else {
    return false;
  };
  // Single (light/heavy) arms run centre-to-edge; the thickness is heavy for
  // weight 2. Double arms (weight 3) are drawn together in `draw_doubles` so
  // their rails close at junctions instead of leaving gapped corners.
  let t = |w: u8| if w == 2 { heavy } else { thin };
  if matches!(arms[0], 1 | 2) {
    v_seg(canvas, top, midy, midx, t(arms[0]), fg);
  }
  if matches!(arms[1], 1 | 2) {
    v_seg(canvas, midy, bottom, midx, t(arms[1]), fg);
  }
  if matches!(arms[2], 1 | 2) {
    h_seg(canvas, x0, midx, midy, t(arms[2]), fg);
  }
  if matches!(arms[3], 1 | 2) {
    h_seg(canvas, midx, right, midy, t(arms[3]), fg);
  }
  draw_doubles(
    canvas,
    arms,
    (x0, top, right, bottom),
    (midx, midy),
    thin,
    fg,
  );
  true
}

/// A horizontal bar of thickness `t` from `xa` to `xb`, centred on row `cy`.
#[expect(
  clippy::cast_sign_loss,
  reason = "line lengths and thickness are clamped non-negative before \
            conversion"
)]
fn h_seg(canvas: &mut Canvas, xa: i32, xb: i32, cy: i32, t: i32, fg: Rgb) {
  canvas.fill_rect(xa, cy - t / 2, (xb - xa).max(0) as u32, t as u32, fg);
}

/// A vertical bar of thickness `t` from `ya` to `yb`, centred on column `cx`.
#[expect(
  clippy::cast_sign_loss,
  reason = "line lengths and thickness are clamped non-negative before \
            conversion"
)]
fn v_seg(canvas: &mut Canvas, ya: i32, yb: i32, cx: i32, t: i32, fg: Rgb) {
  canvas.fill_rect(cx - t / 2, ya, t as u32, (yb - ya).max(0) as u32, fg);
}

/// Draw the double-line (weight 3) arms as pairs of rails straddling the cell
/// centre. Each rail extends past the centre to the perpendicular outer rail
/// when a perpendicular double exists, so corners and crosses close cleanly
/// (`╔ ╬ ╠`) rather than leaving the gapped corners that independent per-arm
/// lines produce. `arms` is `[up, down, left, right]`; `edges` is
/// `(x0, top, right, bottom)`.
fn draw_doubles(
  canvas: &mut Canvas,
  arms: [u8; 4],
  edges: (i32, i32, i32, i32),
  mid: (i32, i32),
  thin: i32,
  fg: Rgb,
) {
  let (x0, top, right, bottom) = edges;
  let (midx, midy) = mid;
  let d = thin + 1;
  let h_dbl = arms[2] == 3 || arms[3] == 3;
  let v_dbl = arms[0] == 3 || arms[1] == 3;
  if h_dbl {
    // Reach to the outer/inner vertical rail when a vertical double is present,
    // else stop at the single centre line.
    let xa = if arms[2] == 3 {
      x0
    } else if v_dbl {
      midx - d
    } else {
      midx
    };
    let xb = if arms[3] == 3 {
      right
    } else if v_dbl {
      midx + d
    } else {
      midx
    };
    h_seg(canvas, xa, xb, midy - d, thin, fg);
    h_seg(canvas, xa, xb, midy + d, thin, fg);
  }
  if v_dbl {
    let ya = if arms[0] == 3 {
      top
    } else if h_dbl {
      midy - d
    } else {
      midy
    };
    let yb = if arms[1] == 3 {
      bottom
    } else if h_dbl {
      midy + d
    } else {
      midy
    };
    v_seg(canvas, ya, yb, midx - d, thin, fg);
    v_seg(canvas, ya, yb, midx + d, thin, fg);
  }
}

/// Draw `n` dashes of thickness `t` evenly along a `span`-long axis starting at
/// `start`, centred on the cross-axis coordinate `cross`.
#[expect(
  clippy::too_many_arguments,
  reason = "renderer hot path keeps pixel parameters explicit"
)]
#[expect(
  clippy::cast_sign_loss,
  reason = "dash geometry is bounded by the terminal cell"
)]
fn draw_dashes(
  canvas: &mut Canvas,
  start: i32,
  span: i32,
  cross: i32,
  t: i32,
  n: i32,
  fg: Rgb,
  horizontal: bool,
) {
  let slot = (span / n).max(1);
  let dash = (slot * 2 / 3).max(1) as u32;
  for i in 0..n {
    let a = start + i * slot;
    if horizontal {
      canvas.fill_rect(a, cross - t / 2, dash, t as u32, fg);
    } else {
      canvas.fill_rect(cross - t / 2, a, t as u32, dash, fg);
    }
  }
}

/// Draw a rounded corner (U+256D-2570): two straight stubs from the cell edges
/// meeting a quarter-circle at the centre.
#[expect(
  clippy::too_many_arguments,
  reason = "renderer hot path keeps pixel parameters explicit"
)]
#[expect(
  clippy::cast_sign_loss,
  reason = "arc geometry is bounded by the terminal cell"
)]
fn draw_arc(
  canvas: &mut Canvas,
  cp: u32,
  x0: i32,
  top: i32,
  w: i32,
  h: i32,
  thin: i32,
  fg: Rgb,
) {
  let (midx, midy) = (x0 + w / 2, top + h / 2);
  let (right, bottom) = (x0 + w, top + h);
  let r = (w / 2).min(h / 2).max(1);
  // Each arc is a quarter-circle whose centre sits one radius diagonally inward
  // from the corner; two straight stubs join it to the cell edges.
  let hstub_r = |c: &mut Canvas| {
    c.fill_rect(
      midx + r,
      midy - thin / 2,
      (right - midx - r).max(0) as u32,
      thin as u32,
      fg,
    );
  };
  let hstub_l = |c: &mut Canvas| {
    c.fill_rect(
      x0,
      midy - thin / 2,
      (midx - r - x0).max(0) as u32,
      thin as u32,
      fg,
    );
  };
  let vstub_d = |c: &mut Canvas| {
    c.fill_rect(
      midx - thin / 2,
      midy + r,
      thin as u32,
      (bottom - midy - r).max(0) as u32,
      fg,
    );
  };
  let vstub_u = |c: &mut Canvas| {
    c.fill_rect(
      midx - thin / 2,
      top,
      thin as u32,
      (midy - r - top).max(0) as u32,
      fg,
    );
  };
  // The arc is the circle quadrant facing the cell centre: `(x_pos, y_pos)`
  // pick which side of the arc centre that quadrant lies on.
  match cp {
    // Down + right: stubs run right and down; arc is the top-left quadrant.
    0x256D => {
      hstub_r(canvas);
      vstub_d(canvas);
      arc_quarter(canvas, midx + r, midy + r, r, false, false, thin, fg);
    },
    0x256E => {
      hstub_l(canvas);
      vstub_d(canvas);
      arc_quarter(canvas, midx - r, midy + r, r, true, false, thin, fg);
    },
    0x256F => {
      hstub_l(canvas);
      vstub_u(canvas);
      arc_quarter(canvas, midx - r, midy - r, r, true, true, thin, fg);
    },
    0x2570 => {
      hstub_r(canvas);
      vstub_u(canvas);
      arc_quarter(canvas, midx + r, midy - r, r, false, true, thin, fg);
    },
    _ => {},
  }
}

/// Plot an antialiased 90-degree arc of radius `r` and thickness `thin` about
/// `(cx, cy)`, in the quadrant selected by `(x_pos, y_pos)` (whether that
/// quadrant lies on the positive x/y side of the centre).
#[expect(
  clippy::too_many_arguments,
  reason = "renderer hot path keeps pixel parameters explicit"
)]
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  clippy::cast_precision_loss,
  reason = "arc sampling stays within bounded cell coordinates"
)]
fn arc_quarter(
  canvas: &mut Canvas,
  cx: i32,
  cy: i32,
  r: i32,
  x_pos: bool,
  y_pos: bool,
  thin: i32,
  fg: Rgb,
) {
  let half = thin as f32 / 2.0;
  let (r_f, cxf, cyf) = (r as f32, cx as f32, cy as f32);
  let (x_lo, x_hi) = if x_pos { (cx, cx + r) } else { (cx - r, cx) };
  let (y_lo, y_hi) = if y_pos { (cy, cy + r) } else { (cy - r, cy) };
  for py in y_lo..=y_hi {
    for px in x_lo..=x_hi {
      let dxp = px as f32 + 0.5 - cxf;
      let dyp = py as f32 + 0.5 - cyf;
      // Keep to the requested quadrant (a small slack avoids clipping the
      // pixels where the arc meets the straight stubs on the axes).
      if (x_pos && dxp < -0.5) || (!x_pos && dxp > 0.5) {
        continue;
      }
      if (y_pos && dyp < -0.5) || (!y_pos && dyp > 0.5) {
        continue;
      }
      // Coverage from the pixel's distance to the ring of radius `r`.
      let ring = (dxp.hypot(dyp) - r_f).abs();
      let cov = (half + 0.5 - ring).clamp(0.0, 1.0);
      if cov > 0.0 {
        canvas.blend(px, py, fg, (cov * 255.0).round() as u8);
      }
    }
  }
}

/// Draw an antialiased line of thickness `thin` between two points (the
/// box-drawing diagonals `╱ ╲ ╳`). Coverage is the pixel's distance from the
/// segment, feathered over the last pixel, so the stroke is smooth rather than
/// the hard staircase a stamped-square line produces.
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  clippy::cast_precision_loss,
  reason = "diagonal geometry is bounded by the terminal cell"
)]
fn draw_diagonal(
  canvas: &mut Canvas,
  xa: i32,
  ya: i32,
  xb: i32,
  yb: i32,
  thin: i32,
  fg: Rgb,
) {
  let (ax, ay) = (xa as f32, ya as f32);
  let (dx, dy) = ((xb - xa) as f32, (yb - ya) as f32);
  let len2 = dy.mul_add(dy, dx * dx);
  if len2 <= 0.0 {
    return;
  }
  let half = thin as f32 / 2.0;
  let (lo_x, hi_x) = (xa.min(xb), xa.max(xb));
  let (lo_y, hi_y) = (ya.min(yb), ya.max(yb));
  for py in lo_y..=hi_y {
    for px in lo_x..=hi_x {
      let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
      // Distance from the pixel centre to the (clamped) segment.
      let t = ((fy - ay).mul_add(dy, (fx - ax) * dx) / len2).clamp(0.0, 1.0);
      let (cx, cy) = (ax + t * dx, ay + t * dy);
      let dist = (fx - cx).hypot(fy - cy);
      let cov = (half + 0.5 - dist).clamp(0.0, 1.0);
      if cov > 0.0 {
        canvas.blend(px, py, fg, (cov * 255.0).round() as u8);
      }
    }
  }
}

/// Arm weights `[up, down, left, right]` for a straight box-drawing codepoint:
/// 0 none, 1 light, 2 heavy, 3 double. `None` for codepoints handled elsewhere
/// (dashes, arcs, diagonals) or outside the solid-line set.
pub(super) const fn box_arms(cp: u32) -> Option<[u8; 4]> {
  let a = match cp {
    0x2500 => [0, 0, 1, 1],
    0x2501 => [0, 0, 2, 2],
    0x2502 => [1, 1, 0, 0],
    0x2503 => [2, 2, 0, 0],
    0x250C => [0, 1, 0, 1],
    0x250D => [0, 1, 0, 2],
    0x250E => [0, 2, 0, 1],
    0x250F => [0, 2, 0, 2],
    0x2510 => [0, 1, 1, 0],
    0x2511 => [0, 1, 2, 0],
    0x2512 => [0, 2, 1, 0],
    0x2513 => [0, 2, 2, 0],
    0x2514 => [1, 0, 0, 1],
    0x2515 => [1, 0, 0, 2],
    0x2516 => [2, 0, 0, 1],
    0x2517 => [2, 0, 0, 2],
    0x2518 => [1, 0, 1, 0],
    0x2519 => [1, 0, 2, 0],
    0x251A => [2, 0, 1, 0],
    0x251B => [2, 0, 2, 0],
    0x251C => [1, 1, 0, 1],
    0x251D => [1, 1, 0, 2],
    0x251E => [2, 1, 0, 1],
    0x251F => [1, 2, 0, 1],
    0x2520 => [2, 2, 0, 1],
    0x2521 => [2, 1, 0, 2],
    0x2522 => [1, 2, 0, 2],
    0x2523 => [2, 2, 0, 2],
    0x2524 => [1, 1, 1, 0],
    0x2525 => [1, 1, 2, 0],
    0x2526 => [2, 1, 1, 0],
    0x2527 => [1, 2, 1, 0],
    0x2528 => [2, 2, 1, 0],
    0x2529 => [2, 1, 2, 0],
    0x252A => [1, 2, 2, 0],
    0x252B => [2, 2, 2, 0],
    0x252C => [0, 1, 1, 1],
    0x252D => [0, 1, 2, 1],
    0x252E => [0, 1, 1, 2],
    0x252F => [0, 1, 2, 2],
    0x2530 => [0, 2, 1, 1],
    0x2531 => [0, 2, 2, 1],
    0x2532 => [0, 2, 1, 2],
    0x2533 => [0, 2, 2, 2],
    0x2534 => [1, 0, 1, 1],
    0x2535 => [1, 0, 2, 1],
    0x2536 => [1, 0, 1, 2],
    0x2537 => [1, 0, 2, 2],
    0x2538 => [2, 0, 1, 1],
    0x2539 => [2, 0, 2, 1],
    0x253A => [2, 0, 1, 2],
    0x253B => [2, 0, 2, 2],
    0x253C => [1, 1, 1, 1],
    0x253D => [1, 1, 2, 1],
    0x253E => [1, 1, 1, 2],
    0x253F => [1, 1, 2, 2],
    0x2540 => [2, 1, 1, 1],
    0x2541 => [1, 2, 1, 1],
    0x2542 => [2, 2, 1, 1],
    0x2543 => [2, 1, 2, 1],
    0x2544 => [2, 1, 1, 2],
    0x2545 => [1, 2, 2, 1],
    0x2546 => [1, 2, 1, 2],
    0x2547 => [2, 1, 2, 2],
    0x2548 => [1, 2, 2, 2],
    0x2549 => [2, 2, 2, 1],
    0x254A => [2, 2, 1, 2],
    0x254B => [2, 2, 2, 2],
    0x2550 => [0, 0, 3, 3],
    0x2551 => [3, 3, 0, 0],
    0x2552 => [0, 1, 0, 3],
    0x2553 => [0, 3, 0, 1],
    0x2554 => [0, 3, 0, 3],
    0x2555 => [0, 1, 3, 0],
    0x2556 => [0, 3, 1, 0],
    0x2557 => [0, 3, 3, 0],
    0x2558 => [1, 0, 0, 3],
    0x2559 => [3, 0, 0, 1],
    0x255A => [3, 0, 0, 3],
    0x255B => [1, 0, 3, 0],
    0x255C => [3, 0, 1, 0],
    0x255D => [3, 0, 3, 0],
    0x255E => [1, 1, 0, 3],
    0x255F => [3, 3, 0, 1],
    0x2560 => [3, 3, 0, 3],
    0x2561 => [1, 1, 3, 0],
    0x2562 => [3, 3, 1, 0],
    0x2563 => [3, 3, 3, 0],
    0x2564 => [0, 1, 3, 3],
    0x2565 => [0, 3, 1, 1],
    0x2566 => [0, 3, 3, 3],
    0x2567 => [1, 0, 3, 3],
    0x2568 => [3, 0, 1, 1],
    0x2569 => [3, 0, 3, 3],
    0x256A => [1, 1, 3, 3],
    0x256B => [3, 3, 1, 1],
    0x256C => [3, 3, 3, 3],
    0x2574 => [0, 0, 1, 0],
    0x2575 => [1, 0, 0, 0],
    0x2576 => [0, 0, 0, 1],
    0x2577 => [0, 1, 0, 0],
    0x2578 => [0, 0, 2, 0],
    0x2579 => [2, 0, 0, 0],
    0x257A => [0, 0, 0, 2],
    0x257B => [0, 2, 0, 0],
    0x257C => [0, 0, 1, 2],
    0x257D => [1, 2, 0, 0],
    0x257E => [0, 0, 2, 1],
    0x257F => [2, 1, 0, 0],
    _ => return None,
  };
  Some(a)
}
mod powerline;

use powerline::draw_powerline;
