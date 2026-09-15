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
    _ => {},
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
