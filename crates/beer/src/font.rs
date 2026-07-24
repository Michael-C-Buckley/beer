//! Font discovery, rasterization, and glyph caching.
//!
//! fontconfig resolves family names and performs per-codepoint fallback;
//! `FreeType` rasterizes each glyph to an 8-bit coverage mask or, for colour
//! fonts, a pre-multiplied BGRA bitmap. Layout is fixed-cell, so a glyph's own
//! advance is never consulted - only the [`CellMetrics`] taken from the primary
//! face. C interop goes through the `freetype`/`fontconfig` safe wrappers; the
//! sole `unsafe` is reading a face's fixed-strike array (see `nearest_strike`).

use std::{collections::HashMap, fmt, fs, num::NonZeroUsize, path::PathBuf};

use fontconfig::{CharSet, Fontconfig, Pattern};
use freetype::{
  Face,
  LcdFilter,
  Library,
  Matrix,
  Vector,
  bitmap::PixelMode,
  face::{LoadFlag, StyleFlag},
};
use harfbuzz_rs_now as harfbuzz;
use lru::LruCache;
use thiserror::Error;

use crate::config::Subpixel;

/// Upper bound on cached glyphs; the working set of a terminal is far smaller,
/// but this caps memory under adversarial all-of-Unicode output.
const GLYPH_CACHE_CAP: usize = 4096;

/// Upper bound on cached shaped clusters (base char + combining marks).
const SHAPE_CACHE_CAP: usize = 1024;

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
  #[error("glyph cache insertion failed")]
  CacheInvariant,
}

/// Bold/italic selection, used both to pick a face and to key the glyph cache.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Style {
  pub bold:   bool,
  pub italic: bool,
}

impl Style {
  /// Dense index in `0..4` for array storage.
  fn index(self) -> usize {
    usize::from(self.bold) | (usize::from(self.italic) << 1)
  }

  const fn fontconfig_style(self) -> &'static str {
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
  pub width:  u32,
  pub height: u32,
  /// Baseline offset from the top of the cell.
  pub ascent: u32,
  /// Light stroke thickness in pixels, from the face's underline thickness.
  /// Box-drawing lines use this so they match the font's visual weight.
  pub stroke: u32,
}

/// A rasterized glyph: its bitmap plus the offsets to place it on the baseline.
#[derive(Clone, Debug)]
pub struct Glyph {
  /// Horizontal offset from the pen position to the bitmap's left edge.
  pub left:   i32,
  /// Vertical offset from the baseline up to the bitmap's top edge.
  pub top:    i32,
  pub width:  u32,
  pub height: u32,
  pub data:   GlyphData,
}

/// Glyph pixel data. A `Mask` is tinted with the cell's foreground colour; a
/// `Color` bitmap (emoji) is composited directly.
#[derive(Clone, Debug)]
pub enum GlyphData {
  /// One coverage byte per pixel.
  Mask(Vec<u8>),
  /// LCD subpixel coverage: three bytes per pixel in `FreeType`'s horizontal
  /// order (physically leftmost, middle, rightmost subpixel). The renderer maps
  /// these onto R/G/B according to the panel's configured [`Subpixel`] order.
  Lcd(Vec<u8>),
  /// Pre-multiplied BGRA, four bytes per pixel.
  Color(Vec<u8>),
}

/// One shaped glyph in a cluster: a glyph index into a specific face plus the
/// pixel offset, relative to the cell origin and baseline, that `HarfBuzz`
/// placed it at. `x` grows rightward, `y` upward (away from the baseline).
#[derive(Clone, Copy, Debug)]
pub struct Placed {
  pub gid: u32,
  pub x:   i32,
  pub y:   i32,
}

/// The result of shaping a base char plus its combining marks: the face the
/// cluster was shaped against and the positioned glyphs to draw, in order.
#[derive(Clone, Debug)]
pub struct ShapedCluster {
  pub face_idx: usize,
  pub glyphs:   Vec<Placed>,
}

/// A loaded face plus where it came from, so `HarfBuzz` can be handed the same
/// font bytes that `FreeType` rasterizes from.
struct FaceEntry {
  face:  Face,
  path:  PathBuf,
  index: u32,
  /// `HarfBuzz` font for this face, built on first shape against it.
  hb:    Option<harfbuzz::Owned<harfbuzz::Font<'static>>>,
}

/// The font set for one terminal: a primary family with lazily-loaded
/// bold/italic variants and per-codepoint fallback faces, plus glyph caches.
pub struct Fonts {
  library:     Library,
  fontconfig:  Fontconfig,
  family:      String,
  size_px:     u32,
  /// Subpixel order for LCD rendering; `None` keeps grayscale coverage.
  subpixel:    Subpixel,
  metrics:     CellMetrics,
  /// All loaded faces; indices into this vector are stable.
  faces:       Vec<FaceEntry>,
  /// Index of each style variant, by [`Style::index`]; filled on demand.
  styled:      [Option<usize>; 4],
  /// Fallback faces resolved by coverage, deduplicated by file path.
  fallbacks:   HashMap<PathBuf, usize>,
  /// Glyphs keyed by `char` (the common, unshaped path).
  cache:       LruCache<(char, usize), Glyph>,
  /// Glyphs keyed by `(glyph index, face, style)` (the shaped path).
  gcache:      LruCache<(u32, usize, usize), Glyph>,
  /// Shaped clusters keyed by `(cluster string, style)`.
  shape_cache: LruCache<(Box<str>, usize), Option<ShapedCluster>>,
}

impl fmt::Debug for Fonts {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Fonts")
      .field("family", &self.family)
      .field("size_px", &self.size_px)
      .field("metrics", &self.metrics)
      .field("faces", &self.faces.len())
      .field("cached", &self.cache.len())
      .finish_non_exhaustive()
  }
}

impl Fonts {
  /// Resolve `family` at `size_px` and compute the cell metrics. `subpixel`
  /// selects LCD rendering; it is downgraded to grayscale if the `FreeType`
  /// build lacks LCD-filter support.
  pub fn new(
    family: &str,
    size_px: u32,
    subpixel: Subpixel,
  ) -> Result<Self, FontError> {
    let library = Library::init()?;
    let fontconfig = Fontconfig::new().ok_or(FontError::FontconfigInit)?;

    // The LCD filter is a library-global FreeType setting; enable it once here
    // so LCD-rendered glyphs are filtered to suppress colour fringing.
    let subpixel = if subpixel != Subpixel::None
      && library.set_lcd_filter(LcdFilter::LcdFilterDefault).is_err()
    {
      tracing::warn!("FreeType lacks LCD filter support; using grayscale");
      Subpixel::None
    } else {
      subpixel
    };

    let regular =
      resolve_face(&library, &fontconfig, family, Style::default(), size_px)?;
    let metrics = cell_metrics(&regular.face, family)?;

    let cap = |n| NonZeroUsize::new(n).ok_or(FontError::CacheInvariant);
    Ok(Self {
      library,
      fontconfig,
      family: family.to_owned(),
      size_px,
      subpixel,
      metrics,
      faces: vec![regular],
      styled: [Some(0), None, None, None],
      fallbacks: HashMap::new(),
      cache: LruCache::new(cap(GLYPH_CACHE_CAP)?),
      gcache: LruCache::new(cap(GLYPH_CACHE_CAP)?),
      shape_cache: LruCache::new(cap(SHAPE_CACHE_CAP)?),
    })
  }

  pub const fn metrics(&self) -> CellMetrics {
    self.metrics
  }

  /// The active subpixel order; `None` when rendering grayscale coverage.
  pub const fn subpixel(&self) -> Subpixel {
    self.subpixel
  }

  /// Return the rasterized glyph for `c` in `style`, rasterizing and caching
  /// it on first use.
  pub fn glyph(&mut self, c: char, style: Style) -> Result<&Glyph, FontError> {
    let key = (c, style.index());
    if self.cache.get(&key).is_none() {
      let lcd = self.subpixel != Subpixel::None;
      let idx = self.face_for(c, style)?;
      let face = &self.faces[idx].face;
      // Synthesize bold/italic only when the resolved face lacks the real
      // variant (most monospace families ship both).
      let (synth_bold, synth_italic) = synth_flags(face, style);
      let glyph = rasterize(face, c, synth_bold, synth_italic, lcd)?;
      self.cache.put(key, glyph);
    }
    self.cache.get(&key).ok_or(FontError::CacheInvariant)
  }

  /// Return the rasterized glyph for glyph index `gid` in `face_idx`,
  /// rasterizing and caching on first use. Used by the shaped path, where
  /// `HarfBuzz` has already chosen the face and glyph.
  pub fn glyph_indexed(
    &mut self,
    face_idx: usize,
    gid: u32,
    style: Style,
  ) -> Result<&Glyph, FontError> {
    let key = (gid, face_idx, style.index());
    if self.gcache.get(&key).is_none() {
      let lcd = self.subpixel != Subpixel::None;
      let face = &self.faces[face_idx].face;
      let (synth_bold, synth_italic) = synth_flags(face, style);
      let glyph = rasterize_index(face, gid, synth_bold, synth_italic, lcd)?;
      self.gcache.put(key, glyph);
    }
    self.gcache.get(&key).ok_or(FontError::CacheInvariant)
  }

  /// Rasterize `c` in `style` at `scale` times the base size, uncached.
  ///
  /// Scaled glyphs come from the text-sizing protocol (`OSC 66`); they are
  /// rare and transient, so they bypass the glyph cache. The scale is applied
  /// as an outline transform, which leaves the face's configured pixel size -
  /// and therefore the cell metrics every other glyph depends on - untouched.
  /// Embedded-bitmap (colour) glyphs ignore the transform; the caller scales
  /// those at blit time instead.
  pub fn glyph_scaled(
    &mut self,
    c: char,
    style: Style,
    scale: f32,
  ) -> Result<Glyph, FontError> {
    let idx = self.face_for(c, style)?;
    let face = &self.faces[idx].face;
    let (synth_bold, synth_italic) = synth_flags(face, style);
    rasterize_scaled(face, c, scale.max(0.01), synth_bold, synth_italic)
  }

  /// Shape `base` plus its combining `marks` into positioned glyphs using
  /// `HarfBuzz`, so marks land where the font's GPOS table wants them rather
  /// than stacked at the origin. Returns `None` when shaping is unavailable or
  /// the cluster has glyphs the face does not cover (`.notdef`), so the caller
  /// can fall back to drawing the marks stacked. Results are cached.
  pub fn shape_cluster(
    &mut self,
    base: char,
    marks: &str,
    style: Style,
  ) -> Option<ShapedCluster> {
    let mut cluster = String::with_capacity(base.len_utf8() + marks.len());
    cluster.push(base);
    cluster.push_str(marks);
    let key = (cluster.clone().into_boxed_str(), style.index());
    if let Some(cached) = self.shape_cache.get(&key) {
      return cached.clone();
    }
    let shaped = self.shape_uncached(base, &cluster, style);
    self.shape_cache.put(key, shaped.clone());
    shaped
  }

  fn shape_uncached(
    &mut self,
    base: char,
    cluster: &str,
    style: Style,
  ) -> Option<ShapedCluster> {
    let face_idx = self.face_for(base, style).ok()?;
    let font = self.hb_font(face_idx)?;
    let buffer = harfbuzz::UnicodeBuffer::new().add_str(cluster);
    let output = harfbuzz::shape(font, buffer, &[]);
    let infos = output.get_glyph_infos();
    let positions = output.get_glyph_positions();
    let mut glyphs = Vec::with_capacity(infos.len());
    let mut pen = 0i32;
    for (info, pos) in infos.iter().zip(positions) {
      // A .notdef means this face does not cover part of the cluster; bail
      // so the caller stacks the marks via per-char fallback instead.
      if info.codepoint == 0 {
        return None;
      }
      glyphs.push(Placed {
        gid: info.codepoint,
        // HarfBuzz positions are 26.6 fixed point at our pixel scale.
        x:   (pen + pos.x_offset) >> 6,
        y:   pos.y_offset >> 6,
      });
      pen += pos.x_advance;
    }
    Some(ShapedCluster { face_idx, glyphs })
  }

  /// Lazily build the `HarfBuzz` font for `face_idx` from the same file bytes
  /// `FreeType` loaded. The bytes are leaked to `'static`: a face lives for the
  /// process, and only the handful actually used to shape clusters allocate.
  fn hb_font(
    &mut self,
    face_idx: usize,
  ) -> Option<&harfbuzz::Owned<harfbuzz::Font<'static>>> {
    if self.faces[face_idx].hb.is_none() {
      let entry = &self.faces[face_idx];
      let bytes = fs::read(&entry.path).ok()?;
      let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
      let face = harfbuzz::Face::from_bytes(leaked, entry.index);
      let mut font = harfbuzz::Font::new(face);
      let scale = i32::try_from(self.size_px).unwrap_or(i32::MAX) * 64;
      font.set_scale(scale, scale);
      font.set_ppem(self.size_px, self.size_px);
      self.faces[face_idx].hb = Some(font);
    }
    self.faces[face_idx].hb.as_ref()
  }

  /// Pick the face that should render `c`: the requested style if it has the
  /// glyph, then regular, then known fallbacks, then a fresh fontconfig
  /// coverage match. Falls back to the styled face (rendering `.notdef`).
  fn face_for(&mut self, c: char, style: Style) -> Result<usize, FontError> {
    let styled = self.styled_face(style)?;
    if face_has_glyph(&self.faces[styled].face, c) {
      return Ok(styled);
    }
    if let Some(regular) = self.styled[0]
      && regular != styled
      && face_has_glyph(&self.faces[regular].face, c)
    {
      return Ok(regular);
    }
    for &idx in self.fallbacks.values() {
      if face_has_glyph(&self.faces[idx].face, c) {
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
    let regular = self.styled[0].ok_or(FontError::CacheInvariant)?;
    let idx = match resolve_face(
      &self.library,
      &self.fontconfig,
      &self.family,
      style,
      self.size_px,
    ) {
      Ok(entry) => {
        self.faces.push(entry);
        self.faces.len() - 1
      },
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
    let index = matched.face_index().unwrap_or(0);
    let face = self
      .library
      .new_face(&path, isize::try_from(index).unwrap_or(isize::MAX))?;
    if size_face(&face, self.size_px).is_err() {
      return Ok(None);
    }
    self.faces.push(FaceEntry {
      face,
      path: path.clone(),
      index: u32::try_from(index).unwrap_or(u32::MAX),
      hb: None,
    });
    let idx = self.faces.len() - 1;
    self.fallbacks.insert(path, idx);
    Ok(Some(idx))
  }
}

fn face_has_glyph(face: &Face, c: char) -> bool {
  face
    .get_char_index(usize::try_from(u32::from(c)).unwrap_or(usize::MAX))
    .is_some_and(|g| g != 0)
}

/// Whether bold/italic must be synthesized: only when the requested style is
/// set but the resolved face lacks the real variant.
fn synth_flags(face: &Face, style: Style) -> (bool, bool) {
  let flags = face.style_flags();
  let synth_bold = style.bold && !flags.contains(StyleFlag::BOLD);
  let synth_italic = style.italic && !flags.contains(StyleFlag::ITALIC);
  (synth_bold, synth_italic)
}

fn resolve_face(
  library: &Library,
  fontconfig: &Fontconfig,
  family: &str,
  style: Style,
  size_px: u32,
) -> Result<FaceEntry, FontError> {
  let font = fontconfig
    .find(family, Some(style.fontconfig_style()))
    .map_err(|_| FontError::NoFamily(family.to_owned()))?;
  let index = font.index.unwrap_or(0);
  let face = library
    .new_face(&font.path, isize::try_from(index).unwrap_or(isize::MAX))?;
  size_face(&face, size_px)?;
  Ok(FaceEntry {
    face,
    path: font.path,
    index: u32::try_from(index).unwrap_or(u32::MAX),
    hb: None,
  })
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
    },
    Err(err) => Err(err.into()),
  }
}

/// Index of the fixed strike whose pixel height is closest to `target`.
#[expect(
  unsafe_code,
  reason = "FreeType exposes fixed strikes only through its validated raw \
            face record"
)]
fn nearest_strike(face: &Face, target: u32) -> i32 {
  let rec = face.raw();
  let target = i32::try_from(target).unwrap_or(i32::MAX);
  let mut best = 0;
  let mut best_delta = i32::MAX;
  for i in 0..rec.num_fixed_sizes {
    // SAFETY: `available_sizes` points to `num_fixed_sizes` valid
    // `FT_Bitmap_Size` entries for the face's lifetime; `i` is in range.
    let height = i32::from(unsafe {
      (*rec
        .available_sizes
        .offset(isize::try_from(i).unwrap_or(isize::MAX)))
      .height
    });
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
  let ascent =
    u32::try_from((metrics.ascender >> 6).max(1)).unwrap_or(u32::MAX);
  let height = u32::try_from((metrics.height >> 6).max(1)).unwrap_or(u32::MAX);

  // For a monospace face every advance is equal; measure one ASCII glyph.
  face.load_char(
    usize::try_from(u32::from('M')).unwrap_or(usize::MAX),
    LoadFlag::DEFAULT,
  )?;
  let width =
    u32::try_from((face.glyph().advance().x >> 6).max(1)).unwrap_or(u32::MAX);

  // Scale the face's underline thickness (font units) to pixels via the size's
  // y-scale: `FT_MulFix` gives 26.6 pixels, then `>> 6`. Bitmap/colour faces
  // may report zero, so fall back to a small fraction of the cell height.
  let raw = i64::from(face.underline_thickness());
  let scaled = (raw * metrics.y_scale + 0x8000) >> 16;
  let underline_px = u32::try_from((scaled >> 6).max(0)).unwrap_or(u32::MAX);
  let stroke = if underline_px > 0 {
    underline_px
  } else {
    (height / 12).max(1)
  };

  Ok(CellMetrics {
    width,
    height,
    ascent,
    stroke,
  })
}

fn rasterize(
  face: &Face,
  c: char,
  synth_bold: bool,
  synth_italic: bool,
  lcd: bool,
) -> Result<Glyph, FontError> {
  let flags = load_flags(lcd);
  rasterize_with(face, synth_bold, synth_italic, |face| {
    face.load_char(usize::try_from(u32::from(c)).unwrap_or(usize::MAX), flags)
  })
}

/// Rasterize by glyph index rather than character (the shaped path).
fn rasterize_index(
  face: &Face,
  gid: u32,
  synth_bold: bool,
  synth_italic: bool,
  lcd: bool,
) -> Result<Glyph, FontError> {
  let flags = load_flags(lcd);
  rasterize_with(face, synth_bold, synth_italic, |face| {
    face.load_glyph(gid, flags)
  })
}

/// Load flags for a normal render: `TARGET_LCD` requests horizontal subpixel
/// coverage; otherwise `FreeType` renders 8-bit grayscale. `COLOR` still yields
/// a BGRA bitmap for colour glyphs regardless of the target.
fn load_flags(lcd: bool) -> LoadFlag {
  let mut flags = LoadFlag::RENDER | LoadFlag::COLOR;
  if lcd {
    flags |= LoadFlag::TARGET_LCD;
  }
  flags
}

/// Rasterize `c` with the outline scaled by `scale` (and sheared if italic is
/// synthesized). The transform is reset before returning so the face is left as
/// it was found.
#[expect(
  clippy::cast_possible_truncation,
  reason = "FreeType requires a bounded 16.16 fixed-point transform"
)]
fn rasterize_scaled(
  face: &Face,
  c: char,
  scale: f32,
  synth_bold: bool,
  synth_italic: bool,
) -> Result<Glyph, FontError> {
  // A scale matrix on the diagonal, in 16.16 fixed point; the off-diagonal
  // `xy` term shears for a synthetic italic (~0.2 of the glyph height). `as _`
  // takes the field's `FT_Fixed` type, as the identity/shear matrices do.
  let mut matrix = Matrix {
    xx: (scale * 65536.0).round() as _,
    xy: if synth_italic {
      (scale * 0.2 * 65536.0).round() as _
    } else {
      0
    },
    yx: 0,
    yy: (scale * 65536.0).round() as _,
  };
  face.set_transform(&mut matrix, &mut Vector { x: 0, y: 0 });
  let result = face.load_char(
    usize::try_from(u32::from(c)).unwrap_or(usize::MAX),
    LoadFlag::RENDER | LoadFlag::COLOR,
  );
  face.set_transform(&mut identity_matrix(), &mut Vector { x: 0, y: 0 });
  result?;

  let slot = face.glyph();
  let bitmap = slot.bitmap();
  let width = usize::try_from(bitmap.width().max(0)).unwrap_or(usize::MAX);
  let height = usize::try_from(bitmap.rows().max(0)).unwrap_or(usize::MAX);
  let pitch = bitmap.pitch();
  let src = bitmap.buffer();
  let mut data = match bitmap.pixel_mode()? {
    PixelMode::Gray => GlyphData::Mask(pack_rows(src, width, pitch, height)),
    PixelMode::Bgra => {
      GlyphData::Color(pack_rows(src, width * 4, pitch, height))
    },
    PixelMode::Mono => GlyphData::Mask(expand_mono(src, width, pitch, height)),
    _ => GlyphData::Mask(vec![0; width * height]),
  };
  if synth_bold && let GlyphData::Mask(mask) = &mut data {
    embolden(mask, width, height);
  }
  Ok(Glyph {
    left: slot.bitmap_left(),
    top: slot.bitmap_top(),
    width: u32::try_from(width).unwrap_or(u32::MAX),
    height: u32::try_from(height).unwrap_or(u32::MAX),
    data,
  })
}

fn rasterize_with(
  face: &Face,
  synth_bold: bool,
  synth_italic: bool,
  load: impl FnOnce(&Face) -> Result<(), freetype::Error>,
) -> Result<Glyph, FontError> {
  // A shear transform fakes italics on a face that has no real oblique. It is
  // applied to the outline at load time, so reset it immediately after.
  if synth_italic {
    face.set_transform(&mut shear_matrix(), &mut Vector { x: 0, y: 0 });
  }
  let result = load(face);
  if synth_italic {
    face.set_transform(&mut identity_matrix(), &mut Vector { x: 0, y: 0 });
  }
  result?;

  let slot = face.glyph();
  let bitmap = slot.bitmap();
  // For an LCD bitmap this is the subpixel column count (three per pixel); for
  // every other mode it is the pixel width.
  let cols = usize::try_from(bitmap.width().max(0)).unwrap_or(usize::MAX);
  let height = usize::try_from(bitmap.rows().max(0)).unwrap_or(usize::MAX);
  let pitch = bitmap.pitch();
  let src = bitmap.buffer();

  // `width` is the logical (whole-pixel) glyph width used by both the pixel
  // buffer sizing and the placed `Glyph`.
  let (mut data, width) = match bitmap.pixel_mode()? {
    PixelMode::Gray => {
      (GlyphData::Mask(pack_rows(src, cols, pitch, height)), cols)
    },
    PixelMode::Lcd => {
      let logical = cols / 3;
      (
        GlyphData::Lcd(pack_rows(src, logical * 3, pitch, height)),
        logical,
      )
    },
    PixelMode::Bgra => {
      (
        GlyphData::Color(pack_rows(src, cols * 4, pitch, height)),
        cols,
      )
    },
    PixelMode::Mono => {
      (GlyphData::Mask(expand_mono(src, cols, pitch, height)), cols)
    },
    _ => (GlyphData::Mask(vec![0; cols * height]), cols),
  };
  // Fake bold by widening coverage one pixel to the right (colour glyphs are
  // left alone - there is no such thing as a bold emoji).
  if synth_bold {
    match &mut data {
      GlyphData::Mask(mask) => embolden(mask, width, height),
      GlyphData::Lcd(sub) => embolden_lcd(sub, width, height),
      GlyphData::Color(_) => {},
    }
  }

  Ok(Glyph {
    left: slot.bitmap_left(),
    top: slot.bitmap_top(),
    width: u32::try_from(width).unwrap_or(u32::MAX),
    height: u32::try_from(height).unwrap_or(u32::MAX),
    data,
  })
}

const fn shear_matrix() -> Matrix {
  // ~0.2 horizontal shear in 16.16 fixed point.
  Matrix {
    xx: 0x1_0000,
    xy: 0x3333,
    yx: 0,
    yy: 0x1_0000,
  }
}

const fn identity_matrix() -> Matrix {
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

/// Synthetic bold for LCD coverage: widen by one whole pixel, comparing each
/// subpixel with the same channel of the pixel to its left so channels do not
/// bleed into one another.
fn embolden_lcd(sub: &mut [u8], width: usize, height: usize) {
  let stride = width * 3;
  for y in 0..height {
    let row = &mut sub[y * stride..y * stride + stride];
    for x in (1..width).rev() {
      for c in 0..3 {
        row[x * 3 + c] = row[x * 3 + c].max(row[(x - 1) * 3 + c]);
      }
    }
  }
}

/// Copy `height` rows of `row_bytes` each out of `FreeType`'s padded buffer,
/// honouring pitch sign (positive = top-down).
fn pack_rows(
  src: &[u8],
  row_bytes: usize,
  pitch: i32,
  height: usize,
) -> Vec<u8> {
  let stride = usize::try_from(pitch.unsigned_abs()).unwrap_or(usize::MAX);
  let take = row_bytes.min(stride);
  let mut out = vec![0u8; row_bytes * height];
  for row in 0..height {
    let src_row = if pitch >= 0 { row } else { height - 1 - row };
    let start = src_row * stride;
    if start + take <= src.len() {
      out[row * row_bytes..row * row_bytes + take]
        .copy_from_slice(&src[start..start + take]);
    }
  }
  out
}

/// Expand a 1-bit-per-pixel mono bitmap to one coverage byte per pixel.
fn expand_mono(src: &[u8], width: usize, pitch: i32, height: usize) -> Vec<u8> {
  let stride = usize::try_from(pitch.unsigned_abs()).unwrap_or(usize::MAX);
  let mut out = vec![0u8; width * height];
  for row in 0..height {
    let src_row = if pitch >= 0 { row } else { height - 1 - row };
    let base = src_row * stride;
    for x in 0..width {
      let byte = base + x / 8;
      if byte < src.len() && src[byte] & (0x80 >> (x % 8)) != 0 {
        out[row * width + x] = 0xFF;
      }
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fonts() -> Fonts {
    Fonts::new("monospace", 16, Subpixel::None)
      .expect("system has a monospace font")
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
      GlyphData::Mask(px) | GlyphData::Lcd(px) => {
        assert!(px.iter().any(|&p| p > 0), "M should have coverage");
      },
      GlyphData::Color(_) => {},
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
    // Each lit pixel bleeds one column to the right; the left edge is
    // unchanged.
    assert_eq!(mask, vec![0, 255, 255, 0, 200, 200]);
  }

  #[test]
  fn shapes_a_simple_cluster() {
    let mut f = fonts();
    // Shaping a bare base char yields exactly its one glyph, and that glyph
    // index rasterizes to ink through the shaped path.
    let shaped = f
      .shape_cluster('a', "", Style::default())
      .expect("monospace shapes 'a'");
    assert_eq!(shaped.glyphs.len(), 1);
    let g = f
      .glyph_indexed(shaped.face_idx, shaped.glyphs[0].gid, Style::default())
      .expect("rasterize shaped glyph");
    match &g.data {
      GlyphData::Mask(px) | GlyphData::Lcd(px) => {
        assert!(px.iter().any(|&p| p > 0), "'a' should have ink");
      },
      GlyphData::Color(_) => {},
    }
  }

  #[test]
  fn shapes_combining_cluster_without_notdef() {
    // 'e' + combining acute: a covering face shapes it (>=1 glyph, never a
    // .notdef, which `shape_cluster` rejects by returning None); a face
    // missing the mark returns None so the renderer stacks instead. Either
    // outcome is fine - the point is no panic and no notdef leaking through.
    let mut f = fonts();
    if let Some(shaped) = f.shape_cluster('e', "\u{0301}", Style::default()) {
      assert!(!shaped.glyphs.is_empty());
      assert!(shaped.glyphs.iter().all(|g| g.gid != 0));
    }
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
