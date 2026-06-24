//! Software renderer: compose the grid into an ARGB8888 buffer.
//!
//! The target is a `wl_shm` buffer in `Argb8888`, which on little-endian is
//! `[B, G, R, A]` per pixel. Rendering is two passes per frame - backgrounds
//! then glyphs - so a wide glyph that overflows its cell is not clipped by the
//! neighbouring cell's background fill.

use crate::font::{CellMetrics, Fonts, GlyphData, Style};
use crate::grid::{Cell, Color, CursorShape, Flags, Grid, Underline};

/// Foreground/background used for `Color::Default`.
const DEFAULT_FG: Rgb = Rgb(0xc5, 0xc8, 0xc6);
const DEFAULT_BG: Rgb = Rgb(0x18, 0x18, 0x18);

#[derive(Clone, Copy, PartialEq, Eq)]
struct Rgb(u8, u8, u8);

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

    /// Fill the whole buffer with one colour (fast path, no per-pixel bounds
    /// checks).
    fn clear(&mut self, c: Rgb) {
        let bytes = [c.2, c.1, c.0, 0xff];
        for px in self.pixels.chunks_exact_mut(4) {
            px.copy_from_slice(&bytes);
        }
    }

    fn fill_rect(&mut self, x0: i32, y0: i32, w: u32, h: u32, c: Rgb) {
        let x_start = x0.max(0) as usize;
        let x_end = ((x0 + w as i32).max(0) as usize).min(self.width);
        let y_start = y0.max(0) as usize;
        let y_end = ((y0 + h as i32).max(0) as usize).min(self.height);
        if x_start >= x_end {
            return;
        }
        let bytes = [c.2, c.1, c.0, 0xff];
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

#[derive(Debug)]
pub struct Renderer {
    fonts: Fonts,
}

impl Renderer {
    pub fn new(fonts: Fonts) -> Self {
        Self { fonts }
    }

    pub fn metrics(&self) -> CellMetrics {
        self.fonts.metrics()
    }

    /// Compose `grid` into `pixels` (BGRA, `width`×`height` px). `focused`
    /// selects a solid or hollow cursor.
    pub fn render(
        &mut self,
        grid: &Grid,
        pixels: &mut [u8],
        width: usize,
        height: usize,
        focused: bool,
    ) {
        let mut canvas = Canvas {
            pixels,
            width,
            height,
        };
        canvas.clear(DEFAULT_BG);

        let m = self.fonts.metrics();

        // Cell backgrounds: only paint cells that differ from the cleared
        // default - most of a screen is default background.
        for y in 0..grid.rows() {
            for x in 0..grid.cols() {
                let (_, bg) = cell_colors(grid.cell(x, y));
                if bg != DEFAULT_BG {
                    let (px, py) = (x as i32 * m.width as i32, y as i32 * m.height as i32);
                    canvas.fill_rect(px, py, m.width, m.height, bg);
                }
            }
        }

        for y in 0..grid.rows() {
            for x in 0..grid.cols() {
                let cell = grid.cell(x, y);
                if cell.flags.contains(Flags::WIDE_CONT) {
                    continue;
                }
                let (fg, _) = cell_colors(cell);
                let origin_x = x as i32 * m.width as i32;
                let cell_top = y as i32 * m.height as i32;
                if cell.c != ' ' {
                    self.draw_glyph(
                        &mut canvas,
                        cell.c,
                        cell_style(cell),
                        origin_x,
                        cell_top,
                        fg,
                    );
                }
                draw_decorations(&mut canvas, cell, origin_x, cell_top, m, fg);
            }
        }

        self.draw_cursor(&mut canvas, grid, m, focused);
    }

    /// Draw the cursor: a solid block/underline/beam when focused, a hollow
    /// outline when not.
    fn draw_cursor(&mut self, canvas: &mut Canvas, grid: &Grid, m: CellMetrics, focused: bool) {
        if !grid.cursor_visible() {
            return;
        }
        let (cx, cy) = grid.cursor();
        let x0 = cx as i32 * m.width as i32;
        let top = cy as i32 * m.height as i32;
        let color = grid
            .cursor_color()
            .map_or(DEFAULT_FG, |(r, g, b)| Rgb(r, g, b));

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
                    let (_, bg) = cell_colors(cell);
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
fn cell_colors(cell: &Cell) -> (Rgb, Rgb) {
    let bold = cell.flags.contains(Flags::BOLD);
    let mut fg = resolve(cell.fg, DEFAULT_FG, bold);
    let mut bg = resolve(cell.bg, DEFAULT_BG, false);
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
fn draw_decorations(canvas: &mut Canvas, cell: &Cell, x0: i32, top: i32, m: CellMetrics, fg: Rgb) {
    let w = m.width;
    let baseline = top + m.ascent as i32;
    let uy = (baseline + 1).min(top + m.height as i32 - 1);
    let uc = resolve(cell.underline_color, fg, false);
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

fn resolve(color: Color, default: Rgb, bold: bool) -> Rgb {
    match color {
        Color::Default => default,
        Color::Indexed(i) => ansi256(if bold && i < 8 { i + 8 } else { i }),
        Color::Rgb(r, g, b) => Rgb(r, g, b),
    }
}

/// The xterm 256-colour palette: 16 base, a 6×6×6 cube, then 24 greys.
fn ansi256(i: u8) -> Rgb {
    const BASE: [Rgb; 16] = [
        Rgb(0x00, 0x00, 0x00),
        Rgb(0xcd, 0x00, 0x00),
        Rgb(0x00, 0xcd, 0x00),
        Rgb(0xcd, 0xcd, 0x00),
        Rgb(0x00, 0x00, 0xee),
        Rgb(0xcd, 0x00, 0xcd),
        Rgb(0x00, 0xcd, 0xcd),
        Rgb(0xe5, 0xe5, 0xe5),
        Rgb(0x7f, 0x7f, 0x7f),
        Rgb(0xff, 0x00, 0x00),
        Rgb(0x00, 0xff, 0x00),
        Rgb(0xff, 0xff, 0x00),
        Rgb(0x5c, 0x5c, 0xff),
        Rgb(0xff, 0x00, 0xff),
        Rgb(0x00, 0xff, 0xff),
        Rgb(0xff, 0xff, 0xff),
    ];
    match i {
        0..=15 => BASE[i as usize],
        16..=231 => {
            let i = i - 16;
            Rgb(cube(i / 36), cube((i / 6) % 6), cube(i % 6))
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            Rgb(v, v, v)
        }
    }
}

fn cube(step: u8) -> u8 {
    if step == 0 { 0 } else { 55 + 40 * step }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_cube_and_grey() {
        assert_eq!(ansi256(16).0, 0); // cube origin is black
        let white = ansi256(231);
        assert_eq!((white.0, white.1, white.2), (255, 255, 255));
        assert_eq!(ansi256(232).0, 8); // first grey step
    }
}
