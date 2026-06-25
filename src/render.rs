//! Software renderer: compose the grid into an ARGB8888 buffer.
//!
//! The target is a `wl_shm` buffer in `Argb8888`, which on little-endian is
//! `[B, G, R, A]` per pixel. Rendering is two passes per frame - backgrounds
//! then glyphs - so a wide glyph that overflows its cell is not clipped by the
//! neighbouring cell's background fill.

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
            if cell.c != ' ' {
                self.draw_glyph(&mut canvas, cell.c, cell_style(cell), origin_x, row_top, fg);
            }
            draw_decorations(&mut canvas, cell, theme, origin_x, row_top, m, fg);
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
