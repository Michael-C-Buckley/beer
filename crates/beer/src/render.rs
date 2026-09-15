//! Software renderer: compose the grid into an ARGB8888 buffer.

//! The target is a `wl_shm` buffer in `Argb8888`, which on little-endian is
//! `[B, G, R, A]` per pixel. Rendering is two passes per frame: backgrounds
//! then glyphs, so a wide glyph that overflows its cell is not clipped by the
//! neighbouring cell background fill.

use std::{
  collections::{HashMap, HashSet},
  mem,
  num::NonZeroU16,
};

use beer_protocols::{
  graphics::PLACEHOLDER,
  text_size::{HAlign, VAlign},
};

use crate::{
  config::{AlphaBlending, Subpixel},
  font::{CellMetrics, Fonts, Style},
  graphics::Graphics,
  grid::{Cell, CursorShape, Flags, Grid},
  theme::{Plane, Rgb, Theme},
};

mod canvas;
mod decorations;
mod geometry;
mod glyph;
mod images;

use canvas::Canvas;
use decorations::draw_decorations;
use geometry::{draw_braille, draw_geometric, is_braille, is_geometric};
use glyph::{blit_glyph, blit_glyph_clipped};
use images::{draw_image_cells, draw_placeholders};
/// Per-frame constants shared by every row: the colour scheme, focus, and the
/// current blink phase.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
  pub theme:        &'a Theme,
  pub focused:      bool,
  pub blink_on:     bool,
  pub rapid_on:     bool,
  /// Hyperlink currently under the pointer; its cells get a hover underline.
  pub hovered_link: Option<NonZeroU16>,
  /// The graphics engine, source of image pixels and placement geometry.
  pub images:       &'a Graphics,
}

/// Glyphs shaped for one cell of a run: the shaping face, the run's style, and
/// each glyph's index and pixel offset from the cell origin.
struct ShapedCell {
  face_idx: usize,
  style:    Style,
  glyphs:   Vec<(u32, i32, i32)>,
}

/// The shaping plan for one row: which cells draw shaped glyphs, and which are
/// covered by a ligature to their left and so draw nothing.
#[derive(Default)]
struct ShapePlan {
  shaped:  HashMap<usize, ShapedCell>,
  covered: HashSet<usize>,
}

/// Whether a cell takes part in run shaping. Cells drawn by another path
/// (combining clusters, braille, box drawing, images, sized blocks) or hidden
/// this blink phase are excluded so a run never crosses them.
fn shapeable(cell: &Cell, blink_on: bool, rapid_on: bool) -> bool {
  if cell.flags.contains(Flags::WIDE_CONT) || cell.sized.is_some() {
    return false;
  }
  if cell.combining.is_some() || cell.c == PLACEHOLDER {
    return false;
  }
  if blink_hidden(cell, blink_on, rapid_on) {
    return false;
  }
  !is_braille(cell.c) && !is_geometric(cell.c)
}

const fn blink_hidden(cell: &Cell, blink_on: bool, rapid_on: bool) -> bool {
  (cell.flags.contains(Flags::BLINK) && !blink_on)
    || (cell.flags.contains(Flags::RAPID_BLINK) && !rapid_on)
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
    let (theme, focused, blink_on, rapid_on) =
      (frame.theme, frame.focused, frame.blink_on, frame.rapid_on);
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

    // Shape the row's runs up front so the draw loop can place ligature glyphs
    // and skip the cells they absorb.
    let plan = if self.fonts.ligatures() {
      self.plan_shaping(cells, cols, blink_on, rapid_on)
    } else {
      ShapePlan::default()
    };

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
      if blink_hidden(cell, blink_on, rapid_on) {
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
      } else if is_geometric(cell.c)
        && draw_geometric(&mut canvas, cell.c, origin_x, row_top, m, fg)
      {
        // Box drawing, block elements, and sextants are drawn geometrically so
        // they fill the cell exactly and tile seamlessly - fonts leave seams.
      } else if let Some(sc) = plan.shaped.get(&x) {
        // Glyphs the run shaper placed for this cell (including ligatures that
        // spill into the covered cells to the right).
        let sub = self.sub_mode(opaque_bg);
        for &(gid, gx, gy) in &sc.glyphs {
          if let Ok(glyph) =
            self.fonts.glyph_indexed(sc.face_idx, gid, sc.style)
          {
            blit_glyph(
              &mut canvas,
              glyph,
              m,
              origin_x + gx,
              row_top,
              gy,
              fg,
              sub,
            );
          }
        }
      } else if plan.covered.contains(&x) {
        // Absorbed by a ligature that a cell to the left already drew.
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

  /// Shape the row's text into a [`ShapePlan`]. Consecutive shapeable cells of
  /// one style form a run shaped together; each glyph is bound to the cell of
  /// its cluster, and cells a ligature absorbed are recorded as covered so the
  /// draw loop skips them. Cells the shaping face does not cover fall through
  /// to the per-cell path, which does its own fontconfig fallback.
  fn plan_shaping(
    &mut self,
    cells: &[Cell],
    cols: usize,
    blink_on: bool,
    rapid_on: bool,
  ) -> ShapePlan {
    let mut plan = ShapePlan::default();
    let cols = cols.min(cells.len());
    let mut x = 0;
    while x < cols {
      if !shapeable(&cells[x], blink_on, rapid_on) {
        x += 1;
        continue;
      }
      let style = cell_style(&cells[x]);
      let mut text = String::new();
      let mut offsets: Vec<(usize, usize)> = Vec::new();
      while x < cols
        && shapeable(&cells[x], blink_on, rapid_on)
        && cell_style(&cells[x]) == style
      {
        offsets.push((text.len(), x));
        text.push(cells[x].c);
        x += 1;
      }
      // A lone cell cannot ligate, so leave it to the cheaper per-cell path.
      if offsets.len() < 2 {
        continue;
      }
      let Some(run) = self.fonts.shape_run(&text, style) else {
        continue;
      };
      let mut present: HashSet<usize> = HashSet::new();
      let mut by_cluster: HashMap<usize, Vec<(u32, i32, i32)>> = HashMap::new();
      for glyph in &run.glyphs {
        let cluster = glyph.cluster as usize;
        present.insert(cluster);
        if glyph.gid != 0 {
          by_cluster
            .entry(cluster)
            .or_default()
            .push((glyph.gid, glyph.x, glyph.y));
        }
      }
      for &(offset, cell_x) in &offsets {
        if let Some(glyphs) = by_cluster.remove(&offset) {
          plan.shaped.insert(cell_x, ShapedCell {
            face_idx: run.face_idx,
            style,
            glyphs,
          });
        } else if !present.contains(&offset) {
          // Merged into a ligature that a cell to the left draws.
          plan.covered.insert(cell_x);
        }
        // A present-but-.notdef cluster falls through to the per-cell path.
      }
    }
    plan
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

#[cfg(test)] mod tests;
