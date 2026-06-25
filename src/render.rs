//! Software renderer: compose the grid into an ARGB8888 buffer.
//!
//! The target is a `wl_shm` buffer in `Argb8888`, which on little-endian is
//! `[B, G, R, A]` per pixel. Rendering is two passes per frame - backgrounds
//! then glyphs - so a wide glyph that overflows its cell is not clipped by the
//! neighbouring cell's background fill.

use std::num::NonZeroU16;

use crate::font::{CellMetrics, Fonts, GlyphData, Style};
use crate::grid::{Cell, CursorShape, Flags, Grid, Underline};
use crate::theme::{Plane, Rgb, Theme};

/// A mutable view over a BGRA pixel buffer.
struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: usize,
    height: usize,
}

impl Canvas<'_> {
    fn index(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return None;
        }
        Some((y as usize * self.width + x as usize) * 4)
    }

    fn fill_rect(&mut self, x0: i32, y0: i32, w: u32, h: u32, c: Rgb) {
        self.fill_rect_a(x0, y0, w, h, c, 0xff);
    }

    /// Fill a rectangle with colour `c` at opacity `alpha`. The shm buffer is
    /// premultiplied ARGB, so a translucent fill stores `rgb * alpha`.
    fn fill_rect_a(&mut self, x0: i32, y0: i32, w: u32, h: u32, c: Rgb, alpha: u8) {
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
            let row =
                &mut self.pixels[(y * self.width + x_start) * 4..(y * self.width + x_end) * 4];
            for px in row.chunks_exact_mut(4) {
                px.copy_from_slice(&bytes);
            }
        }
    }

    /// Alpha-blend `fg` over the existing pixel with coverage `a`.
    fn blend(&mut self, x: i32, y: i32, fg: Rgb, a: u8) {
        let Some(i) = self.index(x, y) else { return };
        let (a, inv) = (u32::from(a), u32::from(255 - a));
        let mix = |src: u8, dst: u8| ((u32::from(src) * a + u32::from(dst) * inv) / 255) as u8;
        self.pixels[i] = mix(fg.2, self.pixels[i]);
        self.pixels[i + 1] = mix(fg.1, self.pixels[i + 1]);
        self.pixels[i + 2] = mix(fg.0, self.pixels[i + 2]);
        self.pixels[i + 3] = 0xff;
    }

    /// Set a single opaque pixel.
    fn put(&mut self, x: i32, y: i32, c: Rgb) {
        if let Some(i) = self.index(x, y) {
            self.pixels[i] = c.2;
            self.pixels[i + 1] = c.1;
            self.pixels[i + 2] = c.0;
            self.pixels[i + 3] = 0xff;
        }
    }

    fn hline(&mut self, x0: i32, y: i32, w: u32, c: Rgb) {
        self.fill_rect(x0, y, w, 1, c);
    }

    /// Composite one pre-multiplied BGRA source pixel over the destination.
    fn over(&mut self, x: i32, y: i32, src: &[u8]) {
        let Some(i) = self.index(x, y) else { return };
        let inv = u32::from(255 - src[3]);
        let comp = |s: u8, dst: u8| (u32::from(s) + u32::from(dst) * inv / 255).min(255) as u8;
        self.pixels[i] = comp(src[0], self.pixels[i]);
        self.pixels[i + 1] = comp(src[1], self.pixels[i + 1]);
        self.pixels[i + 2] = comp(src[2], self.pixels[i + 2]);
        self.pixels[i + 3] = 0xff;
    }
}

/// Per-frame constants shared by every row: the colour scheme, focus, and the
/// current blink phase.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub theme: &'a Theme,
    pub focused: bool,
    pub blink_on: bool,
    /// Hyperlink currently under the pointer; its cells get a hover underline.
    pub hovered_link: Option<NonZeroU16>,
}

#[derive(Debug)]
pub struct Renderer {
    fonts: Fonts,
    /// Inner padding `(x, y)` in pixels between the window edge and the grid.
    pad: (i32, i32),
}

impl Renderer {
    pub fn new(fonts: Fonts) -> Self {
        Self { fonts, pad: (0, 0) }
    }

    pub fn metrics(&self) -> CellMetrics {
        self.fonts.metrics()
    }

    pub fn set_padding(&mut self, pad_x: u32, pad_y: u32) {
        self.pad = (pad_x as i32, pad_y as i32);
    }

    /// Rebuild the font set at a new size (font-resize bindings).
    pub fn set_font(&mut self, family: &str, size_px: u32) -> Result<(), crate::font::FontError> {
        self.fonts = Fonts::new(family, size_px)?;
        Ok(())
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
        };
        canvas.fill_rect_a(0, 0, width as u32, height as u32, theme.bg, theme.alpha);
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
        let (theme, focused, blink_on) = (frame.theme, frame.focused, frame.blink_on);
        let (width, height) = dims;
        let mut canvas = Canvas {
            pixels,
            width,
            height,
        };
        let m = self.fonts.metrics();
        let (pad_x, pad_y) = self.pad;
        let cols = grid.cols();
        let row_top = pad_y + y as i32 * m.height as i32;
        canvas.fill_rect_a(0, row_top, width as u32, m.height, theme.bg, theme.alpha);

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

        for (x, cell) in cells.iter().take(cols).enumerate() {
            if cell.flags.contains(Flags::WIDE_CONT) {
                continue;
            }
            if cell.flags.contains(Flags::BLINK) && !blink_on {
                continue;
            }
            let (fg, _) = cell_colors(cell, theme);
            let origin_x = pad_x + x as i32 * m.width as i32;
            if is_braille(cell.c) {
                // Drawn directly so the dots are crisp and fill the cell, the
                // way tools like btop expect, rather than however the fallback
                // font happens to size its braille glyphs.
                draw_braille(&mut canvas, cell.c, origin_x, row_top, m, fg);
            } else if cell.c != ' ' {
                self.draw_glyph(&mut canvas, cell.c, cell_style(cell), origin_x, row_top, fg);
            }
            // Stack any combining marks over the base glyph; their own bearings
            // position them (no shaper, so placement is the font's default).
            if let Some(marks) = &cell.combining {
                for mark in marks.chars() {
                    self.draw_glyph(&mut canvas, mark, cell_style(cell), origin_x, row_top, fg);
                }
            }
            draw_decorations(&mut canvas, cell, theme, origin_x, row_top, m, fg);
            // Underline an OSC 8 hyperlink while the pointer hovers over it.
            if cell.link.is_some() && cell.link == frame.hovered_link {
                canvas.hline(origin_x, row_top + m.height as i32 - 2, m.width, fg);
            }
        }

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
        };
        let m = self.fonts.metrics();
        let (pad_x, pad_y) = self.pad;
        let row_top = pad_y + row as i32 * m.height as i32;
        canvas.fill_rect(0, row_top, width as u32, m.height, theme.search_bar_bg);
        let style = Style {
            bold: false,
            italic: false,
        };
        let mut x = pad_x;
        for c in text.chars() {
            if x as usize + m.width as usize > width {
                break;
            }
            if c != ' ' {
                self.draw_glyph(&mut canvas, c, style, x, row_top, theme.fg);
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
        };
        let m = self.fonts.metrics();
        let (pad_x, pad_y) = self.pad;
        let row_top = pad_y + row as i32 * m.height as i32;
        let style = Style {
            bold: true,
            italic: false,
        };
        let mut x = pad_x + col as i32 * m.width as i32;
        for c in text.chars() {
            if x as usize + m.width as usize > width {
                break;
            }
            canvas.fill_rect(x, row_top, m.width, m.height, theme.current_match_bg);
            if c != ' ' {
                self.draw_glyph(&mut canvas, c, style, x, row_top, theme.bg);
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
        };
        let m = self.fonts.metrics();
        let (pad_x, pad_y) = self.pad;
        let row_top = pad_y + row as i32 * m.height as i32;
        let style = Style {
            bold: false,
            italic: false,
        };
        let mut x = pad_x + start_col as i32 * m.width as i32;
        for c in text.chars() {
            if x < 0 || x as usize + m.width as usize > width {
                break;
            }
            canvas.fill_rect(x, row_top, m.width, m.height, theme.selection_bg);
            if c != ' ' {
                self.draw_glyph(&mut canvas, c, style, x, row_top, theme.fg);
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
                    self.draw_glyph(canvas, cell.c, cell_style(cell), x0, top, bg);
                }
            }
            CursorShape::Underline => {
                canvas.fill_rect(x0, top + m.height as i32 - 2, m.width, 2, color);
            }
            CursorShape::Beam => canvas.fill_rect(x0, top, 2, m.height, color),
        }
    }

    fn draw_glyph(
        &mut self,
        canvas: &mut Canvas,
        c: char,
        style: Style,
        origin_x: i32,
        cell_top: i32,
        fg: Rgb,
    ) {
        let m = self.fonts.metrics();
        let glyph = match self.fonts.glyph(c, style) {
            Ok(glyph) => glyph,
            Err(err) => {
                tracing::debug!("glyph {c:?}: {err}");
                return;
            }
        };
        let (gw, gh) = (glyph.width as i32, glyph.height as i32);
        match &glyph.data {
            GlyphData::Mask(mask) => {
                let baseline = cell_top + m.ascent as i32;
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
            }
            // Colour glyphs (emoji) come from a fixed strike at native size;
            // scale them to the line height with nearest-neighbour sampling.
            GlyphData::Color(bgra) if gh > 0 => {
                let scale = m.height as f32 / gh as f32;
                let target_w = (gw as f32 * scale) as i32;
                for ty in 0..m.height as i32 {
                    let sy = ((ty as f32 / scale) as i32).min(gh - 1);
                    for tx in 0..target_w {
                        let sx = ((tx as f32 / scale) as i32).min(gw - 1);
                        let i = ((sy * gw + sx) * 4) as usize;
                        canvas.over(origin_x + tx, cell_top + ty, &bgra[i..i + 4]);
                    }
                }
            }
            GlyphData::Color(_) => {}
        }
    }
}

fn cell_style(cell: &Cell) -> Style {
    Style {
        bold: cell.flags.contains(Flags::BOLD),
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
        std::mem::swap(&mut fg, &mut bg);
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
/// column rows 0-2, bits 3-5 the right column rows 0-2, bits 6-7 the bottom row.
fn draw_braille(canvas: &mut Canvas, c: char, x0: i32, top: i32, m: CellMetrics, fg: Rgb) {
    let (w, xs, ys) = braille_geometry(m.width as i32, m.height as i32);
    let sym = ((c as u32) - 0x2800) as u8;
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
    for (mask, col, row) in DOTS {
        if sym & mask != 0 {
            canvas.fill_rect(x0 + xs[col], top + ys[row], w, w, fg);
        }
    }
}

/// Draw underline, strikethrough, and overline for one cell.
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
        crate::grid::Color::Default => fg,
        other => theme.resolve(other, Plane::Fg, false),
    };
    match cell.underline {
        Underline::None => {}
        Underline::Single => canvas.hline(x0, uy, w, uc),
        Underline::Double => {
            canvas.hline(x0, uy, w, uc);
            canvas.hline(x0, (uy - 2).max(top), w, uc);
        }
        Underline::Curly => {
            for dx in 0..w as i32 {
                let wobble = if (dx / 2) % 2 == 0 { 0 } else { 1 };
                canvas.put(x0 + dx, uy - wobble, uc);
            }
        }
        Underline::Dotted => {
            for dx in (0..w as i32).step_by(2) {
                canvas.put(x0 + dx, uy, uc);
            }
        }
        Underline::Dashed => {
            for dx in 0..w as i32 {
                if (dx / 3) % 2 == 0 {
                    canvas.put(x0 + dx, uy, uc);
                }
            }
        }
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
    use super::braille_geometry;

    // Pinned to foot box-drawing.c draw_braille output (cross-checked numerically
    // identical across cell sizes 4..30 x 6..48); guards against drift.
    #[test]
    fn braille_geometry_matches_foot() {
        assert_eq!(braille_geometry(8, 18), (2, [1, 5], [2, 6, 10, 14]));
        assert_eq!(braille_geometry(10, 20), (2, [1, 6], [1, 6, 11, 16]));
        assert_eq!(braille_geometry(12, 27), (3, [1, 8], [1, 8, 15, 22]));
        assert_eq!(braille_geometry(7, 15), (1, [1, 4], [2, 5, 8, 11]));
    }
}
