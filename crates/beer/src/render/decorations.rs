//! Cell decoration rasterization.

use std::f64::consts::TAU;

use super::canvas::Canvas;
use crate::{
  font::CellMetrics,
  grid::{Cell, Color, Flags, Underline},
  theme::{Plane, Rgb, Theme},
};
/// Draw underline, strikethrough, and overline for one cell.
#[expect(
  clippy::cast_possible_wrap,
  reason = "decoration geometry is bounded by the terminal cell"
)]
pub(super) fn draw_decorations(
  canvas: &mut Canvas,
  cell: &Cell,
  theme: &Theme,
  x0: i32,
  top: i32,
  m: CellMetrics,
  fg: Rgb,
) {
  let w = m.width;
  let baseline = top + m.ascent as i32;
  let uy = (baseline + 1).min(top + m.height as i32 - 1);
  // A `Default` underline colour follows the cell's foreground.
  let uc = match cell.underline_color {
    Color::Default => fg,
    other => theme.resolve(other, Plane::Fg, false),
  };
  match cell.underline {
    Underline::None => {},
    Underline::Single => canvas.hline(x0, uy, w, uc),
    Underline::Double => {
      canvas.hline(x0, uy, w, uc);
      canvas.hline(x0, (uy - 2).max(top), w, uc);
    },
    Underline::Curly => {
      draw_undercurl(canvas, x0, top, uy, w, m.stroke, uc);
    },
    Underline::Dotted => {
      for dx in (0..w as i32).step_by(2) {
        canvas.put(x0 + dx, uy, uc);
      }
    },
    Underline::Dashed => {
      for dx in 0..w as i32 {
        if (dx / 3) % 2 == 0 {
          canvas.put(x0 + dx, uy, uc);
        }
      }
    },
  }
  if cell.flags.contains(Flags::OVERLINE) {
    canvas.hline(x0, top, w, fg);
  }
  if cell.flags.contains(Flags::STRIKE) {
    canvas.hline(x0, top + m.ascent as i32 * 2 / 3, w, fg);
  }
}

#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  reason = "underline geometry is bounded by the terminal cell"
)]
fn draw_undercurl(
  canvas: &mut Canvas,
  x0: i32,
  top: i32,
  baseline: i32,
  width: u32,
  stroke: u32,
  color: Rgb,
) {
  let amplitude = f64::from(stroke.clamp(2, 4));
  let period = f64::from(width.max(6));
  for dx in 0..width as i32 {
    let phase = TAU * f64::from(dx) / period;
    let y = baseline - (amplitude * (phase.sin() + 1.0) / 2.0).round() as i32;
    canvas.put(x0 + dx, y.max(top), color);
  }
}
