//! Rasterization for Powerline separator symbols.

use super::super::canvas::Canvas;
use crate::{font::CellMetrics, theme::Rgb};
pub(super) fn draw_powerline(
  canvas: &mut Canvas,
  cp: u32,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) {
  match cp {
    0xE0B0 => draw_triangle(canvas, x0, top, m, fg, true, true),
    0xE0B1 => draw_triangle(canvas, x0, top, m, fg, true, false),
    0xE0B2 => draw_triangle(canvas, x0, top, m, fg, false, true),
    0xE0B3 => draw_triangle(canvas, x0, top, m, fg, false, false),
    0xE0B4 => draw_semicircle(canvas, x0, top, m, fg, true, true),
    0xE0B5 => draw_semicircle(canvas, x0, top, m, fg, true, false),
    0xE0B6 => draw_semicircle(canvas, x0, top, m, fg, false, true),
    0xE0B7 => draw_semicircle(canvas, x0, top, m, fg, false, false),
    0xE0B8 | 0xE0BA | 0xE0BC | 0xE0BE => {
      draw_powerline_slant(canvas, cp, x0, top, m, fg);
    },
    _ => {},
  }
}

/// Filled Powerline triangles use cell corners, independent of font bearings.
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "cell coordinates and clamped glyph coverage are bounded"
)]
fn draw_powerline_slant(
  canvas: &mut Canvas,
  cp: u32,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) {
  let slope = f64::from(m.width) / f64::from(m.height);
  for y in 0..m.height {
    let sample_y = if cp >= 0xE0BC { m.height - 1 - y } else { y };
    for x in 0..m.width {
      let sample_x = if matches!(cp, 0xE0BA | 0xE0BE) {
        m.width - 1 - x
      } else {
        x
      };
      // Integrate horizontal coverage over eight subrows. Fully covered
      // pixels stay opaque, including the horizontal and vertical joins.
      let mut coverage = 0.0;
      for subrow in 0..8 {
        let edge =
          (f64::from(sample_y) + (f64::from(subrow) + 0.5) / 8.0) * slope;
        coverage += (edge - f64::from(sample_x)).clamp(0.0, 1.0);
      }
      canvas.blend(
        x0 + x as i32,
        top + y as i32,
        fg,
        (coverage * (255.0 / 8.0)).round() as u8,
      );
    }
  }
}

#[expect(
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "powerline geometry is bounded by the terminal cell"
)]
fn draw_triangle(
  canvas: &mut Canvas,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
  right: bool,
  filled: bool,
) {
  let (w, h) = (m.width as i32, m.height as i32);
  let center = h / 2;
  for y in 0..h {
    let radius = if y <= center { center } else { h - 1 - center }.max(1);
    let edge = (w - 1) * (radius - (y - center).abs()).max(0) / radius;
    let edge = if right { edge } else { w - 1 - edge };
    if filled {
      let (start, width) = if right {
        (0, edge + 1)
      } else {
        (edge, w - edge)
      };
      canvas.hline(x0 + start, top + y, width as u32, fg);
    } else {
      canvas.put(x0 + edge, top + y, fg);
    }
  }
}

#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "powerline geometry is bounded by the terminal cell"
)]
fn draw_semicircle(
  canvas: &mut Canvas,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
  right: bool,
  filled: bool,
) {
  let (w, h) = (m.width as i32, m.height as i32);
  let radius = f64::from((h - 1).max(1)) / 2.0;
  for y in 0..h {
    let dy = (f64::from(y) - radius) / radius;
    let extent = ((1.0 - dy * dy).max(0.0).sqrt() * f64::from(w - 1)) as i32;
    let edge = if right { extent } else { w - 1 - extent };
    if filled {
      let (start, width) = if right {
        (0, edge + 1)
      } else {
        (edge, w - edge)
      };
      canvas.hline(x0 + start, top + y, width as u32, fg);
    } else {
      canvas.put(x0 + edge, top + y, fg);
    }
  }
}
