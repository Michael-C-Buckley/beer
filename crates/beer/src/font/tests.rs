use super::{
  raster::{embolden, pack_tag},
  *,
};

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
  let mut f = Fonts::new(&opts).expect("font builds with hinting and thicken");
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
