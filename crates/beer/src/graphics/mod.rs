//! The kitty graphics protocol engine: image storage, transmission assembly,
//! decoding, placements, deletion, and the OK/error responses.
//!
//! [`beer_protocols::graphics`] parses the APC control data; this module is the
//! stateful half. It accumulates chunked direct transmissions, reads file and
//! shared-memory payloads, decodes RGB/RGBA/PNG (optionally zlib-compressed)
//! into RGBA, and tracks images and their on-screen placements. The grid carries
//! a per-cell [`crate::grid::ImageRef`] for each displayed cell, so images scroll
//! and clear with the text; this engine owns the pixels and the geometry the
//! renderer composites from.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom};

use beer_protocols::codec::base64_decode;
use beer_protocols::graphics::{Action, Format, GraphicsCommand, Medium};

/// Cap on a single image's pixel buffer (decoded RGBA), guarding against a
/// client claiming an enormous size. 64 MiB is far beyond any real preview.
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Cap on accumulated direct-transmission base64 across chunks.
const MAX_TRANSMIT_BYTES: usize = 96 * 1024 * 1024;

/// Freshly decoded pixels before they become an image or animation frame:
/// `width * height * 4` bytes of row-major, non-premultiplied RGBA.
#[derive(Clone, Debug)]
struct Pixels {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// One animation frame: its RGBA pixels and the gap, in milliseconds, until the
/// terminal advances to the next frame.
#[derive(Clone, Debug)]
struct Frame {
    rgba: Vec<u8>,
    gap_ms: u32,
}

/// A decoded image: one or more frames plus playback state. A still image is a
/// single frame; the protocol's animation commands append, edit, and compose
/// further frames and drive which one is current.
#[derive(Clone, Debug)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// At least one frame; `frames[0]` is the root (frame 1 in the protocol).
    frames: Vec<Frame>,
    /// Index of the frame currently shown.
    current: usize,
    /// Whether the terminal is advancing frames on their gaps.
    playing: bool,
    /// In loading mode the animation waits at the last frame for more frames
    /// rather than looping (`s=2`).
    loading: bool,
    /// Remaining loops; `None` loops forever, `Some(0)` stops at the last frame.
    loops_left: Option<u32>,
    /// Milliseconds accumulated toward the current frame's gap.
    accum_ms: u32,
}

impl Image {
    fn from_pixels(p: Pixels) -> Self {
        Self {
            width: p.width,
            height: p.height,
            frames: vec![Frame {
                rgba: p.rgba,
                gap_ms: 0,
            }],
            current: 0,
            playing: false,
            loading: false,
            loops_left: None,
            accum_ms: 0,
        }
    }

    /// The pixels of the frame currently shown, for the renderer to composite.
    pub fn current_rgba(&self) -> &[u8] {
        &self.frames[self.current.min(self.frames.len() - 1)].rgba
    }

    /// Whether this image has more than one frame and is actively playing.
    fn is_animating(&self) -> bool {
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
                }
                Some(n) => *n -= 1,
                None => {}
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
    pub image: u32,
    /// Cell rectangle size.
    pub cols: u16,
    pub rows: u16,
    /// Source rectangle in image pixels (`w == 0` means to the image edge).
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// Pixel offset within the first cell.
    pub off_x: u32,
    pub off_y: u32,
    /// Stacking order: negative draws below text, non-negative above.
    pub z: i32,
}

/// What the terminal must do to the grid after a command: stamp a placement, or
/// clear image cells matching a spec. Returned alongside any wire response.
#[derive(Clone, Debug)]
pub enum GridOp {
    /// Stamp a `cols` by `rows` placement at the cursor.
    Place {
        image: u32,
        placement: u32,
        cols: usize,
        rows: usize,
        keep_cursor: bool,
    },
    /// Clear cells whose image reference matches the spec.
    Clear(ClearSpec),
}

/// Which displayed image cells a delete affects.
#[derive(Clone, Copy, Debug)]
pub enum ClearSpec {
    All,
    Image(u32),
    Placement(u32, u32),
    AtCursor,
}

/// The result of handling one command: an optional response to write back to the
/// application, and an optional grid mutation for the terminal to apply.
#[derive(Default, Debug)]
pub struct Outcome {
    pub response: Option<Vec<u8>>,
    pub grid_op: Option<GridOp>,
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
    images: HashMap<u32, Image>,
    /// Image number (`I`) to the id it most recently resolved to.
    by_number: HashMap<u32, u32>,
    placements: HashMap<(u32, u32), Placement>,
    pending: Option<Pending>,
    /// Source of ids for images transmitted without one.
    next_auto_id: u32,
}

impl Graphics {
    pub fn new() -> Self {
        Self {
            next_auto_id: 0xf000_0000,
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
    pub fn handle(&mut self, cmd: GraphicsCommand, payload: &[u8], cell_px: (u32, u32)) -> Outcome {
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
    fn transmit(&mut self, cmd: GraphicsCommand, payload: &[u8], cell_px: (u32, u32)) -> Outcome {
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
            let pending = self.pending.as_mut().expect("pending checked by caller");
            push_capped(&mut pending.b64, payload, MAX_TRANSMIT_BYTES);
            // The last chunk carries m=0; the opening command's geometry is used.
            !cont.more
        };
        if !done {
            return Outcome::default();
        }
        let Pending { cmd, b64 } = self.pending.take().expect("pending checked by caller");
        self.finalize_b64(cmd, &b64, cell_px)
    }

    /// Finalize a single-shot transmission whose payload is one base64 blob.
    fn finalize(&mut self, cmd: GraphicsCommand, payload: &[u8], cell_px: (u32, u32)) -> Outcome {
        self.finalize_b64(cmd, payload, cell_px)
    }

    fn finalize_b64(&mut self, cmd: GraphicsCommand, b64: &[u8], cell_px: (u32, u32)) -> Outcome {
        match self.load_pixels(&cmd, b64) {
            Ok(pixels) => {
                if cmd.action == Action::Query {
                    // Verify only: report success, store nothing.
                    return respond(&cmd, "OK");
                }
                if cmd.action == Action::Frame {
                    // Animation frame data for an existing image.
                    return self.add_frame(&cmd, pixels);
                }
                let id = self.store(&cmd, pixels);
                if cmd.action == Action::TransmitAndDisplay {
                    let mut out = self.display(id, &cmd, cell_px);
                    out.response = respond(&cmd, "OK").response;
                    out
                } else {
                    respond(&cmd, "OK")
                }
            }
            Err(msg) => respond_error(&cmd, &msg),
        }
    }

    /// Decode the transmitted payload into RGBA [`Pixels`].
    fn load_pixels(&self, cmd: &GraphicsCommand, b64: &[u8]) -> Result<Pixels, String> {
        let raw = base64_decode(b64).ok_or("EINVAL: bad base64 payload")?;
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
    fn store(&mut self, cmd: &GraphicsCommand, pixels: Pixels) -> u32 {
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
        self.images.insert(id, Image::from_pixels(pixels));
        id
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_auto_id;
        self.next_auto_id = self.next_auto_id.wrapping_add(1).max(0xf000_0000);
        id
    }

    /// Add or edit an animation frame (`a=f`). The decoded `pixels` are composed
    /// onto a base canvas - a chosen base frame (`c`) or transparent black -
    /// inside the destination rectangle `(x, y)` sized to the data, then stored
    /// as a new frame or, with `r`, used to replace frame `r`.
    fn add_frame(&mut self, cmd: &GraphicsCommand, pixels: Pixels) -> Outcome {
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
            &pixels,
            (cmd.x, cmd.y),
            cmd.cap_x == 1,
        );
        let gap = frame_gap(cmd.z);
        if cmd.r != 0 {
            match img.frames.get_mut(cmd.r as usize - 1) {
                Some(f) => {
                    f.rgba = canvas;
                    f.gap_ms = gap;
                }
                None => return respond_error(cmd, "ENOENT: no such frame"),
            }
        } else {
            img.frames.push(Frame {
                rgba: canvas,
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
        if src == 0 || dst == 0 || src > img.frames.len() || dst > img.frames.len() {
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
            }
            3 => {
                img.playing = true;
                img.loading = false;
            }
            _ => {}
        }
        match cmd.height {
            0 => {}
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
        let id = self.resolve_id(&cmd);
        if id.is_none() || !self.images.contains_key(&id.unwrap()) {
            return respond_error(&cmd, "ENOENT: no such image");
        }
        let id = id.unwrap();
        let mut out = self.display(id, &cmd, cell_px);
        out.response = respond(&cmd, "OK").response;
        out
    }

    /// Compute and register a placement for `id`, returning the grid stamp op.
    fn display(&mut self, id: u32, cmd: &GraphicsCommand, cell_px: (u32, u32)) -> Outcome {
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

        let placement_id = cmd.placement;
        self.placements.insert(
            (id, placement_id),
            Placement {
                image: id,
                cols: cols.min(u16::MAX as usize) as u16,
                rows: rows.min(u16::MAX as usize) as u16,
                src_x: cmd.x,
                src_y: cmd.y,
                src_w,
                src_h,
                off_x: cmd.cap_x,
                off_y: cmd.cap_y,
                z: cmd.z,
            },
        );
        // A virtual placement (`U=1`) reserves geometry for Unicode-placeholder
        // cells the application prints itself; it stamps no cells of its own.
        let grid_op = (!cmd.virtual_placement).then_some(GridOp::Place {
            image: id,
            placement: placement_id,
            cols,
            rows,
            keep_cursor: cmd.cursor_policy == 1,
        });
        Outcome {
            response: None,
            grid_op,
        }
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
            0 | b'a' => ClearSpec::All,
            b'i' => {
                let id = cmd.id;
                if cmd.placement != 0 {
                    ClearSpec::Placement(id, cmd.placement)
                } else {
                    ClearSpec::Image(id)
                }
            }
            b'n' => match self.by_number.get(&cmd.number).copied() {
                Some(id) => ClearSpec::Image(id),
                None => return Outcome::default(),
            },
            b'c' => ClearSpec::AtCursor,
            // Other targets (by column/row/z-index, frames) are not yet
            // distinguished; treat them as a visible-placement clear.
            _ => ClearSpec::All,
        };
        if free {
            self.free_for(&spec);
        }
        Outcome {
            response: None,
            grid_op: Some(GridOp::Clear(spec)),
        }
    }

    /// Drop stored image data for an uppercase delete, when not pinned elsewhere.
    fn free_for(&mut self, spec: &ClearSpec) {
        match *spec {
            ClearSpec::All => {
                self.images.clear();
                self.placements.clear();
                self.by_number.clear();
            }
            ClearSpec::Image(id) => {
                self.images.remove(&id);
                self.placements.retain(|&(img, _), _| img != id);
            }
            ClearSpec::Placement(id, p) => {
                self.placements.remove(&(id, p));
            }
            ClearSpec::AtCursor => {}
        }
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
fn cell_rect(c: u32, r: u32, src_w: u32, src_h: u32, cell_w: u32, cell_h: u32) -> (usize, usize) {
    let by_px_w = src_w.div_ceil(cell_w).max(1);
    let by_px_h = src_h.div_ceil(cell_h).max(1);
    let (cols, rows) = match (c, r) {
        (0, 0) => (by_px_w, by_px_h),
        (c, 0) => (
            c,
            ((c * cell_w) as u64 * src_h as u64 / (src_w.max(1) as u64 * cell_h as u64)).max(1)
                as u32,
        ),
        (0, r) => (
            ((r * cell_h) as u64 * src_w as u64 / (src_h.max(1) as u64 * cell_w as u64)).max(1)
                as u32,
            r,
        ),
        (c, r) => (c, r),
    };
    (cols as usize, rows as usize)
}

/// Decode transmitted bytes into RGBA [`Pixels`] per the command's format.
fn decode(cmd: &GraphicsCommand, bytes: Vec<u8>) -> Result<Pixels, String> {
    match cmd.format {
        Format::Png => {
            let img = image::load_from_memory(&bytes).map_err(|e| format!("EINVAL: {e}"))?;
            let rgba = img.to_rgba8();
            let (width, height) = (rgba.width(), rgba.height());
            check_size(width, height)?;
            Ok(Pixels {
                width,
                height,
                rgba: rgba.into_raw(),
            })
        }
        Format::Rgba => {
            check_size(cmd.width, cmd.height)?;
            let want = cmd.width as usize * cmd.height as usize * 4;
            if bytes.len() < want {
                return Err("EINVAL: RGBA data smaller than s*v*4".into());
            }
            Ok(Pixels {
                width: cmd.width,
                height: cmd.height,
                rgba: bytes[..want].to_vec(),
            })
        }
        Format::Rgb => {
            check_size(cmd.width, cmd.height)?;
            let px = cmd.width as usize * cmd.height as usize;
            if bytes.len() < px * 3 {
                return Err("EINVAL: RGB data smaller than s*v*3".into());
            }
            let mut rgba = Vec::with_capacity(px * 4);
            for chunk in bytes[..px * 3].chunks_exact(3) {
                rgba.extend_from_slice(chunk);
                rgba.push(0xff);
            }
            Ok(Pixels {
                width: cmd.width,
                height: cmd.height,
                rgba,
            })
        }
    }
}

fn check_size(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("EINVAL: zero image dimension".into());
    }
    let bytes = width as usize * height as usize * 4;
    if bytes > MAX_IMAGE_BYTES {
        return Err("EINVAL: image too large".into());
    }
    Ok(())
}

/// Append `data` to `buf`, dropping the excess once `cap` is reached so a
/// runaway transmission cannot grow memory without bound.
fn push_capped(buf: &mut Vec<u8>, data: &[u8], cap: usize) {
    let room = cap.saturating_sub(buf.len());
    buf.extend_from_slice(&data[..data.len().min(room)]);
}

/// The stored gap for a frame from its `z` value: zero keeps the default (played
/// as 40ms), a negative value is gapless (stored as 1ms, advanced at once), a
/// positive value is taken as milliseconds.
fn frame_gap(z: i32) -> u32 {
    match z {
        0 => 0,
        n if n < 0 => 1,
        n => n as u32,
    }
}

/// Composite a `(src.width, src.height)` patch onto a `(cw, ch)` canvas with its
/// top-left at `off`, clipped to the canvas, blending unless `overwrite`.
fn compose_rect(
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
#[allow(clippy::too_many_arguments)]
fn compose_frames(
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
fn blend_into(dst: &mut [u8], src: &[u8], overwrite: bool) {
    if overwrite || src[3] == 255 {
        dst.copy_from_slice(src);
        return;
    }
    let (a, inv) = (u32::from(src[3]), u32::from(255 - src[3]));
    for i in 0..3 {
        dst[i] = ((u32::from(src[i]) * a + u32::from(dst[i]) * inv) / 255) as u8;
    }
    dst[3] = (a + u32::from(dst[3]) * inv / 255).min(255) as u8;
}

/// zlib-inflate `o=z` payloads.
fn inflate(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| format!("EINVAL: zlib: {e}"))?;
    Ok(out)
}

/// Read the data for a file / temp-file / shared-memory transmission. `name` is
/// the decoded path or shared-memory object name; `O`/`S` give an offset and
/// length. A temp file is deleted after reading when it is clearly a graphics
/// temp file in a known temp directory.
fn read_source(cmd: &GraphicsCommand, name: &[u8]) -> Result<Vec<u8>, String> {
    let name = std::str::from_utf8(name).map_err(|_| "EINVAL: non-UTF-8 path".to_string())?;
    let data = match cmd.medium {
        Medium::SharedMemory => read_shm(name, cmd.read_offset, cmd.read_size)?,
        _ => read_file(name, cmd.read_offset, cmd.read_size)?,
    };
    if cmd.medium == Medium::TempFile && is_safe_temp(name) {
        let _ = std::fs::remove_file(name);
    }
    Ok(data)
}

fn read_file(path: &str, offset: u32, size: u32) -> Result<Vec<u8>, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("EBADF: {e}"))?;
    read_region(&mut f, offset, size)
}

/// Open a POSIX shared-memory object, read it, and unlink it (the protocol
/// requires the terminal to consume and remove the object).
fn read_shm(name: &str, offset: u32, size: u32) -> Result<Vec<u8>, String> {
    use rustix::shm;
    let fd = shm::open(name, shm::OFlags::RDONLY, shm::Mode::empty())
        .map_err(|e| format!("EBADF: shm {e}"))?;
    let mut f = std::fs::File::from(fd);
    let data = read_region(&mut f, offset, size);
    let _ = shm::unlink(name);
    data
}

fn read_region(f: &mut std::fs::File, offset: u32, size: u32) -> Result<Vec<u8>, String> {
    if offset != 0 {
        f.seek(SeekFrom::Start(offset as u64))
            .map_err(|e| format!("EIO: {e}"))?;
    }
    let mut buf = Vec::new();
    if size != 0 {
        buf.resize(size as usize, 0);
        f.read_exact(&mut buf).map_err(|e| format!("EIO: {e}"))?;
    } else {
        f.read_to_end(&mut buf).map_err(|e| format!("EIO: {e}"))?;
    }
    Ok(buf)
}

/// Whether a temp-file path is safe to delete: it lives in a known temporary
/// directory and its path contains the protocol's `tty-graphics-protocol`
/// marker, exactly as kitty requires before unlinking a client file.
fn is_safe_temp(path: &str) -> bool {
    if !path.contains("tty-graphics-protocol") {
        return false;
    }
    let tmpdir = std::env::var("TMPDIR").unwrap_or_default();
    let roots = ["/tmp/", "/dev/shm/", "/var/tmp/"];
    roots.iter().any(|r| path.starts_with(r)) || (!tmpdir.is_empty() && path.starts_with(&tmpdir))
}

/// Build a success response (`ESC _G <id keys> ; OK ESC \`), unless suppressed
/// by the quiet level (`q>=1` mutes success).
fn respond(cmd: &GraphicsCommand, msg: &str) -> Outcome {
    if cmd.quiet >= 1 {
        return Outcome::default();
    }
    Outcome {
        response: Some(build_response(cmd, msg)),
        grid_op: None,
    }
}

/// Build an error response unless fully quiet (`q>=2`).
fn respond_error(cmd: &GraphicsCommand, msg: &str) -> Outcome {
    if cmd.quiet >= 2 {
        return Outcome::default();
    }
    Outcome {
        response: Some(build_response(cmd, msg)),
        grid_op: None,
    }
}

fn build_response(cmd: &GraphicsCommand, msg: &str) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use beer_protocols::codec::base64_encode;

    fn b64(data: &[u8]) -> Vec<u8> {
        base64_encode(data).into_bytes()
    }

    fn rgba_cmd(w: u32, h: u32, id: u32, action: Action) -> GraphicsCommand {
        GraphicsCommand {
            action,
            format: Format::Rgba,
            width: w,
            height: h,
            id,
            ..Default::default()
        }
    }

    #[test]
    fn transmit_rgba_stores_and_acks() {
        let mut g = Graphics::new();
        let px = vec![0xab; 2 * 2 * 4];
        let out = g.handle(rgba_cmd(2, 2, 1, Action::Transmit), &b64(&px), (8, 16));
        assert_eq!(out.response.as_deref(), Some(&b"\x1b_Gi=1;OK\x1b\\"[..]));
        let img = g.image(1).expect("stored");
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.current_rgba().len(), 16);
    }

    #[test]
    fn rgb_expands_to_rgba() {
        let mut g = Graphics::new();
        let px = vec![0x10; 2 * 3]; // 2x1 RGB
        let mut cmd = rgba_cmd(2, 1, 5, Action::Transmit);
        cmd.format = Format::Rgb;
        g.handle(cmd, &b64(&px), (8, 16));
        let img = g.image(5).unwrap();
        assert_eq!(
            img.current_rgba(),
            [0x10, 0x10, 0x10, 0xff, 0x10, 0x10, 0x10, 0xff]
        );
    }

    #[test]
    fn transmit_and_display_emits_placement() {
        let mut g = Graphics::new();
        let px = vec![0; 16 * 16 * 4];
        let out = g.handle(
            rgba_cmd(16, 16, 2, Action::TransmitAndDisplay),
            &b64(&px),
            (8, 16),
        );
        match out.grid_op {
            Some(GridOp::Place {
                image, cols, rows, ..
            }) => {
                assert_eq!(image, 2);
                assert_eq!(cols, 2); // 16px / 8px cell
                assert_eq!(rows, 1); // 16px / 16px cell
            }
            other => panic!("expected placement, got {other:?}"),
        }
        assert!(g.placement(2, 0).is_some());
    }

    #[test]
    fn chunked_direct_transmission_assembles() {
        let mut g = Graphics::new();
        let px = vec![0x7f; 4 * 4]; // 4x1 RGBA
        let full = base64_encode(&px).into_bytes();
        let (a, b) = full.split_at(8); // 8 is a multiple of 4
        let mut first = rgba_cmd(4, 1, 9, Action::Transmit);
        first.more = true;
        assert!(g.handle(first, a, (8, 16)).response.is_none());
        // Continuation carries only m=0.
        let last = GraphicsCommand {
            action: Action::Transmit,
            more: false,
            ..Default::default()
        };
        let out = g.handle(last, b, (8, 16));
        assert_eq!(out.response.as_deref(), Some(&b"\x1b_Gi=9;OK\x1b\\"[..]));
        assert_eq!(g.image(9).unwrap().current_rgba().len(), 16);
    }

    #[test]
    fn query_verifies_without_storing() {
        let mut g = Graphics::new();
        let px = vec![0; 2 * 2 * 4];
        let out = g.handle(rgba_cmd(2, 2, 3, Action::Query), &b64(&px), (8, 16));
        assert_eq!(out.response.as_deref(), Some(&b"\x1b_Gi=3;OK\x1b\\"[..]));
        assert!(g.image(3).is_none());
    }

    #[test]
    fn bad_payload_reports_error() {
        let mut g = Graphics::new();
        let out = g.handle(rgba_cmd(100, 100, 1, Action::Transmit), b"!!!!", (8, 16));
        let resp = out.response.expect("error response");
        assert!(resp.starts_with(b"\x1b_Gi=1;"));
        assert!(resp.windows(6).any(|w| w == b"EINVAL"));
    }

    #[test]
    fn quiet_suppresses_success() {
        let mut g = Graphics::new();
        let px = vec![0; 4];
        let mut cmd = rgba_cmd(1, 1, 1, Action::Transmit);
        cmd.quiet = 1;
        assert!(g.handle(cmd, &b64(&px), (8, 16)).response.is_none());
    }

    #[test]
    fn delete_all_clears() {
        let mut g = Graphics::new();
        g.handle(
            rgba_cmd(2, 2, 1, Action::TransmitAndDisplay),
            &b64(&[0; 16]),
            (8, 16),
        );
        let cmd = GraphicsCommand {
            action: Action::Delete,
            delete: b'A',
            ..Default::default()
        };
        let out = g.handle(cmd, &[], (8, 16));
        assert!(matches!(out.grid_op, Some(GridOp::Clear(ClearSpec::All))));
        assert!(g.image(1).is_none(), "uppercase delete frees data");
    }

    #[test]
    fn animation_frames_advance_on_tick() {
        let mut g = Graphics::new();
        // Root frame: a 1x1 red pixel.
        g.handle(
            rgba_cmd(1, 1, 1, Action::Transmit),
            &b64(&[0xff, 0, 0, 0xff]),
            (8, 16),
        );
        // Append a second frame (a=f): a 1x1 green pixel, default 40ms gap.
        let frame = GraphicsCommand {
            action: Action::Frame,
            format: Format::Rgba,
            width: 1,
            height: 1,
            id: 1,
            ..Default::default()
        };
        g.handle(frame, &b64(&[0, 0xff, 0, 0xff]), (8, 16));
        // Run looping (a=a, s=3).
        let run = GraphicsCommand {
            action: Action::Animate,
            id: 1,
            width: 3,
            ..Default::default()
        };
        g.handle(run, &[], (8, 16));
        assert!(g.is_animating());
        assert_eq!(
            &g.image(1).unwrap().current_rgba()[..4],
            &[0xff, 0, 0, 0xff]
        );
        // A short tick does not cross the 40ms gap; a full one advances a frame.
        assert!(!g.tick(10));
        assert!(g.tick(40));
        assert_eq!(
            &g.image(1).unwrap().current_rgba()[..4],
            &[0, 0xff, 0, 0xff]
        );
    }

    #[test]
    fn animate_selects_current_frame() {
        let mut g = Graphics::new();
        g.handle(
            rgba_cmd(1, 1, 1, Action::Transmit),
            &b64(&[1, 1, 1, 0xff]),
            (8, 16),
        );
        let frame = GraphicsCommand {
            action: Action::Frame,
            format: Format::Rgba,
            width: 1,
            height: 1,
            id: 1,
            ..Default::default()
        };
        g.handle(frame, &b64(&[2, 2, 2, 0xff]), (8, 16));
        // a=a,c=2 makes the second frame current without playing.
        let select = GraphicsCommand {
            action: Action::Animate,
            id: 1,
            c: 2,
            ..Default::default()
        };
        g.handle(select, &[], (8, 16));
        assert_eq!(&g.image(1).unwrap().current_rgba()[..4], &[2, 2, 2, 0xff]);
        assert!(
            !g.is_animating(),
            "selecting a frame does not start playback"
        );
    }
}
