//! Image decoding, source loading, compositing, and graphics replies.

use std::{
  env,
  fs::{self, File},
  io::{self, BufReader, Cursor, Read, Seek, SeekFrom},
  str,
};

use beer_protocols::graphics::{Format, GraphicsCommand, Medium};
use flate2::read::ZlibDecoder;

use super::{MAX_IMAGE_BYTES, MAX_SOURCE_BYTES, Outcome, Pixels};

pub(super) fn decode(
  cmd: &GraphicsCommand,
  bytes: Vec<u8>,
) -> Result<Pixels, String> {
  match cmd.format {
    Format::Png => {
      let mut reader = image::ImageReader::with_format(
        BufReader::new(Cursor::new(bytes)),
        image::ImageFormat::Png,
      );
      let mut limits = image::Limits::default();
      limits.max_image_width =
        Some(u32::try_from(MAX_IMAGE_BYTES / 4).unwrap_or(u32::MAX));
      limits.max_image_height =
        Some(u32::try_from(MAX_IMAGE_BYTES / 4).unwrap_or(u32::MAX));
      limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
      reader.limits(limits);
      let img = reader.decode().map_err(|e| format!("EINVAL: {e}"))?;
      let rgba = img.to_rgba8();
      let (width, height) = (rgba.width(), rgba.height());
      check_size(width, height)?;
      Ok(Pixels {
        width,
        height,
        rgba: rgba.into_raw(),
      })
    },
    Format::Rgba => {
      let pixels = check_size(cmd.width, cmd.height)?;
      let want = pixels * 4;
      if bytes.len() < want {
        return Err("EINVAL: RGBA data smaller than s*v*4".into());
      }
      Ok(Pixels {
        width:  cmd.width,
        height: cmd.height,
        rgba:   bytes[..want].to_vec(),
      })
    },
    Format::Rgb => {
      let pixels = check_size(cmd.width, cmd.height)?;
      let want = pixels * 3;
      if bytes.len() < want {
        return Err("EINVAL: RGB data smaller than s*v*3".into());
      }
      let mut rgba = Vec::with_capacity(pixels * 4);
      for chunk in bytes[..want].chunks_exact(3) {
        rgba.extend_from_slice(chunk);
        rgba.push(0xFF);
      }
      Ok(Pixels {
        width: cmd.width,
        height: cmd.height,
        rgba,
      })
    },
  }
}

/// Validate decoded geometry and return its pixel count.
pub(super) fn check_size(width: u32, height: u32) -> Result<usize, String> {
  if width == 0 || height == 0 {
    return Err("EINVAL: zero image dimension".into());
  }
  let pixels = (width as usize)
    .checked_mul(height as usize)
    .ok_or("EINVAL: image dimensions overflow")?;
  let bytes = pixels
    .checked_mul(4)
    .ok_or("EINVAL: image dimensions overflow")?;
  if bytes > MAX_IMAGE_BYTES {
    return Err("EINVAL: image too large".into());
  }
  Ok(pixels)
}

/// Append `data` to `buf`, dropping the excess once `cap` is reached so a
/// runaway transmission cannot grow memory without bound.
pub(super) fn push_capped(buf: &mut Vec<u8>, data: &[u8], cap: usize) {
  let room = cap.saturating_sub(buf.len());
  buf.extend_from_slice(&data[..data.len().min(room)]);
}

/// The stored gap for a frame from its `z` value: zero keeps the default
/// (played as 40ms), a negative value is gapless (stored as 1ms, advanced at
/// once), a positive value is taken as milliseconds.
pub(super) const fn frame_gap(z: i32) -> u32 {
  match z {
    0 => 0,
    n if n < 0 => 1,
    n => n.cast_unsigned(),
  }
}

/// Composite a `(src.width, src.height)` patch onto a `(cw, ch)` canvas with
/// its top-left at `off`, clipped to the canvas, blending unless `overwrite`.
pub(super) fn compose_rect(
  canvas: &mut [u8],
  cw: u32,
  ch: u32,
  src: &Pixels,
  off: (u32, u32),
  overwrite: bool,
) {
  let (ox, oy) = off;
  for sy in 0..src.height {
    let dy = oy + sy;
    if dy >= ch {
      break;
    }
    for sx in 0..src.width {
      let dx = ox + sx;
      if dx >= cw {
        break;
      }
      let si = ((sy * src.width + sx) * 4) as usize;
      let di = ((dy * cw + dx) * 4) as usize;
      blend_into(&mut canvas[di..di + 4], &src.rgba[si..si + 4], overwrite);
    }
  }
}

/// Copy a `(w, h)` region of `src` (an `iw` by `ih` frame) at `soff` onto `dst`
/// (also `iw` by `ih`) at `doff`, clipped to the image, blending unless
/// `overwrite`.
#[expect(
  clippy::too_many_arguments,
  reason = "protocol operation mirrors kitty's parameter set"
)]
pub(super) fn compose_frames(
  dst: &mut [u8],
  iw: u32,
  ih: u32,
  src: &[u8],
  soff: (u32, u32),
  doff: (u32, u32),
  size: (u32, u32),
  overwrite: bool,
) {
  let ((sx0, sy0), (dx0, dy0), (w, h)) = (soff, doff, size);
  for row in 0..h {
    let (sy, dy) = (sy0 + row, dy0 + row);
    if sy >= ih || dy >= ih {
      break;
    }
    for col in 0..w {
      let (sx, dx) = (sx0 + col, dx0 + col);
      if sx >= iw || dx >= iw {
        break;
      }
      let si = ((sy * iw + sx) * 4) as usize;
      let di = ((dy * iw + dx) * 4) as usize;
      blend_into(&mut dst[di..di + 4], &src[si..si + 4], overwrite);
    }
  }
}

/// Composite one straight-alpha RGBA pixel `src` onto `dst` in place, either
/// replacing it (`overwrite`) or alpha-blending.
pub(super) fn blend_into(dst: &mut [u8], src: &[u8], overwrite: bool) {
  if overwrite || src[3] == 255 {
    dst.copy_from_slice(src);
    return;
  }
  let (a, inv) = (u32::from(src[3]), u32::from(255 - src[3]));
  for i in 0..3 {
    dst[i] =
      u8::try_from((u32::from(src[i]) * a + u32::from(dst[i]) * inv) / 255)
        .unwrap_or(u8::MAX);
  }
  dst[3] =
    u8::try_from((a + u32::from(dst[3]) * inv / 255).min(u32::from(u8::MAX)))
      .unwrap_or(u8::MAX);
}

/// zlib-inflate `o=z` payloads.
pub(super) fn inflate(bytes: &[u8]) -> Result<Vec<u8>, String> {
  let mut decoder = ZlibDecoder::new(bytes);
  read_limited(&mut decoder, MAX_SOURCE_BYTES)
    .map_err(|e| format!("EINVAL: zlib: {e}"))
}

/// Read the data for a file / temp-file / shared-memory transmission. `name` is
/// the decoded path or shared-memory object name; `O`/`S` give an offset and
/// length. A temp file is deleted after reading when it is clearly a graphics
/// temp file in a known temp directory.
pub(super) fn read_source(
  cmd: &GraphicsCommand,
  name: &[u8],
) -> Result<Vec<u8>, String> {
  let name =
    str::from_utf8(name).map_err(|_| "EINVAL: non-UTF-8 path".to_string())?;
  let data = match cmd.medium {
    Medium::SharedMemory => read_shm(name, cmd.read_offset, cmd.read_size)?,
    _ => read_file(name, cmd.read_offset, cmd.read_size)?,
  };
  if cmd.medium == Medium::TempFile && is_safe_temp(name) {
    let _ = fs::remove_file(name);
  }
  Ok(data)
}

pub(super) fn read_file(
  path: &str,
  offset: u32,
  size: u32,
) -> Result<Vec<u8>, String> {
  let mut f = File::open(path).map_err(|e| format!("EBADF: {e}"))?;
  read_region(&mut f, offset, size)
}

/// Open a POSIX shared-memory object, read it, and unlink it (the protocol
/// requires the terminal to consume and remove the object).
pub(super) fn read_shm(
  name: &str,
  offset: u32,
  size: u32,
) -> Result<Vec<u8>, String> {
  use rustix::shm;
  let fd = shm::open(name, shm::OFlags::RDONLY, shm::Mode::empty())
    .map_err(|e| format!("EBADF: shm {e}"))?;
  let mut f = File::from(fd);
  let data = read_region(&mut f, offset, size);
  let _ = shm::unlink(name);
  data
}

pub(super) fn read_region<R: Read + Seek>(
  f: &mut R,
  offset: u32,
  size: u32,
) -> Result<Vec<u8>, String> {
  if offset != 0 {
    f.seek(SeekFrom::Start(u64::from(offset)))
      .map_err(|e| format!("EIO: {e}"))?;
  }
  if size != 0 {
    let size = size as usize;
    if size > MAX_SOURCE_BYTES {
      return Err("EINVAL: source data too large".into());
    }
    let mut buf = vec![0; size];
    f.read_exact(&mut buf).map_err(|e| format!("EIO: {e}"))?;
    Ok(buf)
  } else {
    read_limited(f, MAX_SOURCE_BYTES).map_err(|e| format!("EIO: {e}"))
  }
}

/// Read one source without letting a special file or decompressor grow an
/// unbounded buffer. Read one byte beyond the cap to distinguish exact-limit
/// input from an oversized stream.
pub(super) fn read_limited<R: Read>(
  reader: &mut R,
  limit: usize,
) -> io::Result<Vec<u8>> {
  let mut out = Vec::new();
  let mut chunk = [0u8; 8192];
  loop {
    if out.len() == limit {
      let mut extra = [0u8; 1];
      return match reader.read(&mut extra)? {
        0 => Ok(out),
        _ => {
          Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source data too large",
          ))
        },
      };
    }
    let count = (limit - out.len()).min(chunk.len());
    let n = reader.read(&mut chunk[..count])?;
    if n == 0 {
      return Ok(out);
    }
    out.extend_from_slice(&chunk[..n]);
  }
}

/// Whether a temp-file path is safe to delete: it lives in a known temporary
/// directory and its path contains the protocol's `tty-graphics-protocol`
/// marker, exactly as kitty requires before unlinking a client file.
pub(super) fn is_safe_temp(path: &str) -> bool {
  if !path.contains("tty-graphics-protocol") {
    return false;
  }
  let tmpdir = env::var("TMPDIR").unwrap_or_default();
  let roots = ["/tmp/", "/dev/shm/", "/var/tmp/"];
  roots.iter().any(|r| path.starts_with(r))
    || (!tmpdir.is_empty() && path.starts_with(&tmpdir))
}

/// Build a success response (`ESC _G <id keys> ; OK ESC \`), unless suppressed
/// by the quiet level (`q>=1` mutes success).
pub(super) fn respond(cmd: &GraphicsCommand, msg: &str) -> Outcome {
  if cmd.quiet >= 1 {
    return Outcome::default();
  }
  Outcome {
    response: Some(build_response(cmd, msg)),
    grid_op:  None,
  }
}

/// Build an error response unless fully quiet (`q>=2`).
pub(super) fn respond_error(cmd: &GraphicsCommand, msg: &str) -> Outcome {
  if cmd.quiet >= 2 {
    return Outcome::default();
  }
  Outcome {
    response: Some(build_response(cmd, msg)),
    grid_op:  None,
  }
}

pub(super) fn build_response(cmd: &GraphicsCommand, msg: &str) -> Vec<u8> {
  let mut out = Vec::from(&b"\x1b_G"[..]);
  if cmd.id != 0 {
    out.extend_from_slice(format!("i={}", cmd.id).as_bytes());
  }
  if cmd.number != 0 {
    if cmd.id != 0 {
      out.push(b',');
    }
    out.extend_from_slice(format!("I={}", cmd.number).as_bytes());
  }
  out.push(b';');
  out.extend_from_slice(msg.as_bytes());
  out.extend_from_slice(b"\x1b\\");
  out
}
