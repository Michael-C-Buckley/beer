//! The kitty graphics protocol engine: image storage, transmission assembly,
//! decoding, placements, deletion, and the OK/error responses.
//!
//! [`beer_protocols::graphics`] parses the APC control data; this module is the
//! stateful half. It accumulates chunked direct transmissions, reads file and
//! shared-memory payloads, decodes RGB/RGBA/PNG (optionally zlib-compressed)
//! into RGBA, and tracks images and their on-screen placements. The grid
//! carries a per-cell [`crate::grid::ImageRef`] for each displayed cell, so
//! images scroll and clear with the text; this engine owns the pixels and the
//! geometry the renderer composites from.

mod decode;

use std::collections::HashMap;

use beer_protocols::{
  codec::base64_decode,
  graphics::{Action, Format, GraphicsCommand, Medium},
};
use decode::{
  compose_frames,
  compose_rect,
  decode,
  frame_gap,
  inflate,
  push_capped,
  read_source,
  respond,
  respond_error,
};

/// Cap on a single image's pixel buffer (decoded RGBA), guarding against a
/// client claiming an enormous size. 64 MiB is far beyond any real preview.
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Every encoded, file, shared-memory, and decompressed source is capped at
/// the maximum decoded image size. A source larger than its resulting image is
/// not useful to this renderer and only increases peak memory pressure.
const MAX_SOURCE_BYTES: usize = MAX_IMAGE_BYTES;

/// Cap on accumulated direct-transmission base64 across chunks. The image cap
/// is a multiple of three, so this is its exact padded base64 representation.
const MAX_TRANSMIT_BYTES: usize = MAX_SOURCE_BYTES / 3 * 4;

/// Freshly decoded pixels before they become an image or animation frame:
/// `width * height * 4` bytes of row-major, non-premultiplied RGBA.
#[derive(Clone, Debug)]
struct Pixels {
  width:  u32,
  height: u32,
  rgba:   Vec<u8>,
}

/// One animation frame: its RGBA pixels and the gap, in milliseconds, until the
/// terminal advances to the next frame.
#[derive(Clone, Debug)]
struct Frame {
  rgba:   Vec<u8>,
  gap_ms: u32,
}

/// A decoded image: one or more frames plus playback state. A still image is a
/// single frame; the protocol's animation commands append, edit, and compose
/// further frames and drive which one is current.
#[derive(Clone, Debug)]
pub struct Image {
  pub width:  u32,
  pub height: u32,
  /// At least one frame; `frames[0]` is the root (frame 1 in the protocol).
  frames:     Vec<Frame>,
  /// Index of the frame currently shown.
  current:    usize,
  /// Whether the terminal is advancing frames on their gaps.
  playing:    bool,
  /// In loading mode the animation waits at the last frame for more frames
  /// rather than looping (`s=2`).
  loading:    bool,
  /// Remaining loops; `None` loops forever, `Some(0)` stops at the last frame.
  loops_left: Option<u32>,
  /// Milliseconds accumulated toward the current frame's gap.
  accum_ms:   u32,
}

impl Image {
  fn from_pixels(p: Pixels) -> Self {
    Self {
      width:      p.width,
      height:     p.height,
      frames:     vec![Frame {
        rgba:   p.rgba,
        gap_ms: 0,
      }],
      current:    0,
      playing:    false,
      loading:    false,
      loops_left: None,
      accum_ms:   0,
    }
  }

  /// The pixels of the frame currently shown, for the renderer to composite.
  pub fn current_rgba(&self) -> &[u8] {
    &self.frames[self.current.min(self.frames.len() - 1)].rgba
  }

  /// Whether this image has more than one frame and is actively playing.
  const fn is_animating(&self) -> bool {
    self.playing && self.frames.len() > 1
  }

  /// Advance playback by `dt_ms`; returns whether the current frame changed.
  fn advance(&mut self, dt_ms: u32) -> bool {
    if !self.is_animating() {
      return false;
    }
    self.accum_ms += dt_ms;
    // A stored gap of zero (the root frame's default) plays at the standard
    // 40ms; gapless frames are stored as 1ms.
    let gap = match self.frames[self.current].gap_ms {
      0 => 40,
      g => g,
    };
    if self.accum_ms < gap {
      return false;
    }
    self.accum_ms -= gap;
    if self.current + 1 < self.frames.len() {
      self.current += 1;
    } else if self.loading {
      // Wait at the last frame for more frames to arrive.
      return false;
    } else {
      match &mut self.loops_left {
        Some(0) => {
          self.playing = false;
          return false;
        },
        Some(n) => *n -= 1,
        None => {},
      }
      self.current = 0;
    }
    true
  }
}

/// One on-screen placement of an image: the cell rectangle it occupies and the
/// source region/offsets/stacking that decide how it is drawn.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
  pub image:  u32,
  /// Cell rectangle size.
  pub cols:   u16,
  pub rows:   u16,
  /// Source rectangle in image pixels (`w == 0` means to the image edge).
  pub src_x:  u32,
  pub src_y:  u32,
  pub src_w:  u32,
  pub src_h:  u32,
  /// Pixel offset within the first cell.
  pub off_x:  u32,
  pub off_y:  u32,
  /// Stacking order: negative draws below text, non-negative above.
  pub z:      i32,
  /// Parent placement and cell offset for relative placement.
  pub parent: Option<(u32, u32, i32, i32)>,
}

/// What the terminal must do to the grid after a command: stamp a placement, or
/// clear image cells matching a spec. Returned alongside any wire response.
#[derive(Clone, Debug)]
pub enum GridOp {
  /// Stamp a `cols` by `rows` placement at the cursor.
  Place {
    image:       u32,
    placement:   u32,
    cols:        usize,
    rows:        usize,
    keep_cursor: bool,
    parent:      Option<(u32, u32, i32, i32)>,
    replace_old: bool,
  },
  /// Clear cells whose image reference matches the spec.
  Clear { spec: ClearSpec, free: bool },
}

/// Which displayed image cells a delete affects.
#[derive(Clone, Copy, Debug)]
pub enum ClearSpec {
  All,
  Image(u32),
  Placement(u32, u32),
  AtCursor,
  Cell { x: u32, y: u32, z: Option<i32> },
  Column(u32),
  Row(u32),
  Z(i32),
  ImageRange(u32, u32),
}

/// The result of handling one command: an optional response to write back to
/// the application, and an optional grid mutation for the terminal to apply.
#[derive(Default, Debug)]
pub struct Outcome {
  pub response: Option<Vec<u8>>,
  pub grid_op:  Option<GridOp>,
}

/// An in-progress chunked direct transmission: the opening command (which holds
/// the format and dimensions) and the base64 text accumulated so far.
#[derive(Debug)]
struct Pending {
  cmd: GraphicsCommand,
  b64: Vec<u8>,
}

/// The graphics state for one terminal.
#[derive(Default, Debug)]
pub struct Graphics {
  images:       HashMap<u32, Image>,
  /// Image number (`I`) to the id it most recently resolved to.
  by_number:    HashMap<u32, u32>,
  placements:   HashMap<(u32, u32), Placement>,
  pending:      Option<Pending>,
  /// Source of ids for images transmitted without one.
  next_auto_id: u32,
}

impl Graphics {
  pub fn new() -> Self {
    Self {
      next_auto_id: 0xF000_0000,
      ..Self::default()
    }
  }

  pub fn image(&self, id: u32) -> Option<&Image> {
    self.images.get(&id)
  }

  pub fn placement(&self, image: u32, placement: u32) -> Option<&Placement> {
    self.placements.get(&(image, placement))
  }

  /// Handle one fully-received graphics command. `cell_px` is the current cell
  /// size in pixels, needed to translate image dimensions into a cell
  /// rectangle when the client does not give one.
  pub fn handle(
    &mut self,
    cmd: GraphicsCommand,
    payload: &[u8],
    cell_px: (u32, u32),
  ) -> Outcome {
    match cmd.action {
      Action::Delete => self.delete(cmd),
      Action::Put => self.put(cmd, cell_px),
      Action::Animate => self.animate(cmd),
      Action::Compose => self.compose(cmd),
      // Frame data is assembled like image data, then routed to add_frame.
      // Transmit / TransmitAndDisplay / Query assemble pixel data too.
      _ => self.transmit(cmd, payload, cell_px),
    }
  }

  /// Assemble (possibly chunked) pixel data, decode and store it, and - for
  /// `a=T` - emit a placement. `a=q` decodes to verify but neither stores nor
  /// displays.
  fn transmit(
    &mut self,
    cmd: GraphicsCommand,
    payload: &[u8],
    cell_px: (u32, u32),
  ) -> Outcome {
    // Continuation chunk: append to the in-flight transmission.
    if let Some(pending) = &self.pending
      && is_continuation(&cmd, pending)
    {
      return self.accumulate(cmd, payload, cell_px);
    }
    if cmd.more {
      // First chunk of a multi-chunk transmission: start accumulating.
      let mut b64 = Vec::new();
      push_capped(&mut b64, payload, MAX_TRANSMIT_BYTES);
      self.pending = Some(Pending { cmd, b64 });
      return Outcome::default();
    }
    // Single-shot transmission.
    self.finalize(cmd, payload, cell_px)
  }

  fn accumulate(
    &mut self,
    cont: GraphicsCommand,
    payload: &[u8],
    cell_px: (u32, u32),
  ) -> Outcome {
    let done = {
      let Some(pending) = self.pending.as_mut() else {
        return respond_error(&cont, "EINVAL: missing pending transmission");
      };
      push_capped(&mut pending.b64, payload, MAX_TRANSMIT_BYTES);
      // The last chunk carries m=0; the opening command's geometry is used.
      !cont.more
    };
    if !done {
      return Outcome::default();
    }
    let Some(Pending { cmd, b64 }) = self.pending.take() else {
      return respond_error(&cont, "EINVAL: missing pending transmission");
    };
    self.finalize_b64(cmd, &b64, cell_px)
  }

  /// Finalize a single-shot transmission whose payload is one base64 blob.
  fn finalize(
    &mut self,
    cmd: GraphicsCommand,
    payload: &[u8],
    cell_px: (u32, u32),
  ) -> Outcome {
    self.finalize_b64(cmd, payload, cell_px)
  }

  fn finalize_b64(
    &mut self,
    cmd: GraphicsCommand,
    b64: &[u8],
    cell_px: (u32, u32),
  ) -> Outcome {
    match Self::load_pixels(&cmd, b64) {
      Ok(pixels) => {
        if cmd.action == Action::Query {
          // Verify only: report success, store nothing.
          return respond(&cmd, "OK");
        }
        if cmd.action == Action::Frame {
          // Animation frame data for an existing image.
          return self.add_frame(&cmd, &pixels);
        }
        let (id, replaced) = self.store(&cmd, pixels);
        if cmd.action == Action::TransmitAndDisplay {
          let mut out = self.display(id, &cmd, cell_px);
          if let Some(GridOp::Place { replace_old, .. }) = out.grid_op.as_mut()
          {
            *replace_old = replaced;
          }
          out.response = respond(&cmd, "OK").response;
          out
        } else {
          let mut out = respond(&cmd, "OK");
          if replaced {
            out.grid_op = Some(GridOp::Clear {
              spec: ClearSpec::Image(id),
              free: false,
            });
          }
          out
        }
      },
      Err(msg) => respond_error(&cmd, &msg),
    }
  }

  /// Decode the transmitted payload into RGBA [`Pixels`].
  fn load_pixels(cmd: &GraphicsCommand, b64: &[u8]) -> Result<Pixels, String> {
    let raw = base64_decode(b64).ok_or("EINVAL: bad base64 payload")?;
    if raw.len() > MAX_SOURCE_BYTES {
      return Err("EINVAL: source data too large".into());
    }
    // For non-direct mediums the payload is the path / shared-memory name.
    let bytes = match cmd.medium {
      Medium::Direct => raw,
      _ => read_source(cmd, &raw)?,
    };
    let bytes = if cmd.compressed {
      inflate(&bytes)?
    } else {
      bytes
    };
    decode(cmd, bytes)
  }

  /// Store pixels as a new still image under its id (or number, or an auto id),
  /// replacing any existing image with that id. Returns the id used.
  fn store(&mut self, cmd: &GraphicsCommand, pixels: Pixels) -> (u32, bool) {
    let id = if cmd.id != 0 {
      cmd.id
    } else if cmd.number != 0 {
      // A fresh id for this number; remember the mapping.
      let id = self.alloc_id();
      self.by_number.insert(cmd.number, id);
      id
    } else {
      self.alloc_id()
    };
    let replaced = self.images.contains_key(&id);
    self.placements.retain(|&(image, _), _| image != id);
    self.images.insert(id, Image::from_pixels(pixels));
    (id, replaced)
  }

  fn alloc_id(&mut self) -> u32 {
    let id = self.next_auto_id;
    self.next_auto_id = self.next_auto_id.wrapping_add(1).max(0xF000_0000);
    id
  }

  /// Add or edit an animation frame (`a=f`). The decoded `pixels` are composed
  /// onto a base canvas - a chosen base frame (`c`) or transparent black -
  /// inside the destination rectangle `(x, y)` sized to the data, then stored
  /// as a new frame or, with `r`, used to replace frame `r`.
  fn add_frame(&mut self, cmd: &GraphicsCommand, pixels: &Pixels) -> Outcome {
    let Some(id) = self.resolve_id(cmd) else {
      return respond_error(cmd, "ENOENT: no such image");
    };
    let Some(img) = self.images.get_mut(&id) else {
      return respond_error(cmd, "ENOENT: no such image");
    };
    let canvas_len = img.width as usize * img.height as usize * 4;
    // Base canvas: an existing frame's pixels, or transparent black.
    let mut canvas = match img.frames.get(cmd.c.wrapping_sub(1) as usize) {
      Some(f) if cmd.c != 0 => f.rgba.clone(),
      _ => vec![0u8; canvas_len],
    };
    compose_rect(
      &mut canvas,
      img.width,
      img.height,
      pixels,
      (cmd.x, cmd.y),
      cmd.cap_x == 1,
    );
    let gap = frame_gap(cmd.z);
    if cmd.r != 0 {
      match img.frames.get_mut(cmd.r as usize - 1) {
        Some(f) => {
          f.rgba = canvas;
          f.gap_ms = gap;
        },
        None => return respond_error(cmd, "ENOENT: no such frame"),
      }
    } else {
      img.frames.push(Frame {
        rgba:   canvas,
        gap_ms: gap,
      });
    }
    respond(cmd, "OK")
  }

  /// Compose a rectangle of one frame onto another (`a=c`): copy a `w` by `h`
  /// region from source frame `r` at `(x, y)` onto destination frame `c` at
  /// `(X, Y)`, alpha-blending unless `C=1` requests a plain overwrite.
  fn compose(&mut self, cmd: GraphicsCommand) -> Outcome {
    let Some(id) = self.resolve_id(&cmd) else {
      return respond_error(&cmd, "ENOENT: no such image");
    };
    let Some(img) = self.images.get_mut(&id) else {
      return respond_error(&cmd, "ENOENT: no such image");
    };
    let (src, dst) = (cmd.r as usize, cmd.c as usize);
    if src == 0 || dst == 0 || src > img.frames.len() || dst > img.frames.len()
    {
      return respond_error(&cmd, "ENOENT: no such frame");
    }
    let (iw, ih) = (img.width, img.height);
    let w = if cmd.w == 0 { iw } else { cmd.w };
    let h = if cmd.h == 0 { ih } else { cmd.h };
    let source = img.frames[src - 1].rgba.clone();
    let overwrite = cmd.cursor_policy == 1;
    compose_frames(
      &mut img.frames[dst - 1].rgba,
      iw,
      ih,
      &source,
      (cmd.x, cmd.y),
      (cmd.cap_x, cmd.cap_y),
      (w, h),
      overwrite,
    );
    respond(&cmd, "OK")
  }

  /// Control playback (`a=a`): set the current frame (`c`), the run state
  /// (`s`: stop / loading / loop), and the loop count (`v`).
  fn animate(&mut self, cmd: GraphicsCommand) -> Outcome {
    let Some(id) = self.resolve_id(&cmd) else {
      return respond_error(&cmd, "ENOENT: no such image");
    };
    let Some(img) = self.images.get_mut(&id) else {
      return respond_error(&cmd, "ENOENT: no such image");
    };
    if cmd.c != 0 {
      img.current = (cmd.c as usize - 1).min(img.frames.len() - 1);
      img.accum_ms = 0;
    }
    // For a=a the `s` key is the run state and `v` the loop count; the parser
    // stores them under the width/height fields they share.
    match cmd.width {
      1 => img.playing = false,
      2 => {
        img.playing = true;
        img.loading = true;
      },
      3 => {
        img.playing = true;
        img.loading = false;
      },
      _ => {},
    }
    match cmd.height {
      0 => {},
      1 => img.loops_left = None,
      n => img.loops_left = Some(n - 1),
    }
    respond(&cmd, "OK")
  }

  /// Advance every playing animation by `dt_ms`; returns whether any image's
  /// current frame changed (and the screen therefore needs repainting).
  pub fn tick(&mut self, dt_ms: u32) -> bool {
    let mut changed = false;
    for img in self.images.values_mut() {
      changed |= img.advance(dt_ms);
    }
    changed
  }

  /// Whether any stored image is currently playing a multi-frame animation.
  pub fn is_animating(&self) -> bool {
    self.images.values().any(Image::is_animating)
  }

  /// Display an already-stored image (`a=p`).
  fn put(&mut self, cmd: GraphicsCommand, cell_px: (u32, u32)) -> Outcome {
    let Some(id) = self.resolve_id(&cmd) else {
      return respond_error(&cmd, "ENOENT: no such image");
    };
    if !self.images.contains_key(&id) {
      return respond_error(&cmd, "ENOENT: no such image");
    }
    let mut out = self.display(id, &cmd, cell_px);
    out.response = respond(&cmd, "OK").response;
    out
  }

  /// Compute and register a placement for `id`, returning the grid stamp op.
  fn display(
    &mut self,
    id: u32,
    cmd: &GraphicsCommand,
    cell_px: (u32, u32),
  ) -> Outcome {
    let Some(img) = self.images.get(&id) else {
      return respond_error(cmd, "ENOENT: no such image");
    };
    let (cell_w, cell_h) = (cell_px.0.max(1), cell_px.1.max(1));

    // Source rectangle, clamped to the image.
    let src_w = if cmd.w == 0 {
      img.width.saturating_sub(cmd.x)
    } else {
      cmd.w.min(img.width.saturating_sub(cmd.x))
    };
    let src_h = if cmd.h == 0 {
      img.height.saturating_sub(cmd.y)
    } else {
      cmd.h.min(img.height.saturating_sub(cmd.y))
    };

    // Cell rectangle: explicit c/r, else derived from the source pixels,
    // filling in a missing dimension by aspect ratio.
    let (cols, rows) = cell_rect(cmd.c, cmd.r, src_w, src_h, cell_w, cell_h);
    if cols == 0 || rows == 0 {
      return respond_error(cmd, "EINVAL: zero-sized placement");
    }
    let parent = if cmd.parent_id == 0 {
      None
    } else {
      let key = (cmd.parent_id, cmd.parent_placement);
      if cmd.virtual_placement {
        return respond_error(
          cmd,
          "EINVAL: relative placement cannot be virtual",
        );
      }
      if !self.placements.contains_key(&key) {
        return respond_error(cmd, "ENOENT: no such parent placement");
      }
      if self.relative_cycle((id, cmd.placement), key) {
        return respond_error(cmd, "EINVAL: relative placement cycle");
      }
      Some((key.0, key.1, cmd.rel_h, cmd.rel_v))
    };

    let placement_id = cmd.placement;
    self.placements.insert((id, placement_id), Placement {
      image: id,
      cols: u16::try_from(cols.min(usize::from(u16::MAX))).unwrap_or(u16::MAX),
      rows: u16::try_from(rows.min(usize::from(u16::MAX))).unwrap_or(u16::MAX),
      src_x: cmd.x,
      src_y: cmd.y,
      src_w,
      src_h,
      off_x: cmd.cap_x,
      off_y: cmd.cap_y,
      z: cmd.z,
      parent,
    });
    // A virtual placement (`U=1`) reserves geometry for Unicode-placeholder
    // cells the application prints itself; it stamps no cells of its own.
    let grid_op = (!cmd.virtual_placement).then_some(GridOp::Place {
      image: id,
      placement: placement_id,
      cols,
      rows,
      keep_cursor: cmd.cursor_policy == 1,
      parent,
      replace_old: false,
    });
    Outcome {
      response: None,
      grid_op,
    }
  }

  fn relative_cycle(&self, child: (u32, u32), mut parent: (u32, u32)) -> bool {
    for _ in 0..64 {
      if parent == child {
        return true;
      }
      let Some(next) = self
        .placements
        .get(&parent)
        .and_then(|placement| placement.parent)
      else {
        return false;
      };
      parent = (next.0, next.1);
    }
    true
  }

  /// Resolve the image an action refers to: by id, else by number.
  fn resolve_id(&self, cmd: &GraphicsCommand) -> Option<u32> {
    if cmd.id != 0 {
      Some(cmd.id)
    } else if cmd.number != 0 {
      self.by_number.get(&cmd.number).copied()
    } else {
      None
    }
  }

  /// Handle `a=d`: clear placements (and free image data for uppercase forms).
  fn delete(&mut self, cmd: GraphicsCommand) -> Outcome {
    let free = cmd.delete_frees_data();
    let spec = match cmd.delete.to_ascii_lowercase() {
      b'a' | 0 => ClearSpec::All,
      b'i' => {
        let id = cmd.id;
        if cmd.placement != 0 {
          ClearSpec::Placement(id, cmd.placement)
        } else {
          ClearSpec::Image(id)
        }
      },
      b'n' => {
        match self.by_number.get(&cmd.number).copied() {
          Some(id) => ClearSpec::Image(id),
          None => return Outcome::default(),
        }
      },
      b'c' => ClearSpec::AtCursor,
      b'f' => return self.delete_frames(&cmd),
      target => {
        let Some(spec) = spatial_delete_spec(target, &cmd) else {
          return Outcome::default();
        };
        spec
      },
    };
    Outcome {
      response: None,
      grid_op:  Some(GridOp::Clear { spec, free }),
    }
  }

  fn delete_frames(&mut self, cmd: &GraphicsCommand) -> Outcome {
    let Some(id) = self.resolve_id(cmd) else {
      return Outcome::default();
    };
    let Some(image) = self.images.get_mut(&id) else {
      return Outcome::default();
    };
    if cmd.r == 0 {
      image.frames.truncate(1);
    } else if let Some(index) =
      usize::try_from(cmd.r).ok().and_then(|n| n.checked_sub(1))
      && index > 0
      && index < image.frames.len()
    {
      image.frames.remove(index);
    }
    image.current = image.current.min(image.frames.len() - 1);
    Outcome::default()
  }

  pub fn finish_delete(
    &mut self,
    removed: &[(u32, u32)],
    free: bool,
    referenced: impl Fn(u32) -> bool,
  ) {
    self.placements.retain(|key, _| !removed.contains(key));
    if free {
      self.images.retain(|id, _| referenced(*id));
      self.by_number.retain(|_, id| self.images.contains_key(id));
    }
  }

  pub fn delete_targets(
    &self,
    spec: ClearSpec,
    direct: &[(u32, u32)],
  ) -> Vec<(u32, u32)> {
    let mut targets = direct.to_vec();
    for (&key, placement) in &self.placements {
      if placement_matches(key, placement, spec) && !targets.contains(&key) {
        targets.push(key);
      }
    }
    loop {
      let before = targets.len();
      for (&key, placement) in &self.placements {
        if placement
          .parent
          .is_some_and(|parent| targets.contains(&(parent.0, parent.1)))
          && !targets.contains(&key)
        {
          targets.push(key);
        }
      }
      if targets.len() == before {
        return targets;
      }
    }
  }
}

fn spatial_delete_spec(target: u8, cmd: &GraphicsCommand) -> Option<ClearSpec> {
  match target {
    b'p' => {
      Some(ClearSpec::Cell {
        x: cmd.x,
        y: cmd.y,
        z: None,
      })
    },
    b'q' => {
      Some(ClearSpec::Cell {
        x: cmd.x,
        y: cmd.y,
        z: Some(cmd.z),
      })
    },
    b'x' => Some(ClearSpec::Column(cmd.x)),
    b'y' => Some(ClearSpec::Row(cmd.y)),
    b'z' => Some(ClearSpec::Z(cmd.z)),
    b'r' => Some(ClearSpec::ImageRange(cmd.x.min(cmd.y), cmd.x.max(cmd.y))),
    _ => None,
  }
}

fn placement_matches(
  key: (u32, u32),
  placement: &Placement,
  spec: ClearSpec,
) -> bool {
  match spec {
    ClearSpec::All => true,
    ClearSpec::Image(id) => key.0 == id,
    ClearSpec::Placement(id, p) => key == (id, p),
    ClearSpec::Z(z) => placement.z == z,
    ClearSpec::ImageRange(lo, hi) => key.0 >= lo && key.0 <= hi,
    _ => false,
  }
}

/// Whether a command is a continuation chunk of the in-flight transmission
/// rather than a fresh command: it carries no geometry or format keys, only
/// `m`/`q` (and, for frames, `a=f`). Both image and animation-frame
/// transmissions chunk this way.
///
/// The spec says continuation chunks omit `i`/`I`, but some senders (e.g.
/// kitty's icat) repeat the image id on every chunk. Accept those too, as
/// long as the id matches the in-flight transmission.
fn is_continuation(cmd: &GraphicsCommand, pending: &Pending) -> bool {
  matches!(cmd.action, Action::Transmit | Action::Frame)
    && (cmd.id == 0 || cmd.id == pending.cmd.id)
    && (cmd.number == 0 || cmd.number == pending.cmd.number)
    && cmd.format == Format::Rgba
    && cmd.width == 0
    && cmd.height == 0
}

/// Choose the cell rectangle for a placement. Explicit `c`/`r` win; a missing
/// dimension is filled by source aspect ratio; with neither given the pixel
/// size is divided by the cell size (rounded up).
fn cell_rect(
  c: u32,
  r: u32,
  src_w: u32,
  src_h: u32,
  cell_w: u32,
  cell_h: u32,
) -> (usize, usize) {
  let by_px_w = src_w.div_ceil(cell_w).max(1);
  let by_px_h = src_h.div_ceil(cell_h).max(1);
  let (cols, rows) = match (c, r) {
    (0, 0) => (by_px_w, by_px_h),
    (c, 0) => (c, aspect_cells(c, cell_w, src_h, src_w.max(1), cell_h)),
    (0, r) => (aspect_cells(r, cell_h, src_w, src_h.max(1), cell_w), r),
    (c, r) => (c, r),
  };
  (cols as usize, rows as usize)
}

/// Scale a requested cell dimension by source aspect ratio without allowing
/// terminal-controlled dimensions to overflow intermediate arithmetic.
fn aspect_cells(
  cells: u32,
  cell_px: u32,
  source_num: u32,
  source_den: u32,
  other_cell_px: u32,
) -> u32 {
  let numerator =
    u128::from(cells) * u128::from(cell_px) * u128::from(source_num);
  let denominator = u128::from(source_den) * u128::from(other_cell_px);
  u32::try_from((numerator / denominator).max(1).min(u128::from(u32::MAX)))
    .unwrap_or(u32::MAX)
}

#[cfg(test)] mod tests;
