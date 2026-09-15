//! Pixel-buffer compositing primitives for the software renderer.

use std::sync::LazyLock;

use crate::{config::AlphaBlending, theme::Rgb};

/// sRGB (8-bit) → linear-light `[0, 1]` lookup, for gamma-correct compositing.
#[expect(
  clippy::cast_precision_loss,
  reason = "the fixed 8-bit sRGB lookup intentionally maps a bounded index to \
            f32"
)]
static SRGB_TO_LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
  let mut t = [0f32; 256];
  for (i, v) in t.iter_mut().enumerate() {
    *v = srgb_to_linear_f(i as f32 / 255.0);
  }
  t
});

/// sRGB transfer decode of a normalized `[0, 1]` channel to linear light.
fn srgb_to_linear_f(c: f32) -> f32 {
  if c <= 0.04045 {
    c / 12.92
  } else {
    ((c + 0.055) / 1.055).powf(2.4)
  }
}

/// sRGB transfer encode of a linear `[0, 1]` channel back to a normalized
/// float.
fn linear_to_srgb_f(c: f32) -> f32 {
  let c = c.clamp(0.0, 1.0);
  if c <= 0.003_130_8 {
    c * 12.92
  } else {
    1.055f32.mul_add(c.powf(1.0 / 2.4), -0.055)
  }
}

/// Encode a linear channel to an 8-bit sRGB value.
#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  reason = "the channel is clamped to the complete u8 range before encoding"
)]
fn linear_to_srgb(c: f32) -> u8 {
  (linear_to_srgb_f(c) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Rec. 709 relative luminance of a linear RGB triple.
fn luminance(rgb: [f32; 3]) -> f32 {
  0.0722f32.mul_add(rgb[2], 0.7152f32.mul_add(rgb[1], 0.2126 * rgb[0]))
}

/// A mutable view over a BGRA pixel buffer.
pub(super) struct Canvas<'a> {
  pub(super) pixels: &'a mut [u8],
  pub(super) width:  usize,
  pub(super) height: usize,
  /// How coverage is composited (see [`AlphaBlending`]). Set to `Native` when
  /// the destination is translucent, since linear blending needs an opaque
  /// destination.
  pub(super) blend:  AlphaBlending,
}

#[expect(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_sign_loss,
  reason = "canvas coordinates are checked or clamped before indexing the \
            pixel buffer"
)]
impl Canvas<'_> {
  const fn index(&self, x: i32, y: i32) -> Option<usize> {
    if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
      return None;
    }
    Some((y as usize * self.width + x as usize) * 4)
  }

  pub(super) fn fill_rect(&mut self, x0: i32, y0: i32, w: u32, h: u32, c: Rgb) {
    self.fill_rect_a(x0, y0, w, h, c, 0xFF);
  }

  /// Composite a premultiplied BGRA source over the pixel at `i`.
  fn over_at(&mut self, i: usize, src: [u8; 4]) {
    let inverse = u32::from(255 - src[3]);
    let over = |source: u8, destination: u8| {
      (u32::from(source) + u32::from(destination) * inverse / 255).min(255)
        as u8
    };
    self.pixels[i] = over(src[0], self.pixels[i]);
    self.pixels[i + 1] = over(src[1], self.pixels[i + 1]);
    self.pixels[i + 2] = over(src[2], self.pixels[i + 2]);
    self.pixels[i + 3] = over(src[3], self.pixels[i + 3]);
  }

  /// Fill a rectangle with colour `c` at opacity `alpha`. The shm buffer is
  /// premultiplied ARGB, so a translucent fill stores `rgb * alpha`.
  pub(super) fn fill_rect_a(
    &mut self,
    x0: i32,
    y0: i32,
    w: u32,
    h: u32,
    c: Rgb,
    alpha: u8,
  ) {
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
      let row = &mut self.pixels
        [(y * self.width + x_start) * 4..(y * self.width + x_end) * 4];
      for px in row.chunks_exact_mut(4) {
        px.copy_from_slice(&bytes);
      }
    }
  }

  /// Alpha-blend `fg` over the existing pixel with coverage `a`, in the
  /// configured [`AlphaBlending`] space. The buffer is BGRA: index 0 is blue,
  /// index 2 is red.
  pub(super) fn blend(
    &mut self,
    pixel_x: i32,
    pixel_y: i32,
    fg: Rgb,
    coverage: u8,
  ) {
    let Some(pixel_index) = self.index(pixel_x, pixel_y) else {
      return;
    };
    match self.blend {
      AlphaBlending::Native => {
        let a = u32::from(coverage);
        let premultiply = |channel: u8| (u32::from(channel) * a / 255) as u8;
        self.over_at(pixel_index, [
          premultiply(fg.2),
          premultiply(fg.1),
          premultiply(fg.0),
          coverage,
        ]);
      },
      AlphaBlending::Linear | AlphaBlending::LinearCorrected => {
        let lut = &*SRGB_TO_LINEAR;
        // Foreground and destination in linear light.
        let foreground =
          [lut[fg.0 as usize], lut[fg.1 as usize], lut[fg.2 as usize]];
        let destination = [
          lut[self.pixels[pixel_index + 2] as usize],
          lut[self.pixels[pixel_index + 1] as usize],
          lut[self.pixels[pixel_index] as usize],
        ];
        let cov = f32::from(coverage) / 255.0;
        // Linear-corrected remaps the coverage so the blended luminance matches
        // what gamma-space (Native) blending would give, preserving perceived
        // stroke weight while keeping colour edges clean.
        let alpha = if self.blend == AlphaBlending::LinearCorrected {
          let foreground_luminance = luminance(foreground);
          let background_luminance = luminance(destination);
          if (foreground_luminance - background_luminance).abs() < 1e-6 {
            cov
          } else {
            let target =
              srgb_to_linear_f(linear_to_srgb_f(background_luminance).mul_add(
                1.0 - cov,
                linear_to_srgb_f(foreground_luminance) * cov,
              ));
            ((target - background_luminance)
              / (foreground_luminance - background_luminance))
              .clamp(0.0, 1.0)
          }
        } else {
          cov
        };
        let out = |fc: f32, dc: f32| {
          linear_to_srgb(dc.mul_add(1.0 - alpha, fc * alpha))
        };
        self.pixels[pixel_index] = out(foreground[2], destination[2]);
        self.pixels[pixel_index + 1] = out(foreground[1], destination[1]);
        self.pixels[pixel_index + 2] = out(foreground[0], destination[0]);
      },
    }
    if self.blend != AlphaBlending::Native {
      self.pixels[pixel_index + 3] = 0xFF;
    }
  }

  /// Alpha-blend `fg` over the destination with independent per-subpixel
  /// coverage (LCD text). `cov` is in `FreeType`'s physical order (leftmost,
  /// middle, rightmost subpixel); `bgr` swaps the outer channels for panels
  /// whose subpixels run blue-green-red rather than red-green-blue.
  pub(super) fn blend_lcd(
    &mut self,
    x: i32,
    y: i32,
    fg: Rgb,
    cov: [u8; 3],
    bgr: bool,
  ) {
    let Some(i) = self.index(x, y) else { return };
    let (ar, ag, ab) = if bgr {
      (u32::from(cov[2]), u32::from(cov[1]), u32::from(cov[0]))
    } else {
      (u32::from(cov[0]), u32::from(cov[1]), u32::from(cov[2]))
    };
    let mix = |src: u8, dst: u8, a: u32| {
      ((u32::from(src) * a + u32::from(dst) * (255 - a)) / 255) as u8
    };
    // The shm buffer is BGRA: index 0 is blue, 2 is red.
    self.pixels[i] = mix(fg.2, self.pixels[i], ab);
    self.pixels[i + 1] = mix(fg.1, self.pixels[i + 1], ag);
    self.pixels[i + 2] = mix(fg.0, self.pixels[i + 2], ar);
    self.pixels[i + 3] = 0xFF;
  }

  /// Set a single opaque pixel.
  pub(super) fn put(&mut self, x: i32, y: i32, c: Rgb) {
    if let Some(i) = self.index(x, y) {
      self.pixels[i] = c.2;
      self.pixels[i + 1] = c.1;
      self.pixels[i + 2] = c.0;
      self.pixels[i + 3] = 0xFF;
    }
  }

  pub(super) fn hline(&mut self, x0: i32, y: i32, w: u32, c: Rgb) {
    self.fill_rect(x0, y, w, 1, c);
  }

  /// Composite one straight-alpha RGBA source pixel over the destination.
  pub(super) fn blend_rgba(&mut self, x: i32, y: i32, rgba: [u8; 4]) {
    let Some(i) = self.index(x, y) else { return };
    let a = u32::from(rgba[3]);
    let premultiply = |channel: u8| (u32::from(channel) * a / 255) as u8;
    self.over_at(i, [
      premultiply(rgba[2]),
      premultiply(rgba[1]),
      premultiply(rgba[0]),
      rgba[3],
    ]);
  }

  /// Composite one pre-multiplied BGRA source pixel over the destination.
  pub(super) fn over(&mut self, x: i32, y: i32, src: &[u8]) {
    let Some(i) = self.index(x, y) else { return };
    self.over_at(i, [src[0], src[1], src[2], src[3]]);
  }
}
