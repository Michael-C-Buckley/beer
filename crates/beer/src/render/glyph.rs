//! Rasterized glyph compositing.

use super::canvas::Canvas;
use crate::{
  font::{CellMetrics, Glyph, GlyphData},
  theme::Rgb,
};

/// Composite a rasterized glyph, clipping to the vertical band `clip = (y0,
/// y1)` so a tall text-sizing glyph paints only the part belonging to the
/// current row. Mask glyphs are tinted with `fg`; colour glyphs are scaled to
/// `target_h` (the outline transform does not scale embedded bitmaps).
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  clippy::cast_precision_loss,
  reason = "glyph placement converts bounded font metrics into pixel \
            coordinates"
)]
pub(super) fn blit_glyph_clipped(
  canvas: &mut Canvas,
  glyph: &Glyph,
  pen_x: i32,
  baseline: i32,
  clip: (i32, i32),
  fg: Rgb,
  target_h: i32,
) {
  let (gw, gh) = (glyph.width as i32, glyph.height as i32);
  match &glyph.data {
    GlyphData::Mask(mask) => {
      for gy in 0..gh {
        let py = baseline - glyph.top + gy;
        if py < clip.0 || py >= clip.1 {
          continue;
        }
        for gx in 0..gw {
          let a = mask[(gy * gw + gx) as usize];
          if a != 0 {
            canvas.blend(pen_x + glyph.left + gx, py, fg, a);
          }
        }
      }
    },
    // Scaled (OSC 66) glyphs are rasterized grayscale, so LCD coverage does not
    // arise here; collapse the middle subpixel to a coverage value defensively.
    GlyphData::Lcd(sub) => {
      let stride = gw * 3;
      for gy in 0..gh {
        let py = baseline - glyph.top + gy;
        if py < clip.0 || py >= clip.1 {
          continue;
        }
        for gx in 0..gw {
          let a = sub[(gy * stride + gx * 3 + 1) as usize];
          if a != 0 {
            canvas.blend(pen_x + glyph.left + gx, py, fg, a);
          }
        }
      }
    },
    GlyphData::Color(bgra) if gh > 0 => {
      let sc = target_h as f32 / gh as f32;
      let tw = (gw as f32 * sc).round() as i32;
      // Place the scaled bitmap so most of it sits above the baseline.
      let top = baseline - target_h * 4 / 5;
      for ty in 0..target_h {
        let py = top + ty;
        if py < clip.0 || py >= clip.1 {
          continue;
        }
        let sy = (ty as f32 + 0.5) / sc - 0.5;
        for tx in 0..tw {
          let sx = (tx as f32 + 0.5) / sc - 0.5;
          let px = sample_bilinear(bgra, gw, gh, sx, sy);
          canvas.over(pen_x + tx, py, &px);
        }
      }
    },
    GlyphData::Color(_) => {},
  }
}

/// Composite a rasterized glyph into the canvas. `origin_x`/`cell_top` are the
/// cell's top-left; `rise` lifts the glyph above the baseline (`HarfBuzz`'s
/// vertical offset, 0 for the unshaped path).
#[expect(
  clippy::too_many_arguments,
  reason = "renderer hot path keeps pixel parameters explicit"
)]
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  clippy::cast_precision_loss,
  reason = "glyph sampling uses bounded font metrics and canvas coordinates"
)]
pub(super) fn blit_glyph(
  canvas: &mut Canvas,
  glyph: &Glyph,
  m: CellMetrics,
  origin_x: i32,
  cell_top: i32,
  rise: i32,
  fg: Rgb,
  sub: Option<bool>,
) {
  let (gw, gh) = (glyph.width as i32, glyph.height as i32);
  match &glyph.data {
    GlyphData::Mask(mask) => {
      let baseline = cell_top + m.ascent as i32 - rise;
      for gy in 0..gh {
        for gx in 0..gw {
          let a = mask[(gy * gw + gx) as usize];
          if a != 0 {
            canvas.blend(
              origin_x + glyph.left + gx,
              baseline - glyph.top + gy,
              fg,
              a,
            );
          }
        }
      }
    },
    // LCD subpixel coverage: three bytes per pixel. Over an opaque background
    // it blends per channel; over a translucent one it averages to a single
    // grayscale coverage, which is all a single-alpha buffer can represent.
    GlyphData::Lcd(cov_bytes) => {
      let baseline = cell_top + m.ascent as i32 - rise;
      let stride = gw * 3;
      for gy in 0..gh {
        for gx in 0..gw {
          let o = (gy * stride + gx * 3) as usize;
          let cov = [cov_bytes[o], cov_bytes[o + 1], cov_bytes[o + 2]];
          if cov[0] | cov[1] | cov[2] == 0 {
            continue;
          }
          let (px, py) =
            (origin_x + glyph.left + gx, baseline - glyph.top + gy);
          if let Some(bgr) = sub {
            canvas.blend_lcd(px, py, fg, cov, bgr);
          } else {
            let a =
              ((u32::from(cov[0]) + u32::from(cov[1]) + u32::from(cov[2])) / 3)
                as u8;
            canvas.blend(px, py, fg, a);
          }
        }
      }
    },
    // Colour glyphs (emoji) come from a fixed strike at native size;
    // scale them to the line height with nearest-neighbour sampling.
    GlyphData::Color(bgra) if gh > 0 => {
      let scale = m.height as f32 / gh as f32;
      let target_w = (gw as f32 * scale) as i32;
      for ty in 0..m.height as i32 {
        let sy = (ty as f32 + 0.5) / scale - 0.5;
        for tx in 0..target_w {
          let sx = (tx as f32 + 0.5) / scale - 0.5;
          let px = sample_bilinear(bgra, gw, gh, sx, sy);
          canvas.over(origin_x + tx, cell_top + ty, &px);
        }
      }
    },
    GlyphData::Color(_) => {},
  }
}

/// Bilinearly sample a premultiplied BGRA image at fractional `(fx, fy)`.
/// Premultiplied colour interpolates linearly, so this is correct to blend.
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  reason = "bilinear sampling clamps coordinates and channels to bounded \
            ranges"
)]
fn sample_bilinear(bgra: &[u8], w: i32, h: i32, fx: f32, fy: f32) -> [u8; 4] {
  let x0f = fx.floor();
  let y0f = fy.floor();
  let (dx, dy) = (fx - x0f, fy - y0f);
  let x0 = (x0f as i32).clamp(0, w - 1);
  let y0 = (y0f as i32).clamp(0, h - 1);
  let x1 = (x0 + 1).min(w - 1);
  let y1 = (y0 + 1).min(h - 1);
  let at =
    |x: i32, y: i32, c: usize| f32::from(bgra[((y * w + x) * 4) as usize + c]);
  let mut out = [0u8; 4];
  for (c, o) in out.iter_mut().enumerate() {
    let top = at(x0, y0, c) * (1.0 - dx) + at(x1, y0, c) * dx;
    let bot = at(x0, y1, c) * (1.0 - dx) + at(x1, y1, c) * dx;
    *o = (top * (1.0 - dy) + bot * dy).round().clamp(0.0, 255.0) as u8;
  }
  out
}
