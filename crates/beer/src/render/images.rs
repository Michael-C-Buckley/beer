//! Image placement compositing.

use beer_protocols::graphics::{PLACEHOLDER, diacritic_value};

use super::canvas::Canvas;
use crate::{
  font::CellMetrics,
  graphics::{Graphics, Image, Placement},
  grid::{Cell, Color},
};

/// Composite the graphics-image cells of one row whose placement z-index passes
/// `z_filter` (one call below the text, one above). Each cell carries its
/// `(dx, dy)` in the placement; the engine supplies the pixels and geometry.
#[expect(
  clippy::too_many_arguments,
  reason = "renderer hot path keeps pixel parameters explicit"
)]
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  reason = "image placement converts bounded cell geometry into pixel \
            coordinates"
)]
pub(super) fn draw_image_cells(
  canvas: &mut Canvas,
  images: &Graphics,
  cells: &[Cell],
  cols: usize,
  pad_x: i32,
  row_top: i32,
  metrics: CellMetrics,
  z_filter: impl Fn(i32) -> bool,
) {
  for (x, cell) in cells.iter().take(cols).enumerate() {
    let Some(r) = cell.image else { continue };
    let Some(p) = images.placement(r.image, r.placement) else {
      continue;
    };
    if !z_filter(p.z) {
      continue;
    }
    let Some(img) = images.image(p.image) else {
      continue;
    };
    let origin_x = pad_x + x as i32 * metrics.width as i32;
    blit_image_cell(
      canvas,
      img,
      p,
      i32::from(r.dx),
      i32::from(r.dy),
      origin_x,
      row_top,
      metrics,
    );
  }
}

/// Composite the Unicode-placeholder cells of one row. A placeholder cell holds
/// `U+10EEEE`, its image id in the foreground colour, and its row/column as
/// combining diacritics; a missing row/column/id-byte is inherited from the
/// placeholder to the left, the way the protocol specifies.
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  reason = "placeholder geometry is bounded by the terminal cell and surface"
)]
pub(super) fn draw_placeholders(
  canvas: &mut Canvas,
  images: &Graphics,
  cells: &[Cell],
  cols: usize,
  pad_x: i32,
  row_top: i32,
  m: CellMetrics,
) {
  // The left neighbour's (row, column, id high byte, foreground), for cells
  // that omit diacritics and continue the run.
  let mut prev: Option<(u32, u32, u32, Color)> = None;
  for (x, cell) in cells.iter().take(cols).enumerate() {
    if cell.c != PLACEHOLDER {
      prev = None;
      continue;
    }
    let Some(base_id) = placeholder_id(cell.fg) else {
      prev = None;
      continue;
    };
    let marks: Vec<char> =
      cell.combining.as_deref().unwrap_or("").chars().collect();
    let d0 = marks.first().copied().and_then(diacritic_value);
    let d1 = marks.get(1).copied().and_then(diacritic_value);
    let d2 = marks.get(2).copied().and_then(diacritic_value);
    let same_fg = prev.is_some_and(|p| p.3 == cell.fg);
    let (row, col, msb) = match (d0, d1, d2, prev) {
      // No diacritics: continue the previous cell's row, next column.
      (None, None, None, Some(p)) if same_fg => (p.0, p.1 + 1, p.2),
      // Only the row: same row continues, next column.
      (Some(r), None, None, Some(p)) if same_fg && p.0 == r => {
        (r, p.1 + 1, p.2)
      },
      // Row and column given, id byte inherited from an adjacent run.
      (Some(r), Some(c), None, Some(p))
        if same_fg && p.0 == r && p.1 + 1 == c =>
      {
        (r, c, p.2)
      },
      // Otherwise take whatever was given, defaulting the rest to zero.
      (r, c, msb, _) => (r.unwrap_or(0), c.unwrap_or(0), msb.unwrap_or(0)),
    };
    prev = Some((row, col, msb, cell.fg));

    let id = base_id | (msb << 24);
    let Some(p) = images.placement(id, 0) else {
      continue;
    };
    let Some(img) = images.image(p.image) else {
      continue;
    };
    let origin_x = pad_x + x as i32 * m.width as i32;
    blit_image_cell(
      canvas, img, p, col as i32, row as i32, origin_x, row_top, m,
    );
  }
}

/// The image id a placeholder cell's foreground colour encodes: an indexed
/// colour is the id directly, a truecolor is its packed 24-bit value. A default
/// foreground carries no id.
fn placeholder_id(fg: Color) -> Option<u32> {
  match fg {
    Color::Indexed(n) => Some(u32::from(n)),
    Color::Rgb(r, g, b) => {
      Some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b))
    },
    Color::Default => None,
  }
}

/// Composite one cell's slice of an image placement. The placement's source
/// rectangle is scaled to its full cell-pixel area; this cell shows the
/// sub-rectangle for its `(dx, dy)`, sampled nearest-neighbour and
/// alpha-blended.
#[expect(
  clippy::too_many_arguments,
  reason = "renderer hot path keeps pixel parameters explicit"
)]
#[expect(
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "image sampling coordinates are bounded by the image placement"
)]
fn blit_image_cell(
  canvas: &mut Canvas,
  img: &Image,
  p: &Placement,
  dx: i32,
  dy: i32,
  origin_x: i32,
  row_top: i32,
  m: CellMetrics,
) {
  let (cell_w, cell_h) = (m.width as i32, m.height as i32);
  // The source rectangle is scaled to the cell area less the first-cell pixel
  // offset, so a non-zero X/Y shifts the image inward from the top-left cell.
  let span_w = (i32::from(p.cols) * cell_w - p.off_x as i32).max(1);
  let span_h = (i32::from(p.rows) * cell_h - p.off_y as i32).max(1);
  let src_w = if p.src_w == 0 { img.width } else { p.src_w } as i32;
  let src_h = if p.src_h == 0 { img.height } else { p.src_h } as i32;
  let (iw, ih) = (img.width as i32, img.height as i32);
  for cy in 0..cell_h {
    // Map this cell row to a source row through the placement's scale.
    let placed_y = dy * cell_h + cy - p.off_y as i32;
    if placed_y < 0 {
      continue;
    }
    let sy = p.src_y as i32 + placed_y * src_h / span_h;
    if sy < 0 || sy >= ih {
      continue;
    }
    for cx in 0..cell_w {
      let placed_x = dx * cell_w + cx - p.off_x as i32;
      if placed_x < 0 {
        continue;
      }
      let sx = p.src_x as i32 + placed_x * src_w / span_w;
      if sx < 0 || sx >= iw {
        continue;
      }
      let i = ((sy * iw + sx) * 4) as usize;
      let px = &img.current_rgba()[i..i + 4];
      canvas
        .blend_rgba(origin_x + cx, row_top + cy, [px[0], px[1], px[2], px[3]]);
    }
  }
}
