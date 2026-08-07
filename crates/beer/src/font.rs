//! Font discovery, rasterization, and glyph caching.
//!
//! fontconfig resolves family names and performs per-codepoint fallback;
//! `FreeType` rasterizes each glyph to an 8-bit coverage mask or, for colour
//! fonts, a pre-multiplied BGRA bitmap. Layout is fixed-cell, so a glyph's own
//! advance is never consulted - only the [`CellMetrics`] taken from the primary
//! face. C interop goes through the `freetype`/`fontconfig` safe wrappers,
//! except two spots that reach the raw `FreeType` API: reading a face's
//! fixed-strike array (`nearest_strike`) and setting variable-font axes
//! (`apply_variations`).

use std::{
  collections::HashMap,
  fmt,
  fs,
  num::NonZeroUsize,
  path::PathBuf,
  ptr,
};

use fontconfig::{CharSet, Fontconfig, Pattern};
use freetype::{
  Face,
  LcdFilter,
  Library,
  Matrix,
  Vector,
  bitmap::PixelMode,
  face::{LoadFlag, StyleFlag},
  ffi,
};
use harfbuzz_rs_now as harfbuzz;
use lru::LruCache;
use thiserror::Error;

use crate::config::{Hinting, Subpixel};

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

/// Everything the font subsystem needs from configuration, borrowed for the
/// duration of [`Fonts::new`]. [`FontOptions::new`] fills in the defaults so a
/// caller sets only the fields it cares about.
#[derive(Clone, Copy, Debug)]
pub struct FontOptions<'a> {
  /// Primary family, resolved via fontconfig.
  pub family:             &'a str,
  /// Per-style family overrides; `None` resolves the primary family's style.
  pub bold_family:        Option<&'a str>,
  pub italic_family:      Option<&'a str>,
  pub bold_italic_family: Option<&'a str>,
  /// Fallback families tried in order before fontconfig coverage matching.
  pub fallback:           &'a [String],
  /// Variation-axis settings, each `tag=value`, applied to the primary family
  /// and its style and fallback variants.
  pub variations:         &'a [String],
  /// OpenType feature settings, each a tag with an optional `+`/`-`/`=value`.
  pub features:           &'a [String],
  /// Whether to shape runs (ligatures, contextual alternates).
  pub ligatures:          bool,
  pub size_px:            u32,
  pub hinting:            Hinting,
  /// Pixels added to the cell advance width, height, and baseline offset.
  pub adjust_width:       i32,
  pub adjust_height:      i32,
  pub adjust_baseline:    i32,
  /// Thicken every glyph by one coverage pixel, a light synthetic weight.
  pub thicken:            bool,
  pub subpixel:           Subpixel,
}

impl<'a> FontOptions<'a> {
  /// Options for `family` at `size_px` with `subpixel` order and every other
  /// knob left at its default.
  pub const fn new(family: &'a str, size_px: u32, subpixel: Subpixel) -> Self {
    Self {
      family,
      bold_family: None,
      italic_family: None,
      bold_italic_family: None,
      fallback: &[],
      variations: &[],
      features: &[],
      ligatures: true,
      size_px,
      hinting: Hinting::Normal,
      adjust_width: 0,
      adjust_height: 0,
      adjust_baseline: 0,
      thicken: false,
      subpixel,
    }
  }
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

/// One shaped glyph of a text run: its glyph index, the byte offset in the run
/// of the cluster it belongs to, and its pixel offset from that cluster's cell
/// origin. `gid == 0` marks a code point the shaping face does not cover.
#[derive(Clone, Copy, Debug)]
pub struct RunGlyph {
  pub gid:     u32,
  pub cluster: u32,
  pub x:       i32,
  pub y:       i32,
}

/// The result of shaping a run of cells against one face: the face used and the
/// positioned glyphs, whose `cluster` maps each back to the originating cell.
#[derive(Clone, Debug)]
pub struct ShapedRun {
  pub face_idx: usize,
  pub glyphs:   Vec<RunGlyph>,
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
  library:           Library,
  fontconfig:        Fontconfig,
  family:            String,
  size_px:           u32,
  /// Subpixel order for LCD rendering; `None` keeps grayscale coverage.
  subpixel:          Subpixel,
  /// Outline grid-fitting strength for every rasterization.
  hinting:           Hinting,
  /// Thicken every glyph by one coverage pixel.
  thicken:           bool,
  metrics:           CellMetrics,
  /// Per-style family override; slot 0 (regular) is unused. `None` resolves
  /// the primary family's style variant instead.
  style_family:      [Option<String>; 4],
  /// Parsed variation-axis settings `(tag, value)` applied to named faces.
  variations:        Vec<(u32, f32)>,
  /// Parsed OpenType feature settings passed to every shaping call.
  features:          Vec<harfbuzz::Feature>,
  /// Whether run shaping (ligatures) is enabled.
  ligatures:         bool,
  /// Configured fallback families, tried in order before coverage matching.
  fallback_families: Vec<String>,
  /// Face indices for the configured fallback families, resolved on first use.
  fallback_chain:    Vec<usize>,
  /// Whether [`Self::fallback_chain`] has been populated.
  fallback_ready:    bool,
  /// All loaded faces; indices into this vector are stable.
  faces:             Vec<FaceEntry>,
  /// Index of each style variant, by [`Style::index`]; filled on demand.
  styled:            [Option<usize>; 4],
  /// Fallback faces resolved by coverage, deduplicated by file path.
  fallbacks:         HashMap<PathBuf, usize>,
  /// Glyphs keyed by `char` (the common, unshaped path).
  cache:             LruCache<(char, usize), Glyph>,
  /// Glyphs keyed by `(glyph index, face, style)` (the shaped path).
  gcache:            LruCache<(u32, usize, usize), Glyph>,
  /// Shaped clusters keyed by `(cluster string, style)`.
  shape_cache:       LruCache<(Box<str>, usize), Option<ShapedCluster>>,
  /// Shaped runs keyed by `(run string, style)`.
  run_cache:         LruCache<(Box<str>, usize), Option<ShapedRun>>,
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
  /// Resolve the primary family at `options.size_px` and compute the cell
  /// metrics. `options.subpixel` selects LCD rendering; it is downgraded to
  /// grayscale if the `FreeType` build lacks LCD-filter support.
  pub fn new(options: &FontOptions) -> Result<Self, FontError> {
    let library = Library::init()?;
    let fontconfig = Fontconfig::new().ok_or(FontError::FontconfigInit)?;

    // The LCD filter is a library-global FreeType setting; enable it once here
    // so LCD-rendered glyphs are filtered to suppress colour fringing.
    let subpixel = if options.subpixel != Subpixel::None
      && library.set_lcd_filter(LcdFilter::LcdFilterDefault).is_err()
    {
      tracing::warn!("FreeType lacks LCD filter support; using grayscale");
      Subpixel::None
    } else {
      options.subpixel
    };

    let size_px = options.size_px;
    let variations = parse_variations(options.variations);
    let regular = resolve_face(
      &library,
      &fontconfig,
      options.family,
      Style::default(),
      size_px,
      &variations,
    )?;
    let metrics = adjust_metrics(
      cell_metrics(&regular.face, options.family)?,
      options.adjust_width,
      options.adjust_height,
      options.adjust_baseline,
    );

    let style_family = [
      None,
      options.bold_family.map(str::to_owned),
      options.italic_family.map(str::to_owned),
      options.bold_italic_family.map(str::to_owned),
    ];

    let cap = |n| NonZeroUsize::new(n).ok_or(FontError::CacheInvariant);
    Ok(Self {
      library,
      fontconfig,
      family: options.family.to_owned(),
      size_px,
      subpixel,
      hinting: options.hinting,
      thicken: options.thicken,
      metrics,
      style_family,
      variations,
      features: parse_features(options.features),
      ligatures: options.ligatures,
      fallback_families: options.fallback.to_vec(),
      fallback_chain: Vec::new(),
      fallback_ready: false,
      faces: vec![regular],
      styled: [Some(0), None, None, None],
      fallbacks: HashMap::new(),
      cache: LruCache::new(cap(GLYPH_CACHE_CAP)?),
      gcache: LruCache::new(cap(GLYPH_CACHE_CAP)?),
      shape_cache: LruCache::new(cap(SHAPE_CACHE_CAP)?),
      run_cache: LruCache::new(cap(SHAPE_CACHE_CAP)?),
    })
  }

  pub const fn metrics(&self) -> CellMetrics {
    self.metrics
  }

  /// Whether run shaping (ligatures, contextual alternates) is enabled.
  pub const fn ligatures(&self) -> bool {
    self.ligatures
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
      let bold = synth_bold || self.thicken;
      let glyph = rasterize(face, c, bold, synth_italic, lcd, self.hinting)?;
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
      let bold = synth_bold || self.thicken;
      let glyph =
        rasterize_index(face, gid, bold, synth_italic, lcd, self.hinting)?;
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
    let bold = synth_bold || self.thicken;
    rasterize_scaled(face, c, scale.max(0.01), bold, synth_italic)
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
    let features = self.features.clone();
    let font = self.hb_font(face_idx)?;
    let buffer = harfbuzz::UnicodeBuffer::new().add_str(cluster);
    let output = harfbuzz::shape(font, buffer, &features);
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

  /// Shape a run of text against the styled face using `HarfBuzz`, producing
  /// ligatures and contextual alternates. Every glyph carries the byte offset
  /// of its cluster so the renderer can map it back to a cell; `.notdef`
  /// glyphs are kept so the caller can fall back per code point. Returns `None`
  /// only when shaping is unavailable for the face. Results are cached.
  pub fn shape_run(&mut self, text: &str, style: Style) -> Option<ShapedRun> {
    if text.is_empty() {
      return None;
    }
    let key = (Box::from(text), style.index());
    if let Some(cached) = self.run_cache.get(&key) {
      return cached.clone();
    }
    let shaped = self.shape_run_uncached(text, style);
    self.run_cache.put(key, shaped.clone());
    shaped
  }

  fn shape_run_uncached(
    &mut self,
    text: &str,
    style: Style,
  ) -> Option<ShapedRun> {
    let face_idx = self.styled_face(style).ok()?;
    let features = self.features.clone();
    let font = self.hb_font(face_idx)?;
    let buffer = harfbuzz::UnicodeBuffer::new().add_str(text);
    let output = harfbuzz::shape(font, buffer, &features);
    let infos = output.get_glyph_infos();
    let positions = output.get_glyph_positions();
    let mut glyphs = Vec::with_capacity(infos.len());
    let mut pen = 0i32;
    let mut cluster = u32::MAX;
    let mut cluster_pen = 0i32;
    for (info, pos) in infos.iter().zip(positions) {
      // Each glyph is placed relative to the origin of its own cluster's cell;
      // advances accumulate only within a cluster (for stacked marks).
      if info.cluster != cluster {
        cluster = info.cluster;
        cluster_pen = pen;
      }
      glyphs.push(RunGlyph {
        gid:     info.codepoint,
        cluster: info.cluster,
        x:       (pen - cluster_pen + pos.x_offset) >> 6,
        y:       pos.y_offset >> 6,
      });
      pen += pos.x_advance;
    }
    Some(ShapedRun { face_idx, glyphs })
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
      if !self.variations.is_empty() {
        let vars: Vec<harfbuzz::Variation> = self
          .variations
          .iter()
          .map(|&(tag, value)| {
            harfbuzz::Variation::new(harfbuzz::Tag(tag), value)
          })
          .collect();
        font.set_variations(&vars);
      }
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
    self.ensure_fallback_chain();
    for i in 0..self.fallback_chain.len() {
      let idx = self.fallback_chain[i];
      if face_has_glyph(&self.faces[idx].face, c) {
        return Ok(idx);
      }
    }
    for &idx in self.fallbacks.values() {
      if face_has_glyph(&self.faces[idx].face, c) {
        return Ok(idx);
      }
    }
    Ok(self.load_fallback(c)?.unwrap_or(styled))
  }

  /// Resolve the configured fallback families to faces once, in order. A
  /// family that fontconfig cannot resolve is logged and skipped so one bad
  /// entry does not disable the rest of the chain.
  fn ensure_fallback_chain(&mut self) {
    if self.fallback_ready {
      return;
    }
    self.fallback_ready = true;
    for i in 0..self.fallback_families.len() {
      let family = self.fallback_families[i].clone();
      match resolve_face(
        &self.library,
        &self.fontconfig,
        &family,
        Style::default(),
        self.size_px,
        &self.variations,
      ) {
        Ok(entry) => {
          self.faces.push(entry);
          self.fallback_chain.push(self.faces.len() - 1);
        },
        Err(err) => tracing::warn!("fallback font {family:?}: {err}"),
      }
    }
  }

  /// Lazily load the face for `style`, caching regular's index if the variant
  /// cannot be resolved so the lookup is not retried per glyph.
  fn styled_face(&mut self, style: Style) -> Result<usize, FontError> {
    if let Some(idx) = self.styled[style.index()] {
      return Ok(idx);
    }
    let regular = self.styled[0].ok_or(FontError::CacheInvariant)?;
    let family = self.style_family[style.index()]
      .as_deref()
      .unwrap_or(&self.family);
    let idx = match resolve_face(
      &self.library,
      &self.fontconfig,
      family,
      style,
      self.size_px,
      &self.variations,
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

/// Pack a four-character axis tag into the big-endian `u32` OpenType uses.
fn pack_tag(tag: &str) -> Option<u32> {
  let b = tag.as_bytes();
  if b.len() != 4 || !tag.is_ascii() {
    return None;
  }
  Some(
    (u32::from(b[0]) << 24)
      | (u32::from(b[1]) << 16)
      | (u32::from(b[2]) << 8)
      | u32::from(b[3]),
  )
}

/// Parse `tag=value` variation specs, skipping and logging malformed entries.
fn parse_variations(specs: &[String]) -> Vec<(u32, f32)> {
  let mut out = Vec::new();
  for spec in specs {
    let parsed = spec
      .split_once('=')
      .and_then(|(t, v)| Some((pack_tag(t.trim())?, v.trim().parse().ok()?)));
    if let Some(pair) = parsed {
      out.push(pair);
    } else {
      tracing::warn!("ignoring malformed font variation {spec:?}");
    }
  }
  out
}

/// Parse OpenType feature settings into `HarfBuzz` features applied over the
/// whole buffer. A leading `-` disables a feature (value 0), a leading `+` or a
/// bare tag enables it (value 1), and `tag=value` sets an explicit value.
/// Malformed entries are skipped and logged.
fn parse_features(specs: &[String]) -> Vec<harfbuzz::Feature> {
  let mut out = Vec::new();
  for spec in specs {
    let trimmed = spec.trim();
    // Default to enabling the bare tag; `tag=value` sets an explicit value and
    // a leading `-`/`+` disables/enables it.
    let mut tag = trimmed;
    let mut value = Some(1u32);
    if let Some((name, raw)) = trimmed.split_once('=') {
      tag = name.trim();
      value = raw.trim().parse().ok();
    } else if let Some(name) = trimmed.strip_prefix('-') {
      tag = name.trim();
      value = Some(0);
    } else if let Some(name) = trimmed.strip_prefix('+') {
      tag = name.trim();
    }
    if let (Some(tag), Some(value)) = (pack_tag(tag), value) {
      out.push(harfbuzz::Feature::new(harfbuzz::Tag(tag), value, ..));
    } else {
      tracing::warn!("ignoring malformed font feature {spec:?}");
    }
  }
  out
}

/// Set the design coordinates of a variable font. Each axis keeps its default
/// unless `variations` names it; a non-variable face is left untouched. Values
/// are clamped to the axis range.
#[expect(
  unsafe_code,
  reason = "variation axes are reachable only through FreeType's Multiple \
            Master C API, which freetype-rs does not wrap"
)]
#[expect(
  clippy::cast_possible_truncation,
  reason = "an axis value in 16.16 fixed point fits the FT_Fixed target"
)]
fn apply_variations(library: &Library, face: &Face, variations: &[(u32, f32)]) {
  if variations.is_empty() {
    return;
  }
  let face_ptr = ptr::from_ref(face.raw()).cast_mut();
  let mut mm: *mut ffi::FT_MM_Var = ptr::null_mut();
  // SAFETY: `face_ptr` is the live FT_Face backing `face`. On success
  // FT_Get_MM_Var stores a heap-allocated FT_MM_Var, released below; a
  // non-variable face returns an error and leaves `mm` null.
  if unsafe { ffi::FT_Get_MM_Var(face_ptr, &raw mut mm) } != 0 || mm.is_null() {
    return;
  }
  // SAFETY: FT_Get_MM_Var succeeded, so `*mm` is initialized and `axis` points
  // to `num_axis` valid entries.
  let (num_axis, axes) = unsafe { ((*mm).num_axis, (*mm).axis) };
  let count = num_axis as usize;
  let mut coords: Vec<ffi::FT_Fixed> = Vec::with_capacity(count);
  for i in 0..count {
    // SAFETY: `i < num_axis` indexes the axis array FreeType allocated.
    let axis = unsafe { *axes.add(i) };
    let mut value = axis.def;
    for &(tag, requested) in variations {
      if axis.tag == ffi::FT_ULong::from(tag) {
        let fixed = (f64::from(requested) * 65536.0).round() as ffi::FT_Fixed;
        value = fixed.clamp(axis.minimum, axis.maximum);
      }
    }
    coords.push(value);
  }
  // SAFETY: `coords` holds exactly `num_axis` entries for this face, and
  // `library` owns `mm`.
  unsafe {
    ffi::FT_Set_Var_Design_Coordinates(face_ptr, num_axis, coords.as_ptr());
    ffi::FT_Done_MM_Var(library.raw(), mm);
  }
}

fn resolve_face(
  library: &Library,
  fontconfig: &Fontconfig,
  family: &str,
  style: Style,
  size_px: u32,
  variations: &[(u32, f32)],
) -> Result<FaceEntry, FontError> {
  let font = fontconfig
    .find(family, Some(style.fontconfig_style()))
    .map_err(|_| FontError::NoFamily(family.to_owned()))?;
  let index = font.index.unwrap_or(0);
  let face = library
    .new_face(&font.path, isize::try_from(index).unwrap_or(isize::MAX))?;
  size_face(&face, size_px)?;
  apply_variations(library, &face, variations);
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

/// Apply the configured pixel adjustments to the measured cell geometry.
/// Width, height, and baseline stay at least one pixel; the baseline is kept
/// within the cell.
#[expect(
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "cell geometry is small and clamped to positive before casting back"
)]
fn adjust_metrics(m: CellMetrics, dw: i32, dh: i32, db: i32) -> CellMetrics {
  let width = (m.width as i32 + dw).max(1) as u32;
  let height = (m.height as i32 + dh).max(1) as u32;
  let ascent = (m.ascent as i32 + db).clamp(1, height as i32) as u32;
  CellMetrics {
    width,
    height,
    ascent,
    stroke: m.stroke,
  }
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
  hinting: Hinting,
) -> Result<Glyph, FontError> {
  let flags = load_flags(lcd, hinting);
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
  hinting: Hinting,
) -> Result<Glyph, FontError> {
  let flags = load_flags(lcd, hinting);
  rasterize_with(face, synth_bold, synth_italic, |face| {
    face.load_glyph(gid, flags)
  })
}

/// Load flags for a normal render: `TARGET_LCD` requests horizontal subpixel
/// coverage; otherwise `FreeType` renders 8-bit grayscale. `COLOR` still yields
/// a BGRA bitmap for colour glyphs regardless of the target. Hinting refines
/// grid-fitting: `None` disables it outright, `Slight` uses the light
/// autohinter (grayscale only, since LCD needs its own render target), and
/// `Normal` keeps `FreeType`'s default.
fn load_flags(lcd: bool, hinting: Hinting) -> LoadFlag {
  let mut flags = LoadFlag::RENDER | LoadFlag::COLOR;
  match hinting {
    Hinting::None => flags |= LoadFlag::NO_HINTING,
    Hinting::Slight if !lcd => flags |= LoadFlag::TARGET_LIGHT,
    Hinting::Slight | Hinting::Normal => {},
  }
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
    Fonts::new(&FontOptions::new("monospace", 16, Subpixel::None))
      .expect("system has a monospace font")
  }

  #[test]
  fn adjust_metrics_shifts_and_clamps() {
    let base = CellMetrics {
      width:  10,
      height: 20,
      ascent: 16,
      stroke: 1,
    };
    let bigger = adjust_metrics(base, 2, 4, 1);
    assert_eq!((bigger.width, bigger.height, bigger.ascent), (12, 24, 17));
    // Over-shrinking floors width/height at one pixel and keeps the baseline
    // inside the cell.
    let tiny = adjust_metrics(base, -100, -100, 100);
    assert_eq!((tiny.width, tiny.height, tiny.ascent), (1, 1, 1));
  }

  #[test]
  fn pack_tag_requires_four_ascii() {
    assert_eq!(pack_tag("wght"), Some(0x7767_6874));
    assert!(pack_tag("wg").is_none());
    assert!(pack_tag("wghtx").is_none());
    assert!(pack_tag("wgÿt").is_none());
  }

  #[test]
  fn parse_variations_reads_pairs_and_skips_garbage() {
    let specs = vec![
      "wght=550".to_string(),
      "no-equals".to_string(),
      "slnt = -8".to_string(),
    ];
    let wght = pack_tag("wght").expect("valid tag");
    let slnt = pack_tag("slnt").expect("valid tag");
    assert_eq!(parse_variations(&specs), vec![(wght, 550.0), (slnt, -8.0)]);
  }

  #[test]
  fn parse_features_reads_values_and_toggles() {
    let specs = vec![
      "ss01".to_string(),
      "-liga".to_string(),
      "+calt".to_string(),
      "cv01=2".to_string(),
      "bad".to_string(),
    ];
    let feats = parse_features(&specs);
    let seen: Vec<(String, u32)> = feats
      .iter()
      .map(|f| (f.tag().to_string(), f.value()))
      .collect();
    assert_eq!(seen, vec![
      ("ss01".to_string(), 1),
      ("liga".to_string(), 0),
      ("calt".to_string(), 1),
      ("cv01".to_string(), 2),
    ]);
  }

  #[test]
  fn shape_run_maps_glyphs_to_clusters() {
    let mut f = fonts();
    // A short ASCII run shapes to one glyph per byte, each anchored at the
    // cluster of its own cell; monospace has no ligature to merge them.
    let run = f.shape_run("ab", Style::default()).expect("run shapes");
    assert_eq!(run.glyphs.len(), 2);
    assert_eq!(run.glyphs[0].cluster, 0);
    assert_eq!(run.glyphs[1].cluster, 1);
    assert!(run.glyphs.iter().all(|g| g.gid != 0));
  }

  #[test]
  fn variations_build_is_tolerant() {
    // A face that is not variable ignores the axes; the build and a render
    // must still succeed.
    let vars = vec!["wght=600".to_string()];
    let mut opts = FontOptions::new("monospace", 16, Subpixel::None);
    opts.variations = &vars;
    let mut f = Fonts::new(&opts).expect("builds even when the face is static");
    f.glyph('a', Style::default()).expect("still renders");
  }

  #[test]
  fn hinting_and_thicken_still_render() {
    let mut opts = FontOptions::new("monospace", 16, Subpixel::None);
    opts.hinting = Hinting::Slight;
    opts.thicken = true;
    let mut f =
      Fonts::new(&opts).expect("font builds with hinting and thicken");
    let glyph = f.glyph('M', Style::default()).expect("thickened M renders");
    assert!(glyph.width > 0 && glyph.height > 0);
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
  fn unresolved_style_and_fallback_families_are_tolerated() {
    // An override family and a fallback family that fontconfig cannot resolve
    // must not break rendering: the style falls back to the primary family and
    // the bad fallback entry is skipped.
    let fallback = vec!["definitely-not-a-real-font".to_string()];
    let mut opts = FontOptions::new("monospace", 16, Subpixel::None);
    opts.bold_family = Some("definitely-not-a-real-font");
    opts.fallback = &fallback;
    let mut f = Fonts::new(&opts).expect("primary family still resolves");
    let bold = Style {
      bold:   true,
      italic: false,
    };
    let glyph = f.glyph('a', bold).expect("bold 'a' still renders");
    assert!(glyph.width > 0 && glyph.height > 0);
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
