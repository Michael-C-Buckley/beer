use super::{
  canvas::Canvas,
  geometry::{
    box_arms,
    braille_geometry,
    draw_geometric,
    is_box_draw,
    is_geometric,
    sextant_pattern,
  },
};
use crate::{config::AlphaBlending, font::CellMetrics, theme::Rgb};

/// Render one box glyph into a fresh `size`x`size` buffer and return a
/// predicate for whether a given pixel received ink.
#[expect(
  clippy::cast_sign_loss,
  reason = "render tests call this helper only with positive cell sizes and \
            coordinates"
)]
fn render_glyph(c: char, size: i32) -> impl Fn(i32, i32) -> bool {
  let n = size as usize;
  let mut buf = vec![0u8; n * n * 4];
  let mut canvas = Canvas {
    pixels: &mut buf,
    width:  n,
    height: n,
    blend:  AlphaBlending::Native,
  };
  let m = CellMetrics {
    width:  size as u32,
    height: size as u32,
    ascent: (size * 3 / 4) as u32,
    stroke: (size / 8).max(1) as u32,
  };
  assert!(draw_geometric(&mut canvas, c, 0, 0, m, Rgb(255, 255, 255)));
  move |x: i32, y: i32| buf[((y * size + x) * 4) as usize] != 0
}

// Pinned to foot box-drawing.c draw_braille output (cross-checked numerically
// identical across cell sizes 4..30 x 6..48); guards against drift.
#[test]
fn braille_geometry_matches_foot() {
  assert_eq!(braille_geometry(8, 18), (2, [1, 5], [2, 6, 10, 14]));
  assert_eq!(braille_geometry(10, 20), (2, [1, 6], [1, 6, 11, 16]));
  assert_eq!(braille_geometry(12, 27), (3, [1, 8], [1, 8, 15, 22]));
  assert_eq!(braille_geometry(7, 15), (1, [1, 4], [2, 5, 8, 11]));
}

#[test]
fn box_draw_range_membership() {
  assert!(is_box_draw('\u{2500}')); // light horizontal
  assert!(is_box_draw('\u{2588}')); // full block
  assert!(is_box_draw('\u{259F}')); // quadrant
  assert!(is_box_draw('\u{1FB00}')); // first sextant
  assert!(is_box_draw('\u{1FB3B}')); // last sextant
  assert!(!is_box_draw('\u{1FB3C}')); // wedge, not handled
  assert!(!is_box_draw('A'));
  assert!(is_geometric('\u{E0B0}'));
  assert!(is_geometric('\u{E0B7}'));
}

#[test]
fn powerline_separators_reach_cell_edges() {
  let right = render_glyph('\u{E0B0}', 16);
  assert!(right(0, 0));
  assert!(right(15, 8));
  let left = render_glyph('\u{E0B2}', 16);
  assert!(left(15, 0));
  assert!(left(0, 8));
}

#[test]
fn powerline_slants_tile_and_antialias() {
  for (width, height) in [(9, 21), (10, 20), (1, 1)] {
    let mut buffers = Vec::new();
    for c in ['\u{e0b8}', '\u{e0ba}', '\u{e0bc}', '\u{e0be}'] {
      assert!(is_geometric(c));
      let mut pixels = vec![0; width * height * 4];
      let mut canvas = Canvas {
        pixels: &mut pixels,
        width,
        height,
        blend: AlphaBlending::Native,
      };
      let metrics = CellMetrics {
        width:  u32::try_from(width).unwrap(),
        height: u32::try_from(height).unwrap(),
        ascent: 0,
        stroke: 1,
      };
      assert!(draw_geometric(
        &mut canvas,
        c,
        0,
        0,
        metrics,
        Rgb(255, 255, 255)
      ));
      assert!(pixels.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 255));
      // White over transparent must retain coverage in every channel.
      assert!(pixels.chunks_exact(4).all(|p| p == [p[3]; 4]));
      buffers.push(pixels);
    }
    // Opposite triangles cover the entire cell without gaps or overlaps.
    for (a, b) in [(0, 3), (1, 2)] {
      for (left, right) in buffers[a].iter().zip(&buffers[b]) {
        assert!((254..=256).contains(&(u16::from(*left) + u16::from(*right))));
      }
    }
    assert_eq!(
      buffers[0],
      buffers[3].iter().rev().copied().collect::<Vec<_>>()
    );
    // The upper-left slant joins a solid segment along its top edge;
    // only the corner pixel intersects the diagonal.
    assert!(buffers[2][..(width - 1) * 4].iter().all(|&v| v == 255));
    assert!(buffers[0][4..width * 4].iter().all(|&v| v == 0));
  }
}

#[test]
fn box_arms_weights() {
  // Light cross: every arm light. Heavy cross: every arm heavy.
  assert_eq!(box_arms(0x253C), Some([1, 1, 1, 1]));
  assert_eq!(box_arms(0x254B), Some([2, 2, 2, 2]));
  // Double horizontal/vertical and the double cross.
  assert_eq!(box_arms(0x2550), Some([0, 0, 3, 3]));
  assert_eq!(box_arms(0x2551), Some([3, 3, 0, 0]));
  assert_eq!(box_arms(0x256C), Some([3, 3, 3, 3]));
  // A light down-and-right corner is down + right only.
  assert_eq!(box_arms(0x250C), Some([0, 1, 0, 1]));
  // Dashes/arcs/diagonals are handled elsewhere, not here.
  assert_eq!(box_arms(0x2504), None);
  assert_eq!(box_arms(0x256D), None);
}

#[test]
fn sextant_pattern_skips_half_blocks() {
  // The enumeration runs 1..=62 skipping the left-half (21) and right-half
  // (42) bit patterns, so the range endpoints map to 1 and 62.
  assert_eq!(sextant_pattern(0x1FB00), 1);
  assert_eq!(sextant_pattern(0x1FB3B), 62);
  // No codepoint in the range produces a skipped pattern.
  for cp in 0x1FB00..=0x1FB3B {
    let p = sextant_pattern(cp);
    assert!(p != 21 && p != 42 && p != 0 && p != 63);
  }
}

#[test]
fn double_corner_closes_and_stays_hollow() {
  // U+2554 ╔ (double down-and-right). thin = 16/8 = 2, d = 3, centre = 8.
  let ink = render_glyph('\u{2554}', 16);
  // The outer corner where the two outer rails meet is inked...
  assert!(ink(5, 5), "double corner should close at the outer rails");
  // ...while the centre of the corner box stays hollow.
  assert!(!ink(8, 8), "the double corner's interior should be hollow");
  // The arms reach their edges (right arm at the top rail, down arm's left
  // rail near the bottom).
  assert!(ink(15, 5), "top rail should reach the right edge");
  assert!(ink(5, 15), "left rail should reach the bottom edge");
}

#[test]
fn octant_table_and_geometry() {
  // The table is the full octant block, endpoints as transcribed from Kitty.
  assert_eq!(super::geometry::OCTANTS.len(), 230);
  assert_eq!(super::geometry::OCTANTS[0], 0x02);
  assert_eq!(*super::geometry::OCTANTS.last().unwrap(), 0xFE);
  // U+1CD00 → 0x02 = left column, row 1 only (rows are quarter-cells).
  let ink = render_glyph('\u{1CD00}', 16);
  assert!(ink(2, 5), "left column row 1 should be filled"); // row1 = [4,8)
  assert!(!ink(2, 1), "row 0 should be empty");
  assert!(!ink(10, 5), "right column should be empty");
}

#[test]
fn blend_endpoints_hold_in_all_modes() {
  for mode in [
    AlphaBlending::Native,
    AlphaBlending::Linear,
    AlphaBlending::LinearCorrected,
  ] {
    // Zero coverage leaves the (black) background untouched.
    let mut buf = vec![0u8; 4];
    let mut c = Canvas {
      pixels: &mut buf,
      width:  1,
      height: 1,
      blend:  mode,
    };
    c.blend(0, 0, Rgb(255, 255, 255), 0);
    assert_eq!(buf[2], 0, "{mode:?}: zero coverage keeps the background");
    // Full coverage paints the foreground.
    let mut buf = vec![0u8; 4];
    let mut c = Canvas {
      pixels: &mut buf,
      width:  1,
      height: 1,
      blend:  mode,
    };
    c.blend(0, 0, Rgb(255, 255, 255), 255);
    assert!(
      buf[2] >= 254,
      "{mode:?}: full coverage paints the foreground"
    );
  }
}

#[test]
fn native_blend_preserves_translucent_destination_alpha() {
  // The buffer stores premultiplied BGRA. Half-covered white over a
  // half-transparent background must remain translucent; forcing alpha to
  // 255 creates the dark fringe seen around antialiased text.
  let mut buf = vec![30, 40, 50, 128];
  let mut canvas = Canvas {
    pixels: &mut buf,
    width:  1,
    height: 1,
    blend:  AlphaBlending::Native,
  };
  canvas.blend(0, 0, Rgb(255, 255, 255), 128);
  assert_eq!(buf, [142, 147, 152, 191]);
}

#[test]
fn rgba_blend_uses_premultiplied_source_over() {
  let mut buf = vec![10, 20, 30, 64];
  let mut canvas = Canvas {
    pixels: &mut buf,
    width:  1,
    height: 1,
    blend:  AlphaBlending::Native,
  };
  canvas.blend_rgba(0, 0, [200, 100, 50, 128]);
  assert_eq!(buf, [29, 59, 114, 159]);
}

#[test]
fn diagonal_is_antialiased() {
  // A hard staircase would ink pixels fully or not at all; antialiasing
  // leaves edge pixels at partial coverage. Render U+2571 (╱) and confirm at
  // least one pixel is partially covered (white fg → blue byte in 1..255).
  let n = 24usize;
  let mut buf = vec![0u8; n * n * 4];
  let mut canvas = Canvas {
    pixels: &mut buf,
    width:  n,
    height: n,
    blend:  AlphaBlending::Native,
  };
  let m = CellMetrics {
    width:  u32::try_from(n).unwrap_or(u32::MAX),
    height: u32::try_from(n).unwrap_or(u32::MAX),
    ascent: 18,
    stroke: 2,
  };
  assert!(draw_geometric(
    &mut canvas,
    '\u{2571}',
    0,
    0,
    m,
    Rgb(255, 255, 255)
  ));
  let partial = buf.iter().step_by(4).any(|&b| b > 0 && b < 255);
  assert!(
    partial,
    "diagonal should have antialiased (partial) coverage"
  );
}

#[test]
fn single_cross_and_corner_geometry() {
  // A light cross inks its centre; a top-left corner leaves the top-left
  // pixel blank (arms only run down and right from the centre).
  let cross = render_glyph('\u{253C}', 16); // ┼
  assert!(cross(8, 8), "cross centre should be inked");
  let corner = render_glyph('\u{250C}', 16); // ┌
  assert!(corner(8, 8), "corner centre should be inked");
  assert!(!corner(0, 0), "corner should not ink the opposite quadrant");
}
