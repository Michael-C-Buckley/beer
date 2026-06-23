//! Software renderer: compose the grid into an ARGB8888 buffer.
//!
//! The target is a `wl_shm` buffer in `Argb8888`, which on little-endian is
//! `[B, G, R, A]` per pixel. Rendering is two passes per frame - backgrounds
//! then glyphs - so a wide glyph that overflows its cell is not clipped by the
//! neighbouring cell's background fill.

use crate::font::{CellMetrics, Fonts, GlyphData, Style};
use crate::grid::{Color, Flags, Grid};

/// Foreground/background used for `Color::Default`.
const DEFAULT_FG: Rgb = Rgb(0xc5, 0xc8, 0xc6);
const DEFAULT_BG: Rgb = Rgb(0x18, 0x18, 0x18);

#[derive(Clone, Copy)]
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

    fn fill_rect(&mut self, x0: i32, y0: i32, w: u32, h: u32, c: Rgb) {
        for dy in 0..h as i32 {
            for dx in 0..w as i32 {
                if let Some(i) = self.index(x0 + dx, y0 + dy) {
                    self.pixels[i] = c.2;
                    self.pixels[i + 1] = c.1;
                    self.pixels[i + 2] = c.0;
                    self.pixels[i + 3] = 0xff;
                }
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

    /// Compose `grid` into `pixels` (BGRA, `width`×`height` px). The cursor cell
    /// is drawn reversed.
    pub fn render(&mut self, grid: &Grid, pixels: &mut [u8], width: usize, height: usize) {
        let mut canvas = Canvas {
            pixels,
            width,
            height,
        };
        canvas.fill_rect(0, 0, width as u32, height as u32, DEFAULT_BG);

        let m = self.fonts.metrics();
        let cursor = grid.cursor();

        for y in 0..grid.rows() {
            for x in 0..grid.cols() {
                let (_, bg) = cell_colors(grid.cell(x, y), (x, y) == cursor);
                let (px, py) = (x as i32 * m.width as i32, y as i32 * m.height as i32);
                canvas.fill_rect(px, py, m.width, m.height, bg);
            }
        }

        for y in 0..grid.rows() {
            for x in 0..grid.cols() {
                let cell = grid.cell(x, y);
                if cell.flags.contains(Flags::WIDE_CONT) || cell.c == ' ' {
                    continue;
                }
                let (fg, _) = cell_colors(cell, (x, y) == cursor);
                let style = Style {
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                };
                let origin_x = x as i32 * m.width as i32;
                let baseline = y as i32 * m.height as i32 + m.ascent as i32;
                self.draw_glyph(&mut canvas, cell.c, style, origin_x, baseline, fg);
            }
        }
    }

    fn draw_glyph(
        &mut self,
        canvas: &mut Canvas,
        c: char,
        style: Style,
        origin_x: i32,
        baseline: i32,
        fg: Rgb,
    ) {
        let glyph = match self.fonts.glyph(c, style) {
            Ok(glyph) => glyph,
            Err(err) => {
                tracing::debug!("glyph {c:?}: {err}");
                return;
            }
        };
        let (left, top, w, h) = (
            glyph.left,
            glyph.top,
            glyph.width as i32,
            glyph.height as i32,
        );
        match &glyph.data {
            GlyphData::Mask(mask) => {
                for gy in 0..h {
                    for gx in 0..w {
                        let a = mask[(gy * w + gx) as usize];
                        if a != 0 {
                            canvas.blend(origin_x + left + gx, baseline - top + gy, fg, a);
                        }
                    }
                }
            }
            GlyphData::Color(bgra) => {
                for gy in 0..h {
                    for gx in 0..w {
                        let i = ((gy * w + gx) * 4) as usize;
                        canvas.over(origin_x + left + gx, baseline - top + gy, &bgra[i..i + 4]);
                    }
                }
            }
        }
    }
}

/// Resolve a cell's (foreground, background) RGB, applying reverse video,
/// bold-as-bright for the foreground, and hidden.
fn cell_colors(cell: &crate::grid::Cell, cursor: bool) -> (Rgb, Rgb) {
    let bold = cell.flags.contains(Flags::BOLD);
    let mut fg = resolve(cell.fg, DEFAULT_FG, bold);
    let mut bg = resolve(cell.bg, DEFAULT_BG, false);
    if cell.flags.contains(Flags::REVERSE) ^ cursor {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.flags.contains(Flags::HIDDEN) {
        fg = bg;
    }
    (fg, bg)
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
