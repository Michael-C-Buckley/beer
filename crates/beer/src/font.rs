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
use std::{collections::HashMap, fmt, fs, num::NonZeroUsize, path::PathBuf};

use fontconfig::{CharSet, Fontconfig, Pattern};
use freetype::{Face, LcdFilter, Library};
use harfbuzz_rs_now as harfbuzz;
use lru::LruCache;
use thiserror::Error;

mod raster;

use raster::{
  adjust_metrics,
  cell_metrics,
  face_has_glyph,
  is_join_control,
  parse_features,
  parse_variations,
  rasterize,
  rasterize_index,
  rasterize_scaled,
  resolve_face,
  size_face,
  synth_flags,
};

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
    let mut candidates = vec![self.face_for(base, style).ok()?];
    for character in cluster.chars().filter(|&c| !is_join_control(c)) {
      if let Ok(face_idx) = self.face_for(character, style)
        && !candidates.contains(&face_idx)
      {
        candidates.push(face_idx);
      }
    }
    for face_idx in candidates {
      if let Some(shaped) = self.shape_cluster_with_face(face_idx, cluster) {
        return Some(shaped);
      }
    }
    None
  }

  fn shape_cluster_with_face(
    &mut self,
    face_idx: usize,
    cluster: &str,
  ) -> Option<ShapedCluster> {
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

#[cfg(test)] mod tests;
