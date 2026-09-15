//! Face resolution, metric calculation, and glyph rasterization.

use std::ptr;

use fontconfig::Fontconfig;
use freetype::{
  Face,
  Library,
  Matrix,
  Vector,
  bitmap::PixelMode,
  face::{LoadFlag, StyleFlag},
  ffi,
};
use harfbuzz_rs_now as harfbuzz;

use super::{CellMetrics, FaceEntry, FontError, Glyph, GlyphData, Style};
use crate::config::Hinting;
pub(super) const fn is_join_control(c: char) -> bool {
  matches!(c as u32, 0x200C | 0x200D | 0xFE0E | 0xFE0F)
}

pub(super) fn face_has_glyph(face: &Face, c: char) -> bool {
  face
    .get_char_index(usize::try_from(u32::from(c)).unwrap_or(usize::MAX))
    .is_some_and(|g| g != 0)
}

/// Whether bold/italic must be synthesized: only when the requested style is
/// set but the resolved face lacks the real variant.
pub(super) fn synth_flags(face: &Face, style: Style) -> (bool, bool) {
  let flags = face.style_flags();
  let synth_bold = style.bold && !flags.contains(StyleFlag::BOLD);
  let synth_italic = style.italic && !flags.contains(StyleFlag::ITALIC);
  (synth_bold, synth_italic)
}

/// Pack a four-character axis tag into the big-endian `u32` OpenType uses.
pub(super) fn pack_tag(tag: &str) -> Option<u32> {
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
pub(super) fn parse_variations(specs: &[String]) -> Vec<(u32, f32)> {
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
pub(super) fn parse_features(specs: &[String]) -> Vec<harfbuzz::Feature> {
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
pub(super) fn apply_variations(
  library: &Library,
  face: &Face,
  variations: &[(u32, f32)],
) {
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

pub(super) fn resolve_face(
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
pub(super) fn size_face(face: &Face, size_px: u32) -> Result<(), FontError> {
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
pub(super) fn nearest_strike(face: &Face, target: u32) -> i32 {
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
pub(super) fn adjust_metrics(
  m: CellMetrics,
  dw: i32,
  dh: i32,
  db: i32,
) -> CellMetrics {
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

pub(super) fn cell_metrics(
  face: &Face,
  family: &str,
) -> Result<CellMetrics, FontError> {
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

pub(super) fn rasterize(
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
pub(super) fn rasterize_index(
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
pub(super) fn load_flags(lcd: bool, hinting: Hinting) -> LoadFlag {
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
pub(super) fn rasterize_scaled(
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

pub(super) fn rasterize_with(
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

pub(super) const fn shear_matrix() -> Matrix {
  // ~0.2 horizontal shear in 16.16 fixed point.
  Matrix {
    xx: 0x1_0000,
    xy: 0x3333,
    yx: 0,
    yy: 0x1_0000,
  }
}

pub(super) const fn identity_matrix() -> Matrix {
  Matrix {
    xx: 0x1_0000,
    xy: 0,
    yx: 0,
    yy: 0x1_0000,
  }
}

/// Widen each row's coverage by one pixel (synthetic bold).
pub(super) fn embolden(mask: &mut [u8], width: usize, height: usize) {
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
pub(super) fn embolden_lcd(sub: &mut [u8], width: usize, height: usize) {
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
pub(super) fn pack_rows(
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
pub(super) fn expand_mono(
  src: &[u8],
  width: usize,
  pitch: i32,
  height: usize,
) -> Vec<u8> {
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
