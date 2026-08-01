//! Software renderer: compose the grid into an ARGB8888 buffer.
//!
//! The target is a `wl_shm` buffer in `Argb8888`, which on little-endian is
//! `[B, G, R, A]` per pixel. Rendering is two passes per frame - backgrounds
//! then glyphs - so a wide glyph that overflows its cell is not clipped by the
//! neighbouring cell's background fill.

use std::{mem, num::NonZeroU16, sync::LazyLock};

use beer_protocols::{
  graphics::{PLACEHOLDER, diacritic_value},
  text_size::{HAlign, VAlign},
};

use crate::{
  config::{AlphaBlending, Subpixel},
  font::{CellMetrics, Fonts, Glyph, GlyphData, Style},
  graphics::{Graphics, Image, Placement},
  grid::{Cell, Color, CursorShape, Flags, Grid, Underline},
  theme::{Plane, Rgb, Theme},
};

/// sRGB (8-bit) → linear-light `[0, 1]` lookup, for gamma-correct compositing.
#[expect(
  clippy::cast_precision_loss,
  reason = "the fixed 8-bit sRGB lookup intentionally maps a bounded index to \
            f32"
)]
static SRGB_TO_LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
  let mut t = [0f32; 256];
  for (i, v) in t.iter_mut().enumerate() {
    *v = srgb_to_linear_f(i as f32 / 255.0);
  }
  t
});

/// sRGB transfer decode of a normalized `[0, 1]` channel to linear light.
fn srgb_to_linear_f(c: f32) -> f32 {
  if c <= 0.04045 {
    c / 12.92
  } else {
    ((c + 0.055) / 1.055).powf(2.4)
  }
}

/// sRGB transfer encode of a linear `[0, 1]` channel back to a normalized
/// float.
fn linear_to_srgb_f(c: f32) -> f32 {
  let c = c.clamp(0.0, 1.0);
  if c <= 0.003_130_8 {
    c * 12.92
  } else {
    1.055f32.mul_add(c.powf(1.0 / 2.4), -0.055)
  }
}

/// Encode a linear channel to an 8-bit sRGB value.
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  reason = "the channel is clamped to the complete u8 range before encoding"
)]
fn linear_to_srgb(c: f32) -> u8 {
  (linear_to_srgb_f(c) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Rec. 709 relative luminance of a linear RGB triple.
fn luminance(rgb: [f32; 3]) -> f32 {
  0.0722f32.mul_add(rgb[2], 0.7152f32.mul_add(rgb[1], 0.2126 * rgb[0]))
}

/// A mutable view over a BGRA pixel buffer.
struct Canvas<'a> {
  pixels: &'a mut [u8],
  width:  usize,
  height: usize,
  /// How coverage is composited (see [`AlphaBlending`]). Set to `Native` when
  /// the destination is translucent, since linear blending needs an opaque
  /// destination.
  blend:  AlphaBlending,
}

#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "canvas coordinates are checked or clamped before indexing the \
            pixel buffer"
)]
impl Canvas<'_> {
  const fn index(&self, x: i32, y: i32) -> Option<usize> {
    if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
      return None;
    }
    Some((y as usize * self.width + x as usize) * 4)
  }

  fn fill_rect(&mut self, x0: i32, y0: i32, w: u32, h: u32, c: Rgb) {
    self.fill_rect_a(x0, y0, w, h, c, 0xFF);
  }

  /// Fill a rectangle with colour `c` at opacity `alpha`. The shm buffer is
  /// premultiplied ARGB, so a translucent fill stores `rgb * alpha`.
  fn fill_rect_a(
    &mut self,
    x0: i32,
    y0: i32,
    w: u32,
    h: u32,
    c: Rgb,
    alpha: u8,
  ) {
    let x_start = x0.max(0) as usize;
    let x_end = ((x0 + w as i32).max(0) as usize).min(self.width);
    let y_start = y0.max(0) as usize;
    let y_end = ((y0 + h as i32).max(0) as usize).min(self.height);
    if x_start >= x_end {
      return;
    }
    let a = u32::from(alpha);
    let pm = |v: u8| ((u32::from(v) * a) / 255) as u8;
    let bytes = [pm(c.2), pm(c.1), pm(c.0), alpha];
    for y in y_start..y_end {
      let row = &mut self.pixels
        [(y * self.width + x_start) * 4..(y * self.width + x_end) * 4];
      for px in row.chunks_exact_mut(4) {
        px.copy_from_slice(&bytes);
      }
    }
  }

  /// Alpha-blend `fg` over the existing pixel with coverage `a`, in the
  /// configured [`AlphaBlending`] space. The buffer is BGRA: index 0 is blue,
  /// index 2 is red.
  fn blend(&mut self, pixel_x: i32, pixel_y: i32, fg: Rgb, coverage: u8) {
    let Some(pixel_index) = self.index(pixel_x, pixel_y) else {
      return;
    };
    match self.blend {
      AlphaBlending::Native => {
        let (coverage, inverse) =
          (u32::from(coverage), u32::from(255 - coverage));
        let mix = |src: u8, dst: u8| {
          ((u32::from(src) * coverage + u32::from(dst) * inverse) / 255) as u8
        };
        self.pixels[pixel_index] = mix(fg.2, self.pixels[pixel_index]);
        self.pixels[pixel_index + 1] = mix(fg.1, self.pixels[pixel_index + 1]);
        self.pixels[pixel_index + 2] = mix(fg.0, self.pixels[pixel_index + 2]);
      },
      AlphaBlending::Linear | AlphaBlending::LinearCorrected => {
        let lut = &*SRGB_TO_LINEAR;
        // Foreground and destination in linear light.
        let foreground =
          [lut[fg.0 as usize], lut[fg.1 as usize], lut[fg.2 as usize]];
        let destination = [
          lut[self.pixels[pixel_index + 2] as usize],
          lut[self.pixels[pixel_index + 1] as usize],
          lut[self.pixels[pixel_index] as usize],
        ];
        let cov = f32::from(coverage) / 255.0;
        // Linear-corrected remaps the coverage so the blended luminance matches
        // what gamma-space (Native) blending would give, preserving perceived
        // stroke weight while keeping colour edges clean.
        let alpha = if self.blend == AlphaBlending::LinearCorrected {
          let foreground_luminance = luminance(foreground);
          let background_luminance = luminance(destination);
          if (foreground_luminance - background_luminance).abs() < 1e-6 {
            cov
          } else {
            let target =
              srgb_to_linear_f(linear_to_srgb_f(background_luminance).mul_add(
                1.0 - cov,
                linear_to_srgb_f(foreground_luminance) * cov,
              ));
            ((target - background_luminance)
              / (foreground_luminance - background_luminance))
              .clamp(0.0, 1.0)
          }
        } else {
          cov
        };
        let out = |fc: f32, dc: f32| {
          linear_to_srgb(dc.mul_add(1.0 - alpha, fc * alpha))
        };
        self.pixels[pixel_index] = out(foreground[2], destination[2]);
        self.pixels[pixel_index + 1] = out(foreground[1], destination[1]);
        self.pixels[pixel_index + 2] = out(foreground[0], destination[0]);
      },
    }
    self.pixels[pixel_index + 3] = 0xFF;
  }

  /// Alpha-blend `fg` over the destination with independent per-subpixel
  /// coverage (LCD text). `cov` is in `FreeType`'s physical order (leftmost,
  /// middle, rightmost subpixel); `bgr` swaps the outer channels for panels
  /// whose subpixels run blue-green-red rather than red-green-blue.
  fn blend_lcd(&mut self, x: i32, y: i32, fg: Rgb, cov: [u8; 3], bgr: bool) {
    let Some(i) = self.index(x, y) else { return };
    let (ar, ag, ab) = if bgr {
      (u32::from(cov[2]), u32::from(cov[1]), u32::from(cov[0]))
    } else {
      (u32::from(cov[0]), u32::from(cov[1]), u32::from(cov[2]))
    };
    let mix = |src: u8, dst: u8, a: u32| {
      ((u32::from(src) * a + u32::from(dst) * (255 - a)) / 255) as u8
    };
    // The shm buffer is BGRA: index 0 is blue, 2 is red.
    self.pixels[i] = mix(fg.2, self.pixels[i], ab);
    self.pixels[i + 1] = mix(fg.1, self.pixels[i + 1], ag);
    self.pixels[i + 2] = mix(fg.0, self.pixels[i + 2], ar);
    self.pixels[i + 3] = 0xFF;
  }

  /// Set a single opaque pixel.
  fn put(&mut self, x: i32, y: i32, c: Rgb) {
    if let Some(i) = self.index(x, y) {
      self.pixels[i] = c.2;
      self.pixels[i + 1] = c.1;
      self.pixels[i + 2] = c.0;
      self.pixels[i + 3] = 0xFF;
    }
  }

  fn hline(&mut self, x0: i32, y: i32, w: u32, c: Rgb) {
    self.fill_rect(x0, y, w, 1, c);
  }

  /// Composite one straight-alpha RGBA source pixel over the destination.
  fn blend_rgba(&mut self, x: i32, y: i32, rgba: [u8; 4]) {
    let a = u32::from(rgba[3]);
    if a == 0 {
      return;
    }
    let Some(i) = self.index(x, y) else { return };
    let inv = 255 - a;
    let mix = |src: u8, dst: u8| {
      ((u32::from(src) * a + u32::from(dst) * inv) / 255) as u8
    };
    self.pixels[i] = mix(rgba[2], self.pixels[i]);
    self.pixels[i + 1] = mix(rgba[1], self.pixels[i + 1]);
    self.pixels[i + 2] = mix(rgba[0], self.pixels[i + 2]);
    self.pixels[i + 3] = 0xFF;
  }

  /// Composite one pre-multiplied BGRA source pixel over the destination.
  fn over(&mut self, x: i32, y: i32, src: &[u8]) {
    let Some(i) = self.index(x, y) else { return };
    let inv = u32::from(255 - src[3]);
    let comp = |s: u8, dst: u8| {
      (u32::from(s) + u32::from(dst) * inv / 255).min(255) as u8
    };
    self.pixels[i] = comp(src[0], self.pixels[i]);
    self.pixels[i + 1] = comp(src[1], self.pixels[i + 1]);
    self.pixels[i + 2] = comp(src[2], self.pixels[i + 2]);
    self.pixels[i + 3] = 0xFF;
  }
}

/// Per-frame constants shared by every row: the colour scheme, focus, and the
/// current blink phase.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
  pub theme:        &'a Theme,
  pub focused:      bool,
  pub blink_on:     bool,
  /// Hyperlink currently under the pointer; its cells get a hover underline.
  pub hovered_link: Option<NonZeroU16>,
  /// The graphics engine, source of image pixels and placement geometry.
  pub images:       &'a Graphics,
}

#[derive(Debug)]
pub struct Renderer {
  fonts: Fonts,
  /// Inner padding `(x, y)` in pixels between the window edge and the grid.
  pad:   (i32, i32),
  /// Configured coverage compositing mode (downgraded to `Native` per frame
  /// when the background is translucent).
  blend: AlphaBlending,
}

#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  clippy::cast_precision_loss,
  reason = "renderer dimensions and cell geometry are bounded by the \
            compositor surface"
)]
impl Renderer {
  pub fn new(fonts: Fonts) -> Self {
    Self {
      fonts,
      pad: (0, 0),
      blend: AlphaBlending::default(),
    }
  }

  pub const fn metrics(&self) -> CellMetrics {
    self.fonts.metrics()
  }

  pub const fn set_padding(&mut self, pad_x: u32, pad_y: u32) {
    self.pad = (pad_x as i32, pad_y as i32);
  }

  pub const fn set_alpha_blending(&mut self, blend: AlphaBlending) {
    self.blend = blend;
  }

  /// The compositing mode to use this frame: the configured mode when the
  /// background is opaque, else `Native` (linear blending needs an opaque
  /// destination to read).
  const fn blend_for(&self, theme: &Theme) -> AlphaBlending {
    if theme.alpha == 0xFF {
      self.blend
    } else {
      AlphaBlending::Native
    }
  }

  /// Fill the whole buffer (including the padding margins) with the background
  /// colour. Called once per fresh shm buffer; per-row repaints then leave the
  /// margins untouched.
  pub fn clear(&self, pixels: &mut [u8], dims: (usize, usize), theme: &Theme) {
    let (width, height) = dims;
    let mut canvas = Canvas {
      pixels,
      width,
      height,
      blend: self.blend_for(theme),
    };
    canvas.fill_rect_a(
      0,
      0,
      width as u32,
      height as u32,
      theme.bg,
      theme.alpha,
    );
  }

  /// Repaint a single grid row `y` into `pixels` (BGRA, `width`×`height` px):
  /// clear the row band, fill backgrounds (and selection), draw glyphs and
  /// decorations, then the cursor if it sits on this row. `blink_on` is the
  /// current blink phase; blinking cells and a blinking cursor vanish when it
  /// is `false`. Painting one row at a time is what lets the caller damage
  /// only the rows that actually changed.
  pub fn render_row(
    &mut self,
    pixels: &mut [u8],
    dims: (usize, usize),
    grid: &Grid,
    frame: &Frame,
    y: usize,
  ) {
    let (theme, focused, blink_on) =
      (frame.theme, frame.focused, frame.blink_on);
    // Subpixel text needs an opaque destination; a translucent window falls
    // back to grayscale (see `sub_mode`).
    let opaque_bg = theme.alpha == 0xFF;
    let (width, height) = dims;
    let mut canvas = Canvas {
      pixels,
      width,
      height,
      blend: self.blend_for(theme),
    };
    let m = self.fonts.metrics();
    let (pad_x, pad_y) = self.pad;
    let cols = grid.cols();
    let row_top = pad_y + y as i32 * m.height as i32;
    canvas.fill_rect_a(
      0,
      row_top,
      width as u32,
      m.height,
      theme.bg,
      theme.alpha,
    );

    // Rows come through the scrollback viewport and may be shorter than
    // `cols` after a resize, so clamp with `take`.
    let abs = grid.view_to_abs(y);
    let cells = grid.view_row(y);
    let search = grid.search_spans_on(abs);
    let match_at = |x: usize| -> Option<bool> {
      search
        .iter()
        .find(|(lo, hi, _)| x >= *lo && x <= *hi)
        .map(|(_, _, current)| *current)
    };
    for (x, cell) in cells.iter().take(cols).enumerate() {
      // Focused match > selection > other match > the cell's own bg.
      let bg = match match_at(x) {
        Some(true) => theme.current_match_bg,
        _ if grid.is_selected(abs, x) => theme.selection_bg,
        Some(false) => theme.match_bg,
        None => cell_colors(cell, theme).1,
      };
      if bg != theme.bg {
        canvas.fill_rect(
          pad_x + x as i32 * m.width as i32,
          row_top,
          m.width,
          m.height,
          bg,
        );
      }
    }

    // Graphics images stacked below the text (negative z-index).
    draw_image_cells(
      &mut canvas,
      frame.images,
      cells,
      cols,
      pad_x,
      row_top,
      m,
      |z| z < 0,
    );

    for (x, cell) in cells.iter().take(cols).enumerate() {
      if cell.flags.contains(Flags::WIDE_CONT) {
        continue;
      }
      // Text-sizing (OSC 66) blocks are drawn from their per-row left edge,
      // clipped to this row's band; every other block cell is skipped.
      if let Some(sized) = &cell.sized {
        if sized.dx == 0
          && let Some((lead, dy)) = grid.sized_lead(y, x)
        {
          let origin_x = pad_x + x as i32 * m.width as i32;
          let (fg, _) = cell_colors(lead, theme);
          self.draw_sized(&mut canvas, lead, dy, (origin_x, row_top), m, fg);
        }
        continue;
      }
      if cell.flags.contains(Flags::BLINK) && !blink_on {
        continue;
      }
      // A Unicode placeholder cell shows an image slice, drawn in its own
      // pass below; never paint the placeholder code point as a glyph.
      if cell.c == PLACEHOLDER {
        continue;
      }
      let (fg, _) = cell_colors(cell, theme);
      let origin_x = pad_x + x as i32 * m.width as i32;
      let style = cell_style(cell);
      // A cell carrying combining marks is shaped as a cluster so the
      // marks land where the font's GPOS table wants them. Shaping returns
      // None for braille (drawn directly) and for clusters the face does
      // not fully cover, both of which fall through to the legacy path.
      let shaped = match &cell.combining {
        Some(marks) if !is_braille(cell.c) => {
          self.fonts.shape_cluster(cell.c, marks, style)
        },
        _ => None,
      };
      if let Some(shaped) = shaped {
        let sub = self.sub_mode(opaque_bg);
        for placed in &shaped.glyphs {
          if let Ok(glyph) =
            self.fonts.glyph_indexed(shaped.face_idx, placed.gid, style)
          {
            blit_glyph(
              &mut canvas,
              glyph,
              m,
              origin_x + placed.x,
              row_top,
              placed.y,
              fg,
              sub,
            );
          }
        }
      } else if is_braille(cell.c) {
        // Drawn directly so the dots are crisp and fill the cell, the
        // way tools like btop expect, rather than however the fallback
        // font happens to size its braille glyphs.
        draw_braille(&mut canvas, cell.c, origin_x, row_top, m, fg);
      } else if is_box_draw(cell.c)
        && draw_box(&mut canvas, cell.c, origin_x, row_top, m, fg)
      {
        // Box drawing, block elements, and sextants are drawn geometrically so
        // they fill the cell exactly and tile seamlessly - fonts leave seams.
      } else {
        if cell.c != ' ' {
          self.draw_glyph(
            &mut canvas,
            cell.c,
            style,
            origin_x,
            row_top,
            fg,
            opaque_bg,
          );
        }
        // No shaper available for this cluster: stack the marks over the
        // base using each mark glyph's own bearings.
        if let Some(marks) = &cell.combining {
          for mark in marks.chars() {
            self.draw_glyph(
              &mut canvas,
              mark,
              style,
              origin_x,
              row_top,
              fg,
              opaque_bg,
            );
          }
        }
      }
      draw_decorations(&mut canvas, cell, theme, origin_x, row_top, m, fg);
      // Underline an OSC 8 hyperlink while the pointer hovers over it.
      if cell.link.is_some() && cell.link == frame.hovered_link {
        canvas.hline(origin_x, row_top + m.height as i32 - 2, m.width, fg);
      }
    }

    // Graphics images stacked above the text (z-index >= 0).
    draw_image_cells(
      &mut canvas,
      frame.images,
      cells,
      cols,
      pad_x,
      row_top,
      m,
      |z| z >= 0,
    );
    // Unicode-placeholder image cells.
    draw_placeholders(
      &mut canvas,
      frame.images,
      cells,
      cols,
      pad_x,
      row_top,
      m,
    );

    // The cursor belongs to the live screen; hide it while scrolled back.
    if grid.view_at_bottom() && grid.cursor().1 == y {
      self.draw_cursor(&mut canvas, grid, theme, m, focused, blink_on);
    }
  }

  /// Draw the incremental-search prompt across the bottom row, over whatever
  /// grid content was there. The caller marks the bottom row dirty so this
  /// repaints whenever the query or match count changes.
  pub fn render_search_bar(
    &mut self,
    pixels: &mut [u8],
    dims: (usize, usize),
    theme: &Theme,
    row: usize,
    text: &str,
  ) {
    let (width, height) = dims;
    let mut canvas = Canvas {
      pixels,
      width,
      height,
      blend: self.blend_for(theme),
    };
    let m = self.fonts.metrics();
    let (pad_x, pad_y) = self.pad;
    let row_top = pad_y + row as i32 * m.height as i32;
    canvas.fill_rect(0, row_top, width as u32, m.height, theme.search_bar_bg);
    let style = Style {
      bold:   false,
      italic: false,
    };
    let mut x = pad_x;
    for c in text.chars() {
      if x as usize + m.width as usize > width {
        break;
      }
      if c != ' ' {
        self.draw_glyph(&mut canvas, c, style, x, row_top, theme.fg, true);
      }
      x += m.width as i32;
    }
  }

  /// Draw a URL hint label (e.g. `a`, `bc`) as a highlighted tag starting at
  /// viewport cell `(row, col)`, over whatever was there.
  pub fn render_label(
    &mut self,
    pixels: &mut [u8],
    dims: (usize, usize),
    theme: &Theme,
    row: usize,
    col: usize,
    text: &str,
  ) {
    let (width, height) = dims;
    let mut canvas = Canvas {
      pixels,
      width,
      height,
      blend: self.blend_for(theme),
    };
    let m = self.fonts.metrics();
    let (pad_x, pad_y) = self.pad;
    let row_top = pad_y + row as i32 * m.height as i32;
    let style = Style {
      bold:   true,
      italic: false,
    };
    let mut x = pad_x + col as i32 * m.width as i32;
    for c in text.chars() {
      if x as usize + m.width as usize > width {
        break;
      }
      canvas.fill_rect(x, row_top, m.width, m.height, theme.current_match_bg);
      if c != ' ' {
        self.draw_glyph(&mut canvas, c, style, x, row_top, theme.bg, true);
      }
      x += m.width as i32;
    }
  }

  /// Draw the IME preedit string inline, starting at grid cell `start_col` of
  /// row `row`, over whatever was there. The preedit sits on the selection
  /// background and is underlined so it reads as uncommitted, in-flight text.
  pub fn render_preedit(
    &mut self,
    pixels: &mut [u8],
    dims: (usize, usize),
    theme: &Theme,
    row: usize,
    start_col: usize,
    text: &str,
  ) {
    let (width, height) = dims;
    let mut canvas = Canvas {
      pixels,
      width,
      height,
      blend: self.blend_for(theme),
    };
    let m = self.fonts.metrics();
    let (pad_x, pad_y) = self.pad;
    let row_top = pad_y + row as i32 * m.height as i32;
    let style = Style {
      bold:   false,
      italic: false,
    };
    let mut x = pad_x + start_col as i32 * m.width as i32;
    for c in text.chars() {
      if x < 0 || x as usize + m.width as usize > width {
        break;
      }
      canvas.fill_rect(x, row_top, m.width, m.height, theme.selection_bg);
      if c != ' ' {
        self.draw_glyph(&mut canvas, c, style, x, row_top, theme.fg, true);
      }
      // Underline the run a row above the cell bottom.
      canvas.hline(x, row_top + m.height as i32 - 2, m.width, theme.fg);
      x += m.width as i32;
    }
  }

  /// Draw the cursor: a solid block/underline/beam when focused, a hollow
  /// outline when not. A blinking cursor shape is only drawn while `blink_on`.
  fn draw_cursor(
    &mut self,
    canvas: &mut Canvas,
    grid: &Grid,
    theme: &Theme,
    m: CellMetrics,
    focused: bool,
    blink_on: bool,
  ) {
    if !grid.cursor_visible() || (grid.cursor_blink() && !blink_on) {
      return;
    }
    let (cx, cy) = grid.cursor();
    let x0 = self.pad.0 + cx as i32 * m.width as i32;
    let top = self.pad.1 + cy as i32 * m.height as i32;
    // OSC 12 cursor colour wins, then the configured cursor colour, then fg.
    let color = grid
      .cursor_color()
      .map(|(r, g, b)| Rgb(r, g, b))
      .or(theme.cursor)
      .unwrap_or(theme.fg);

    if !focused {
      let right = x0 + m.width as i32 - 1;
      let bottom = top + m.height as i32 - 1;
      canvas.hline(x0, top, m.width, color);
      canvas.hline(x0, bottom, m.width, color);
      canvas.fill_rect(x0, top, 1, m.height, color);
      canvas.fill_rect(right, top, 1, m.height, color);
      return;
    }

    match grid.cursor_shape() {
      CursorShape::Block => {
        canvas.fill_rect(x0, top, m.width, m.height, color);
        let cell = grid.cell(cx, cy);
        if cell.c != ' ' && !cell.flags.contains(Flags::WIDE_CONT) {
          let (_, bg) = cell_colors(cell, theme);
          self.draw_glyph(canvas, cell.c, cell_style(cell), x0, top, bg, true);
        }
      },
      CursorShape::Underline => {
        canvas.fill_rect(x0, top + m.height as i32 - 2, m.width, 2, color);
      },
      CursorShape::Beam => canvas.fill_rect(x0, top, 2, m.height, color),
    }
  }

  /// How LCD glyphs should be composited: `Some(bgr)` renders subpixel in the
  /// panel's order, `None` renders grayscale. Subpixel coverage only composites
  /// correctly over an opaque destination, so translucent backgrounds
  /// (`opaque == false`) fall back to grayscale rather than fringe over an
  /// unknown desktop behind the window.
  const fn sub_mode(&self, opaque: bool) -> Option<bool> {
    match self.fonts.subpixel() {
      Subpixel::Rgb if opaque => Some(false),
      Subpixel::Bgr if opaque => Some(true),
      _ => None,
    }
  }

  #[expect(
    clippy::too_many_arguments,
    reason = "renderer hot path keeps pixel parameters explicit"
  )]
  fn draw_glyph(
    &mut self,
    canvas: &mut Canvas,
    c: char,
    style: Style,
    origin_x: i32,
    cell_top: i32,
    fg: Rgb,
    opaque: bool,
  ) {
    let m = self.fonts.metrics();
    let sub = self.sub_mode(opaque);
    let glyph = match self.fonts.glyph(c, style) {
      Ok(glyph) => glyph,
      Err(err) => {
        tracing::debug!("glyph {c:?}: {err}");
        return;
      },
    };
    blit_glyph(canvas, glyph, m, origin_x, cell_top, 0, fg, sub);
  }

  /// Draw the slice of a text-sizing (`OSC 66`) block that falls in one row.
  ///
  /// `lead` is the block's leading cell (holding the run text); `dy` is this
  /// row's offset within the block; `pos` is this row's left edge and top in
  /// pixels. The full block is `cols * width` by `rows * height` pixels,
  /// starting `dy` rows above the row top; glyphs are rasterized at the block's
  /// font scale and clipped to this row's band, so the block paints correctly
  /// across its several per-row repaints.
  fn draw_sized(
    &mut self,
    canvas: &mut Canvas,
    lead: &Cell,
    dy: usize,
    pos: (i32, i32),
    m: CellMetrics,
    fg: Rgb,
  ) {
    let (block_left_x, row_top) = pos;
    let Some(s) = lead.sized.as_deref() else {
      return;
    };
    let scale = s.size.font_scale();
    let (cols, rows) = (i32::from(s.cols), i32::from(s.rows));
    let block_top = row_top - dy as i32 * m.height as i32;
    let (block_w, block_h) = (cols * m.width as i32, rows * m.height as i32);
    let clip = (row_top, row_top + m.height as i32);

    let advance = (m.width as f32 * scale).round().max(1.0) as i32;
    let render_h = (m.height as f32 * scale).round().max(1.0) as i32;

    // The run: a packed string (w>0) or the leading grapheme (w==0).
    let run: Vec<char> = s
      .run
      .as_deref()
      .map_or_else(|| vec![lead.c], |text| text.chars().collect());
    let render_w = advance * run.len() as i32;

    // A fractional scale renders into an area smaller than the block, placed
    // by the v/h alignment; a whole scale fills the block (offsets zero).
    let (ox, oy) = if s.size.has_fraction() {
      let ox = match s.size.halign {
        HAlign::Left => 0,
        HAlign::Right => block_w - render_w,
        HAlign::Center => (block_w - render_w) / 2,
      };
      let oy = match s.size.valign {
        VAlign::Top => 0,
        VAlign::Bottom => block_h - render_h,
        VAlign::Middle => (block_h - render_h) / 2,
      };
      (ox.max(0), oy.max(0))
    } else {
      (0, 0)
    };

    let baseline = block_top + oy + (m.ascent as f32 * scale).round() as i32;
    let style = cell_style(lead);
    let mut pen_x = block_left_x + ox;
    for c in run {
      if c != ' '
        && let Ok(glyph) = self.fonts.glyph_scaled(c, style, scale)
      {
        blit_glyph_clipped(canvas, &glyph, pen_x, baseline, clip, fg, render_h);
      }
      pen_x += advance;
    }
  }
}

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
fn blit_glyph_clipped(
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
fn draw_image_cells(
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
fn draw_placeholders(
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
fn blit_glyph(
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

const fn cell_style(cell: &Cell) -> Style {
  Style {
    bold:   cell.flags.contains(Flags::BOLD),
    italic: cell.flags.contains(Flags::ITALIC),
  }
}

/// Resolve a cell's (foreground, background) RGB, applying reverse video,
/// bold-as-bright, dim, and hidden.
fn cell_colors(cell: &Cell, theme: &Theme) -> (Rgb, Rgb) {
  let bold = cell.flags.contains(Flags::BOLD);
  let mut fg = theme.resolve(cell.fg, Plane::Fg, bold);
  let mut bg = theme.resolve(cell.bg, Plane::Bg, false);
  if cell.flags.contains(Flags::REVERSE) {
    mem::swap(&mut fg, &mut bg);
  }
  if cell.flags.contains(Flags::DIM) {
    fg = blend_rgb(fg, bg);
  }
  if cell.flags.contains(Flags::HIDDEN) {
    fg = bg;
  }
  (fg, bg)
}

/// Mix `c` two-thirds of the way from `toward`, used for the dim attribute.
#[expect(
  clippy::cast_possible_truncation,
  reason = "the channel arithmetic is bounded to the u8 range"
)]
fn blend_rgb(c: Rgb, toward: Rgb) -> Rgb {
  let mix = |a: u8, b: u8| ((u32::from(a) * 2 + u32::from(b)) / 3) as u8;
  Rgb(mix(c.0, toward.0), mix(c.1, toward.1), mix(c.2, toward.2))
}

/// Whether `c` is a Braille Patterns codepoint (U+2800-U+28FF).
fn is_braille(c: char) -> bool {
  ('\u{2800}'..='\u{28ff}').contains(&c)
}

/// Braille dot geometry for a `width`×`height` cell: the square dot side `w`,
/// the two column origins, and the four row origins. Ported verbatim from
/// foot's `box-drawing.c` `draw_braille` - base size and spacing from the cell,
/// then leftover pixels distributed (dot → margin → spacing → margin → dot) so
/// dots land on exact pixels with no rounding drift.
#[expect(clippy::cast_sign_loss, reason = "cell geometry is non-negative")]
fn braille_geometry(width: i32, height: i32) -> (u32, [i32; 2], [i32; 4]) {
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
fn draw_braille(
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
const fn is_box_draw(c: char) -> bool {
  matches!(c as u32, 0x2500..=0x259F | 0x1FB00..=0x1FB3B | 0x1CD00..=0x1CDE5)
}

/// Draw a box-drawing, block-element, or sextant glyph directly into the cell.
/// These characters tile edge-to-edge in TUIs, so drawing them from the cell
/// geometry (rather than a font bitmap that may fall short of the cell) keeps
/// borders, bars, and block mosaics seamless. Returns `false` for a codepoint
/// in the box range that is not handled, so the caller falls back to the font.
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
const fn sextant_pattern(cp: u32) -> u8 {
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
const OCTANTS: [u8; 230] = [
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
const fn box_arms(cp: u32) -> Option<[u8; 4]> {
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

/// Draw underline, strikethrough, and overline for one cell.
#[expect(
  clippy::cast_possible_wrap,
  reason = "decoration geometry is bounded by the terminal cell"
)]
fn draw_decorations(
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
      for dx in 0..w as i32 {
        let wobble = i32::from((dx / 2) % 2 != 0);
        canvas.put(x0 + dx, uy - wobble, uc);
      }
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

#[cfg(test)]
mod tests {
  use super::{
    AlphaBlending,
    Canvas,
    CellMetrics,
    Rgb,
    box_arms,
    braille_geometry,
    draw_box,
    is_box_draw,
    sextant_pattern,
  };

  /// Render one box glyph into a fresh `size`x`size` buffer and return a
  /// predicate for whether a given pixel received ink.
  #[expect(
    clippy::cast_sign_loss,
    reason = "render tests call this helper only with positive cell sizes and \
              coordinates"
  )]
  fn render_glyph(c: char, size: i32) -> impl Fn(i32, i32) -> bool {
    let n = size as usize;
    let mut buf = vec![0u8; n * n * 4];
    let mut canvas = Canvas {
      pixels: &mut buf,
      width:  n,
      height: n,
      blend:  AlphaBlending::Native,
    };
    let m = CellMetrics {
      width:  size as u32,
      height: size as u32,
      ascent: (size * 3 / 4) as u32,
      stroke: (size / 8).max(1) as u32,
    };
    assert!(draw_box(&mut canvas, c, 0, 0, m, Rgb(255, 255, 255)));
    move |x: i32, y: i32| buf[((y * size + x) * 4) as usize] != 0
  }

  // Pinned to foot box-drawing.c draw_braille output (cross-checked numerically
  // identical across cell sizes 4..30 x 6..48); guards against drift.
  #[test]
  fn braille_geometry_matches_foot() {
    assert_eq!(braille_geometry(8, 18), (2, [1, 5], [2, 6, 10, 14]));
    assert_eq!(braille_geometry(10, 20), (2, [1, 6], [1, 6, 11, 16]));
    assert_eq!(braille_geometry(12, 27), (3, [1, 8], [1, 8, 15, 22]));
    assert_eq!(braille_geometry(7, 15), (1, [1, 4], [2, 5, 8, 11]));
  }

  #[test]
  fn box_draw_range_membership() {
    assert!(is_box_draw('\u{2500}')); // light horizontal
    assert!(is_box_draw('\u{2588}')); // full block
    assert!(is_box_draw('\u{259F}')); // quadrant
    assert!(is_box_draw('\u{1FB00}')); // first sextant
    assert!(is_box_draw('\u{1FB3B}')); // last sextant
    assert!(!is_box_draw('\u{1FB3C}')); // wedge, not handled
    assert!(!is_box_draw('A'));
  }

  #[test]
  fn box_arms_weights() {
    // Light cross: every arm light. Heavy cross: every arm heavy.
    assert_eq!(box_arms(0x253C), Some([1, 1, 1, 1]));
    assert_eq!(box_arms(0x254B), Some([2, 2, 2, 2]));
    // Double horizontal/vertical and the double cross.
    assert_eq!(box_arms(0x2550), Some([0, 0, 3, 3]));
    assert_eq!(box_arms(0x2551), Some([3, 3, 0, 0]));
    assert_eq!(box_arms(0x256C), Some([3, 3, 3, 3]));
    // A light down-and-right corner is down + right only.
    assert_eq!(box_arms(0x250C), Some([0, 1, 0, 1]));
    // Dashes/arcs/diagonals are handled elsewhere, not here.
    assert_eq!(box_arms(0x2504), None);
    assert_eq!(box_arms(0x256D), None);
  }

  #[test]
  fn sextant_pattern_skips_half_blocks() {
    // The enumeration runs 1..=62 skipping the left-half (21) and right-half
    // (42) bit patterns, so the range endpoints map to 1 and 62.
    assert_eq!(sextant_pattern(0x1FB00), 1);
    assert_eq!(sextant_pattern(0x1FB3B), 62);
    // No codepoint in the range produces a skipped pattern.
    for cp in 0x1FB00..=0x1FB3B {
      let p = sextant_pattern(cp);
      assert!(p != 21 && p != 42 && p != 0 && p != 63);
    }
  }

  #[test]
  fn double_corner_closes_and_stays_hollow() {
    // U+2554 ╔ (double down-and-right). thin = 16/8 = 2, d = 3, centre = 8.
    let ink = render_glyph('\u{2554}', 16);
    // The outer corner where the two outer rails meet is inked...
    assert!(ink(5, 5), "double corner should close at the outer rails");
    // ...while the centre of the corner box stays hollow.
    assert!(!ink(8, 8), "the double corner's interior should be hollow");
    // The arms reach their edges (right arm at the top rail, down arm's left
    // rail near the bottom).
    assert!(ink(15, 5), "top rail should reach the right edge");
    assert!(ink(5, 15), "left rail should reach the bottom edge");
  }

  #[test]
  fn octant_table_and_geometry() {
    // The table is the full octant block, endpoints as transcribed from Kitty.
    assert_eq!(super::OCTANTS.len(), 230);
    assert_eq!(super::OCTANTS[0], 0x02);
    assert_eq!(*super::OCTANTS.last().unwrap(), 0xFE);
    // U+1CD00 → 0x02 = left column, row 1 only (rows are quarter-cells).
    let ink = render_glyph('\u{1CD00}', 16);
    assert!(ink(2, 5), "left column row 1 should be filled"); // row1 = [4,8)
    assert!(!ink(2, 1), "row 0 should be empty");
    assert!(!ink(10, 5), "right column should be empty");
  }

  #[test]
  fn blend_endpoints_hold_in_all_modes() {
    for mode in [
      AlphaBlending::Native,
      AlphaBlending::Linear,
      AlphaBlending::LinearCorrected,
    ] {
      // Zero coverage leaves the (black) background untouched.
      let mut buf = vec![0u8; 4];
      let mut c = Canvas {
        pixels: &mut buf,
        width:  1,
        height: 1,
        blend:  mode,
      };
      c.blend(0, 0, Rgb(255, 255, 255), 0);
      assert_eq!(buf[2], 0, "{mode:?}: zero coverage keeps the background");
      // Full coverage paints the foreground.
      let mut buf = vec![0u8; 4];
      let mut c = Canvas {
        pixels: &mut buf,
        width:  1,
        height: 1,
        blend:  mode,
      };
      c.blend(0, 0, Rgb(255, 255, 255), 255);
      assert!(
        buf[2] >= 254,
        "{mode:?}: full coverage paints the foreground"
      );
    }
  }

  #[test]
  fn diagonal_is_antialiased() {
    // A hard staircase would ink pixels fully or not at all; antialiasing
    // leaves edge pixels at partial coverage. Render U+2571 (╱) and confirm at
    // least one pixel is partially covered (white fg → blue byte in 1..255).
    let n = 24usize;
    let mut buf = vec![0u8; n * n * 4];
    let mut canvas = Canvas {
      pixels: &mut buf,
      width:  n,
      height: n,
      blend:  AlphaBlending::Native,
    };
    let m = CellMetrics {
      width:  u32::try_from(n).unwrap_or(u32::MAX),
      height: u32::try_from(n).unwrap_or(u32::MAX),
      ascent: 18,
      stroke: 2,
    };
    assert!(draw_box(
      &mut canvas,
      '\u{2571}',
      0,
      0,
      m,
      Rgb(255, 255, 255)
    ));
    let partial = buf.iter().step_by(4).any(|&b| b > 0 && b < 255);
    assert!(
      partial,
      "diagonal should have antialiased (partial) coverage"
    );
  }

  #[test]
  fn single_cross_and_corner_geometry() {
    // A light cross inks its centre; a top-left corner leaves the top-left
    // pixel blank (arms only run down and right from the centre).
    let cross = render_glyph('\u{253C}', 16); // ┼
    assert!(cross(8, 8), "cross centre should be inked");
    let corner = render_glyph('\u{250C}', 16); // ┌
    assert!(corner(8, 8), "corner centre should be inked");
    assert!(!corner(0, 0), "corner should not ink the opposite quadrant");
  }
}
