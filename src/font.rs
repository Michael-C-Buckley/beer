//! Font discovery, rasterization, and glyph caching.
//!
//! fontconfig resolves family names and performs per-codepoint fallback;
//! FreeType rasterizes each glyph to an 8-bit coverage mask or, for colour
//! fonts, a pre-multiplied BGRA bitmap. Layout is fixed-cell, so a glyph's own
//! advance is never consulted - only the [`CellMetrics`] taken from the primary
//! face. C interop goes through the `freetype`/`fontconfig` safe wrappers; the
//! sole `unsafe` is reading a face's fixed-strike array (see `nearest_strike`).

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use fontconfig::{CharSet, Fontconfig, Pattern};
use freetype::bitmap::PixelMode;
use freetype::face::{LoadFlag, StyleFlag};
use freetype::{Face, Library, Matrix, Vector};
use lru::LruCache;
use thiserror::Error;

/// Upper bound on cached glyphs; the working set of a terminal is far smaller,
/// but this caps memory under adversarial all-of-Unicode output.
const GLYPH_CACHE_CAP: usize = 4096;

#[derive(Debug, Error)]
pub enum FontError {
    #[error("FreeType: {0}")]
    FreeType(#[from] freetype::Error),
    #[error("could not initialize fontconfig")]
    FontconfigInit,
    #[error("fontconfig: {0}")]
    Fontconfig(#[from] fontconfig::FontconfigError),
    #[error("no font matched family {0:?}")]
    NoFamily(String),
    #[error("font {0:?} reports no size metrics")]
    NoMetrics(String),
}

/// Bold/italic selection, used both to pick a face and to key the glyph cache.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
}

impl Style {
    /// Dense index in `0..4` for array storage.
    fn index(self) -> usize {
        usize::from(self.bold) | (usize::from(self.italic) << 1)
    }

    fn fontconfig_style(self) -> &'static str {
        match (self.bold, self.italic) {
            (false, false) => "Regular",
            (true, false) => "Bold",
            (false, true) => "Italic",
            (true, true) => "Bold Italic",
        }
    }
}

/// Fixed cell geometry in pixels, derived from the primary face.
#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
    /// Baseline offset from the top of the cell.
    pub ascent: u32,
}

/// A rasterized glyph: its bitmap plus the offsets to place it on the baseline.
#[derive(Clone, Debug)]
pub struct Glyph {
    /// Horizontal offset from the pen position to the bitmap's left edge.
    pub left: i32,
    /// Vertical offset from the baseline up to the bitmap's top edge.
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub data: GlyphData,
}

/// Glyph pixel data. A `Mask` is tinted with the cell's foreground colour; a
/// `Color` bitmap (emoji) is composited directly.
#[derive(Clone, Debug)]
pub enum GlyphData {
    /// One coverage byte per pixel.
    Mask(Vec<u8>),
    /// Pre-multiplied BGRA, four bytes per pixel.
    Color(Vec<u8>),
}

/// The font set for one terminal: a primary family with lazily-loaded
/// bold/italic variants and per-codepoint fallback faces, plus a glyph cache.
pub struct Fonts {
    library: Library,
    fontconfig: Fontconfig,
    family: String,
    size_px: u32,
    metrics: CellMetrics,
    /// All loaded faces; indices into this vector are stable.
    faces: Vec<Face>,
    /// Index of each style variant, by [`Style::index`]; filled on demand.
    styled: [Option<usize>; 4],
    /// Fallback faces resolved by coverage, deduplicated by file path.
    fallbacks: HashMap<PathBuf, usize>,
    cache: LruCache<(char, usize), Glyph>,
}

impl fmt::Debug for Fonts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fonts")
            .field("family", &self.family)
            .field("size_px", &self.size_px)
            .field("metrics", &self.metrics)
            .field("faces", &self.faces.len())
            .field("cached", &self.cache.len())
            .finish()
    }
}

impl Fonts {
    /// Resolve `family` at `size_px` and compute the cell metrics.
    pub fn new(family: &str, size_px: u32) -> Result<Self, FontError> {
        let library = Library::init()?;
        let fontconfig = Fontconfig::new().ok_or(FontError::FontconfigInit)?;

        let regular = resolve_face(&library, &fontconfig, family, Style::default(), size_px)?;
        let metrics = cell_metrics(&regular, family)?;

        Ok(Self {
            library,
            fontconfig,
            family: family.to_owned(),
            size_px,
            metrics,
            faces: vec![regular],
            styled: [Some(0), None, None, None],
            fallbacks: HashMap::new(),
            cache: LruCache::new(NonZeroUsize::new(GLYPH_CACHE_CAP).expect("cap is nonzero")),
        })
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Return the rasterized glyph for `c` in `style`, rasterizing and caching
    /// it on first use.
    pub fn glyph(&mut self, c: char, style: Style) -> Result<&Glyph, FontError> {
        let key = (c, style.index());
        if self.cache.get(&key).is_none() {
            let idx = self.face_for(c, style)?;
            let face = &self.faces[idx];
            // Synthesize bold/italic only when the resolved face lacks the real
            // variant (most monospace families ship both).
            let flags = face.style_flags();
            let synth_bold = style.bold && !flags.contains(StyleFlag::BOLD);
            let synth_italic = style.italic && !flags.contains(StyleFlag::ITALIC);
            let glyph = rasterize(face, c, synth_bold, synth_italic)?;
            self.cache.put(key, glyph);
        }
        Ok(self.cache.get(&key).expect("glyph was just inserted"))
    }

    /// Pick the face that should render `c`: the requested style if it has the
    /// glyph, then regular, then known fallbacks, then a fresh fontconfig
    /// coverage match. Falls back to the styled face (rendering `.notdef`).
    fn face_for(&mut self, c: char, style: Style) -> Result<usize, FontError> {
        let styled = self.styled_face(style)?;
        if face_has_glyph(&self.faces[styled], c) {
            return Ok(styled);
        }
        if let Some(regular) = self.styled[0]
            && regular != styled
            && face_has_glyph(&self.faces[regular], c)
        {
            return Ok(regular);
        }
        for &idx in self.fallbacks.values() {
            if face_has_glyph(&self.faces[idx], c) {
                return Ok(idx);
            }
        }
        Ok(self.load_fallback(c)?.unwrap_or(styled))
    }

    /// Lazily load the face for `style`, caching regular's index if the variant
    /// cannot be resolved so the lookup is not retried per glyph.
    fn styled_face(&mut self, style: Style) -> Result<usize, FontError> {
        if let Some(idx) = self.styled[style.index()] {
            return Ok(idx);
        }
        let regular = self.styled[0].expect("regular face is loaded at construction");
        let idx = match resolve_face(
            &self.library,
            &self.fontconfig,
            &self.family,
            style,
            self.size_px,
        ) {
            Ok(face) => {
                self.faces.push(face);
                self.faces.len() - 1
            }
            Err(_) => regular,
        };
        self.styled[style.index()] = Some(idx);
        Ok(idx)
    }

    /// Ask fontconfig for a font covering `c`, load it, and remember it.
    fn load_fallback(&mut self, c: char) -> Result<Option<usize>, FontError> {
        let mut charset = CharSet::new(&self.fontconfig)?;
        charset.add_char(c)?;
        let mut pattern = Pattern::new(&self.fontconfig)?;
        pattern.add_string(c"family", c"monospace")?;
        pattern.add_charset(charset)?;
        let matched = pattern.font_match()?;

        let path = PathBuf::from(matched.filename()?);
        if let Some(&idx) = self.fallbacks.get(&path) {
            return Ok(Some(idx));
        }
        let index = matched.face_index().unwrap_or(0) as isize;
        let face = self.library.new_face(&path, index)?;
        if size_face(&face, self.size_px).is_err() {
            return Ok(None);
        }
        self.faces.push(face);
        let idx = self.faces.len() - 1;
        self.fallbacks.insert(path, idx);
        Ok(Some(idx))
    }
}

fn face_has_glyph(face: &Face, c: char) -> bool {
    face.get_char_index(c as usize).is_some_and(|g| g != 0)
}

fn resolve_face(
    library: &Library,
    fontconfig: &Fontconfig,
    family: &str,
    style: Style,
    size_px: u32,
) -> Result<Face, FontError> {
    let font = fontconfig
        .find(family, Some(style.fontconfig_style()))
        .map_err(|_| FontError::NoFamily(family.to_owned()))?;
    let face = library.new_face(&font.path, font.index.unwrap_or(0) as isize)?;
    size_face(&face, size_px)?;
    Ok(face)
}

/// Set a face to `size_px`. Scalable faces size directly; bitmap-strike faces
/// (e.g. colour-emoji fonts) cannot, so select the nearest available strike and
/// let the renderer scale its glyphs into the cell.
fn size_face(face: &Face, size_px: u32) -> Result<(), FontError> {
    match face.set_pixel_sizes(0, size_px) {
        Ok(()) => Ok(()),
        Err(_) if face.has_fixed_sizes() => {
            face.select_size(nearest_strike(face, size_px))?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

/// Index of the fixed strike whose pixel height is closest to `target`.
fn nearest_strike(face: &Face, target: u32) -> i32 {
    let rec = face.raw();
    let target = i32::try_from(target).unwrap_or(i32::MAX);
    let mut best = 0;
    let mut best_delta = i32::MAX;
    for i in 0..rec.num_fixed_sizes {
        // SAFETY: `available_sizes` points to `num_fixed_sizes` valid
        // `FT_Bitmap_Size` entries for the face's lifetime; `i` is in range.
        let height = i32::from(unsafe { (*rec.available_sizes.offset(i as isize)).height });
        let delta = (height - target).abs();
        if delta < best_delta {
            best = i;
            best_delta = delta;
        }
    }
    best
}

fn cell_metrics(face: &Face, family: &str) -> Result<CellMetrics, FontError> {
    let metrics = face
        .size_metrics()
        .ok_or_else(|| FontError::NoMetrics(family.to_owned()))?;
    // FreeType reports these in 26.6 fixed point.
    let ascent = (metrics.ascender >> 6).max(1) as u32;
    let height = (metrics.height >> 6).max(1) as u32;

    // For a monospace face every advance is equal; measure one ASCII glyph.
    face.load_char('M' as usize, LoadFlag::DEFAULT)?;
    let width = (face.glyph().advance().x >> 6).max(1) as u32;

    Ok(CellMetrics {
        width,
        height,
        ascent,
    })
}

fn rasterize(
    face: &Face,
    c: char,
    synth_bold: bool,
    synth_italic: bool,
) -> Result<Glyph, FontError> {
    // A shear transform fakes italics on a face that has no real oblique. It is
    // applied to the outline at load time, so reset it immediately after.
    if synth_italic {
        face.set_transform(&mut shear_matrix(), &mut Vector { x: 0, y: 0 });
    }
    let result = face.load_char(c as usize, LoadFlag::RENDER | LoadFlag::COLOR);
    if synth_italic {
        face.set_transform(&mut identity_matrix(), &mut Vector { x: 0, y: 0 });
    }
    result?;

    let slot = face.glyph();
    let bitmap = slot.bitmap();
    let width = bitmap.width().max(0) as usize;
    let height = bitmap.rows().max(0) as usize;
    let pitch = bitmap.pitch();
    let src = bitmap.buffer();

    let mut data = match bitmap.pixel_mode()? {
        PixelMode::Gray => GlyphData::Mask(pack_rows(src, width, pitch, height)),
        PixelMode::Bgra => GlyphData::Color(pack_rows(src, width * 4, pitch, height)),
        PixelMode::Mono => GlyphData::Mask(expand_mono(src, width, pitch, height)),
        _ => GlyphData::Mask(vec![0; width * height]),
    };
    // Fake bold by widening coverage one pixel to the right (colour glyphs are
    // left alone - there is no such thing as a bold emoji).
    if synth_bold && let GlyphData::Mask(mask) = &mut data {
        embolden(mask, width, height);
    }

    Ok(Glyph {
        left: slot.bitmap_left(),
        top: slot.bitmap_top(),
        width: width as u32,
        height: height as u32,
        data,
    })
}

fn shear_matrix() -> Matrix {
    // ~0.2 horizontal shear in 16.16 fixed point.
    Matrix {
        xx: 0x1_0000,
        xy: 0x3333,
        yx: 0,
        yy: 0x1_0000,
    }
}

fn identity_matrix() -> Matrix {
    Matrix {
        xx: 0x1_0000,
        xy: 0,
        yx: 0,
        yy: 0x1_0000,
    }
}

/// Widen each row's coverage by one pixel (synthetic bold).
fn embolden(mask: &mut [u8], width: usize, height: usize) {
    for y in 0..height {
        let row = &mut mask[y * width..y * width + width];
        for x in (1..width).rev() {
            row[x] = row[x].max(row[x - 1]);
        }
    }
}

/// Copy `height` rows of `row_bytes` each out of FreeType's padded buffer,
/// honouring pitch sign (positive = top-down).
fn pack_rows(src: &[u8], row_bytes: usize, pitch: i32, height: usize) -> Vec<u8> {
    let stride = pitch.unsigned_abs() as usize;
    let take = row_bytes.min(stride);
    let mut out = vec![0u8; row_bytes * height];
    for row in 0..height {
        let src_row = if pitch >= 0 { row } else { height - 1 - row };
        let start = src_row * stride;
        if start + take <= src.len() {
            out[row * row_bytes..row * row_bytes + take].copy_from_slice(&src[start..start + take]);
        }
    }
    out
}

/// Expand a 1-bit-per-pixel mono bitmap to one coverage byte per pixel.
fn expand_mono(src: &[u8], width: usize, pitch: i32, height: usize) -> Vec<u8> {
    let stride = pitch.unsigned_abs() as usize;
    let mut out = vec![0u8; width * height];
    for row in 0..height {
        let src_row = if pitch >= 0 { row } else { height - 1 - row };
        let base = src_row * stride;
        for x in 0..width {
            let byte = base + x / 8;
            if byte < src.len() && src[byte] & (0x80 >> (x % 8)) != 0 {
                out[row * width + x] = 0xff;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Fonts {
        Fonts::new("monospace", 16).expect("system has a monospace font")
    }

    #[test]
    fn cell_metrics_are_sane() {
        let m = fonts().metrics();
        assert!(m.width >= 1 && m.height >= 1);
        assert!(m.ascent >= 1 && m.ascent <= m.height);
    }

    #[test]
    fn ascii_glyph_has_ink() {
        let mut f = fonts();
        let glyph = f.glyph('M', Style::default()).expect("rasterize M");
        assert!(glyph.width > 0 && glyph.height > 0);
        match &glyph.data {
            GlyphData::Mask(px) => assert!(px.iter().any(|&p| p > 0), "M should have coverage"),
            GlyphData::Color(_) => {}
        }
    }

    #[test]
    fn space_is_blank_but_ok() {
        let mut f = fonts();
        // Space resolves without error; it simply carries no ink.
        f.glyph(' ', Style::default()).expect("rasterize space");
    }

    #[test]
    fn embolden_widens_coverage() {
        // 3x2 mask, one lit pixel per row at x=1.
        let mut mask = vec![0, 255, 0, 0, 200, 0];
        embolden(&mut mask, 3, 2);
        // Each lit pixel bleeds one column to the right; the left edge is unchanged.
        assert_eq!(mask, vec![0, 255, 255, 0, 200, 200]);
    }

    #[test]
    fn glyphs_are_cached() {
        let mut f = fonts();
        f.glyph('a', Style::default()).unwrap();
        let before = f.cache.len();
        f.glyph('a', Style::default()).unwrap();
        assert_eq!(f.cache.len(), before, "second lookup must hit the cache");
    }
}
