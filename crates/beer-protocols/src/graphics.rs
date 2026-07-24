//! The kitty terminal graphics protocol: parsing the control data of a graphics
//! command.
//!
//! A graphics command arrives as an Application Programming Command (APC):
//! `ESC _ G <control data> ; <payload> ESC \`. The control data is a
//! comma-separated list of `key=value` pairs; the payload is base64-encoded
//! binary. This module turns the control data into a typed [`GraphicsCommand`].
//! Capturing the APC from the byte stream, accumulating the payload across
//! chunks, decoding pixels, and storing/displaying images all live in the
//! terminal (`vte` does not surface APC, and those steps need terminal state);
//! this module is the pure, testable parse step.
//!
//! Several keys are overloaded by action: `x/y/w/h` are a source rectangle when
//! displaying but a destination rectangle for animation frames; `X/Y/z/c/r`
//! likewise change meaning between display, frame transmission, and frame
//! composition. The parser stores the raw value under one field and the engine
//! interprets it by [`Action`]; the field docs note each reuse.

/// The overall action of a graphics command (`a` key).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Action {
  /// `a=t`: transmit image data and store it.
  #[default]
  Transmit,
  /// `a=T`: transmit and immediately display.
  TransmitAndDisplay,
  /// `a=q`: query support; load but neither store nor display.
  Query,
  /// `a=p`: display (put) an already-transmitted image.
  Put,
  /// `a=d`: delete images/placements.
  Delete,
  /// `a=f`: transmit animation frame data.
  Frame,
  /// `a=a`: control animation playback.
  Animate,
  /// `a=c`: compose animation frames.
  Compose,
}

/// Pixel format of transmitted data (`f` key).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
  /// `f=24`: 24-bit RGB, three bytes per pixel.
  Rgb,
  /// `f=32` (default): 32-bit RGBA, four bytes per pixel.
  #[default]
  Rgba,
  /// `f=100`: a PNG image; dimensions are read from the data.
  Png,
}

/// How the pixel data reaches the terminal (`t` key).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Medium {
  /// `t=d` (default): inline in the escape code payload, base64, chunked.
  #[default]
  Direct,
  /// `t=f`: a regular file at the path given in the payload.
  File,
  /// `t=t`: a temporary file the terminal deletes after reading.
  TempFile,
  /// `t=s`: a POSIX shared-memory object, unlinked after reading.
  SharedMemory,
}

/// Parsed control data of one graphics command. Every field's protocol default
/// is its type's default (zero, `false`, or the `#[default]` enum variant), so
/// an absent key simply leaves the default in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct GraphicsCommand {
  /// `a`: the action.
  pub action:            Action,
  /// `q`: response suppression (0 = all, 1 = errors only off too, 2 = quiet).
  pub quiet:             u8,
  /// `f`: transmitted pixel format.
  pub format:            Format,
  /// `t`: transmission medium.
  pub medium:            Medium,
  /// `s`: image width in pixels (RGB/RGBA), or a frame source-rect width.
  pub width:             u32,
  /// `v`: image height in pixels (RGB/RGBA), or a frame source-rect height.
  pub height:            u32,
  /// `S`: number of bytes to read from a file/shared-memory object.
  pub read_size:         u32,
  /// `O`: byte offset to start reading a file/shared-memory object from.
  pub read_offset:       u32,
  /// `i`: image id (`1..=u32::MAX`; 0 means unset).
  pub id:                u32,
  /// `I`: image number, an alternative client-side handle.
  pub number:            u32,
  /// `p`: placement id.
  pub placement:         u32,
  /// `o`: whether the payload is zlib-compressed (`o=z`).
  pub compressed:        bool,
  /// `m`: whether more chunks follow (`m=1`); the last chunk has `m=0`.
  pub more:              bool,
  /// `x`: source-rect left (display) or frame destination left, in pixels.
  pub x:                 u32,
  /// `y`: source-rect top (display) or frame destination top, in pixels.
  pub y:                 u32,
  /// `w`: source-rect width (display) or frame destination width, in pixels.
  pub w:                 u32,
  /// `h`: source-rect height (display) or frame destination height, in pixels.
  pub h:                 u32,
  /// `c`: columns to display in, or (compose) the overlaid frame number, or
  /// (frame transmit) the base frame number.
  pub c:                 u32,
  /// `r`: rows to display in, or the frame number being edited.
  pub r:                 u32,
  /// `X`: cell x pixel offset (display) or composition mode (frame: 1 =
  /// overwrite, else alpha blend).
  pub cap_x:             u32,
  /// `Y`: cell y pixel offset (display) or background RGBA (frame).
  pub cap_y:             u32,
  /// `z`: z-index / vertical stacking order (display) or frame gap in ms.
  pub z:                 i32,
  /// `C`: cursor-movement policy on display (`C=1` leaves the cursor put).
  pub cursor_policy:     u8,
  /// `U`: create a virtual placement for a Unicode placeholder (`U=1`).
  pub virtual_placement: bool,
  /// `P`: parent image id for a relative placement.
  pub parent_id:         u32,
  /// `Q`: parent placement id for a relative placement.
  pub parent_placement:  u32,
  /// `H`: relative-placement horizontal cell offset (signed).
  pub rel_h:             i32,
  /// `V`: relative-placement vertical cell offset (signed).
  pub rel_v:             i32,
  /// `d`: delete target, the raw key char (0 when absent). Uppercase variants
  /// also free the stored image data; the engine reads the case.
  pub delete:            u8,
}

impl GraphicsCommand {
  /// Whether a `d`-key delete frees the stored image data (uppercase variant).
  #[must_use]
  pub const fn delete_frees_data(&self) -> bool {
    self.delete.is_ascii_uppercase()
  }
}

/// The Unicode placeholder character. A cell holding this code point displays a
/// slice of an image, with the row/column carried by combining diacritics and
/// the image id by the cell's foreground colour (see [`diacritic_value`]). This
/// lets images survive multiplexers and editors that only forward text.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// The ordered combining marks the placeholder protocol uses to encode a row,
/// column, or high image-id byte: the value is the mark's index in this list.
/// Taken verbatim from kitty's `rowcolumn-diacritics.txt`.
#[rustfmt::skip]
const DIACRITICS: [u32; 297] = [
    0x305, 0x30D, 0x30E, 0x310, 0x312, 0x33D, 0x33E, 0x33F, 0x346, 0x34A, 0x34B, 0x34C,
    0x350, 0x351, 0x352, 0x357, 0x35B, 0x363, 0x364, 0x365, 0x366, 0x367, 0x368, 0x369,
    0x36A, 0x36B, 0x36C, 0x36D, 0x36E, 0x36F, 0x483, 0x484, 0x485, 0x486, 0x487, 0x592,
    0x593, 0x594, 0x595, 0x597, 0x598, 0x599, 0x59C, 0x59D, 0x59E, 0x59F, 0x5A0, 0x5A1,
    0x5A8, 0x5A9, 0x5AB, 0x5AC, 0x5AF, 0x5C4, 0x610, 0x611, 0x612, 0x613, 0x614, 0x615,
    0x616, 0x617, 0x657, 0x658, 0x659, 0x65A, 0x65B, 0x65D, 0x65E, 0x6D6, 0x6D7, 0x6D8,
    0x6D9, 0x6DA, 0x6DB, 0x6DC, 0x6DF, 0x6E0, 0x6E1, 0x6E2, 0x6E4, 0x6E7, 0x6E8, 0x6EB,
    0x6EC, 0x730, 0x732, 0x733, 0x735, 0x736, 0x73A, 0x73D, 0x73F, 0x740, 0x741, 0x743,
    0x745, 0x747, 0x749, 0x74A, 0x7EB, 0x7EC, 0x7ED, 0x7EE, 0x7EF, 0x7F0, 0x7F1, 0x7F3,
    0x816, 0x817, 0x818, 0x819, 0x81B, 0x81C, 0x81D, 0x81E, 0x81F, 0x820, 0x821, 0x822,
    0x823, 0x825, 0x826, 0x827, 0x829, 0x82A, 0x82B, 0x82C, 0x82D, 0x951, 0x953, 0x954,
    0xF82, 0xF83, 0xF86, 0xF87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187,
    0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

/// The value a placeholder diacritic encodes (its position in the table), or
/// `None` if `c` is not one of the protocol's row/column diacritics.
#[must_use]
pub fn diacritic_value(c: char) -> Option<u32> {
  let cp = c as u32;
  DIACRITICS
    .iter()
    .position(|&d| d == cp)
    .and_then(|i| u32::try_from(i).ok())
}

/// Parse the control-data field of a graphics command (everything between
/// `ESC _ G` and the `;` that precedes the payload). Unknown keys and malformed
/// pairs are ignored; an empty field yields the default command.
#[must_use]
pub fn parse(control: &[u8]) -> GraphicsCommand {
  let mut cmd = GraphicsCommand::default();
  for pair in control.split(|&b| b == b',') {
    let Some(eq) = pair.iter().position(|&b| b == b'=') else {
      continue;
    };
    let (key, value) = pair.split_at(eq);
    let [key] = key else { continue };
    let value = &value[1..];
    let ch = value.first().copied();
    match key {
      b'a' => {
        cmd.action = match ch {
          Some(b'T') => Action::TransmitAndDisplay,
          Some(b'q') => Action::Query,
          Some(b'p') => Action::Put,
          Some(b'd') => Action::Delete,
          Some(b'f') => Action::Frame,
          Some(b'a') => Action::Animate,
          Some(b'c') => Action::Compose,
          _ => Action::Transmit,
        }
      },
      b'f' => {
        cmd.format = match parse_u32(value) {
          Some(24) => Format::Rgb,
          Some(100) => Format::Png,
          _ => Format::Rgba,
        }
      },
      b't' => {
        cmd.medium = match ch {
          Some(b'f') => Medium::File,
          Some(b't') => Medium::TempFile,
          Some(b's') => Medium::SharedMemory,
          _ => Medium::Direct,
        }
      },
      b'o' => cmd.compressed = ch == Some(b'z'),
      b'm' => cmd.more = parse_u32(value) == Some(1),
      b'q' => set_u8(&mut cmd.quiet, value),
      b'C' => set_u8(&mut cmd.cursor_policy, value),
      b'U' => cmd.virtual_placement = parse_u32(value) == Some(1),
      b'd' => cmd.delete = ch.unwrap_or(0),
      b's' => set_u32(&mut cmd.width, value),
      b'v' => set_u32(&mut cmd.height, value),
      b'S' => set_u32(&mut cmd.read_size, value),
      b'O' => set_u32(&mut cmd.read_offset, value),
      b'i' => set_u32(&mut cmd.id, value),
      b'I' => set_u32(&mut cmd.number, value),
      b'p' => set_u32(&mut cmd.placement, value),
      b'x' => set_u32(&mut cmd.x, value),
      b'y' => set_u32(&mut cmd.y, value),
      b'w' => set_u32(&mut cmd.w, value),
      b'h' => set_u32(&mut cmd.h, value),
      b'c' => set_u32(&mut cmd.c, value),
      b'r' => set_u32(&mut cmd.r, value),
      b'X' => set_u32(&mut cmd.cap_x, value),
      b'Y' => set_u32(&mut cmd.cap_y, value),
      b'P' => set_u32(&mut cmd.parent_id, value),
      b'Q' => set_u32(&mut cmd.parent_placement, value),
      b'z' => set_i32(&mut cmd.z, value),
      b'H' => set_i32(&mut cmd.rel_h, value),
      b'V' => set_i32(&mut cmd.rel_v, value),
      _ => {},
    }
  }
  cmd
}

fn set_u32(slot: &mut u32, value: &[u8]) {
  if let Some(v) = parse_u32(value) {
    *slot = v;
  }
}

fn set_u8(slot: &mut u8, value: &[u8]) {
  if let Some(v) = parse_u32(value) {
    *slot = u8::try_from(v.min(u32::from(u8::MAX))).unwrap_or(u8::MAX);
  }
}

fn set_i32(slot: &mut i32, value: &[u8]) {
  if let Some(v) = parse_i32(value) {
    *slot = v;
  }
}

/// Parse ASCII digits into a `u32`, saturating. `None` if empty or non-digit.
fn parse_u32(bytes: &[u8]) -> Option<u32> {
  if bytes.is_empty() {
    return None;
  }
  let mut acc: u32 = 0;
  for &b in bytes {
    let d = b.checked_sub(b'0').filter(|&d| d < 10)?;
    acc = acc.saturating_mul(10).saturating_add(u32::from(d));
  }
  Some(acc)
}

/// Parse an optionally-signed decimal into an `i32`, saturating.
fn parse_i32(bytes: &[u8]) -> Option<i32> {
  let (neg, digits) = match bytes.split_first() {
    Some((b'-', rest)) => (true, rest),
    _ => (false, bytes),
  };
  let mag = parse_u32(digits)?;
  Some(if neg {
    i32::try_from(-i64::from(
      mag.min(u32::try_from(i32::MAX).unwrap_or(u32::MAX) + 1),
    ))
    .unwrap_or(i32::MIN)
  } else {
    i32::try_from(mag.min(u32::try_from(i32::MAX).unwrap_or(u32::MAX)))
      .unwrap_or(i32::MAX)
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_is_default_transmit() {
    let c = parse(b"");
    assert_eq!(c, GraphicsCommand::default());
    assert_eq!(c.action, Action::Transmit);
    assert_eq!(c.format, Format::Rgba);
    assert_eq!(c.medium, Medium::Direct);
  }

  #[test]
  fn transmit_and_display_rgb_with_dimensions() {
    let c = parse(b"a=T,f=24,s=10,v=20,i=3");
    assert_eq!(c.action, Action::TransmitAndDisplay);
    assert_eq!(c.format, Format::Rgb);
    assert_eq!((c.width, c.height), (10, 20));
    assert_eq!(c.id, 3);
  }

  #[test]
  fn chunked_direct_transmission_flags() {
    let first = parse(b"a=t,f=100,m=1");
    assert!(first.more);
    assert_eq!(first.format, Format::Png);
    let last = parse(b"m=0");
    assert!(!last.more);
  }

  #[test]
  fn file_medium_with_size_and_offset() {
    let c = parse(b"t=t,S=4096,O=128,i=7");
    assert_eq!(c.medium, Medium::TempFile);
    assert_eq!((c.read_size, c.read_offset), (4096, 128));
  }

  #[test]
  fn compression_flag() {
    assert!(parse(b"o=z").compressed);
    assert!(!parse(b"o=0").compressed);
  }

  #[test]
  fn display_layout_keys_and_negative_z() {
    let c = parse(b"a=p,c=4,r=2,X=3,Y=5,z=-1,C=1");
    assert_eq!(c.action, Action::Put);
    assert_eq!((c.c, c.r), (4, 2));
    assert_eq!((c.cap_x, c.cap_y), (3, 5));
    assert_eq!(c.z, -1);
    assert_eq!(c.cursor_policy, 1);
  }

  #[test]
  fn delete_case_controls_freeing() {
    let keep = parse(b"a=d,d=i,i=9");
    assert_eq!(keep.delete, b'i');
    assert!(!keep.delete_frees_data());
    let free = parse(b"a=d,d=I,i=9");
    assert!(free.delete_frees_data());
  }

  #[test]
  fn relative_placement_and_placeholder() {
    let c = parse(b"a=p,U=1,P=2,Q=3,H=-4,V=5");
    assert!(c.virtual_placement);
    assert_eq!((c.parent_id, c.parent_placement), (2, 3));
    assert_eq!((c.rel_h, c.rel_v), (-4, 5));
  }

  #[test]
  fn unknown_and_malformed_keys_ignored() {
    let c = parse(b"a=t,zz=9,bad,=5,s=8");
    assert_eq!(c.action, Action::Transmit);
    assert_eq!(c.width, 8);
  }

  #[test]
  fn placeholder_diacritics_encode_positions() {
    // The first two table entries are values 0 and 1 (the doc's examples).
    assert_eq!(diacritic_value('\u{0305}'), Some(0));
    assert_eq!(diacritic_value('\u{030D}'), Some(1));
    // The last entry is value 296.
    assert_eq!(diacritic_value('\u{1D244}'), Some(296));
    // A non-diacritic is rejected.
    assert_eq!(diacritic_value('a'), None);
    assert_eq!(DIACRITICS.len(), 297);
  }
}
