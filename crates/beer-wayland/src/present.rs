//! Buffer-ring management and surface presentation - the platform half of the
//! old `App::present`. The Grid-based damage diff and glyph rendering stay in
//! the app; here we only hand out a physical-pixel canvas and, once the app has
//! painted it, attach/scale/damage/commit the surface with a frame callback.

use beer_window::{Frame, WindowId};
use smithay_client_toolkit::{
  compositor::FrameCallbackData,
  shell::WaylandSurface,
};
use wayland_client::protocol::wl_shm;

use crate::state::{FrameBuf, MAX_BUFFERS, Platform};

impl Platform {
  /// Acquire a physical-pixel buffer to paint into: reuse one the compositor
  /// has released, else grow the ring up to [`MAX_BUFFERS`]. A resize (buffer
  /// size change) invalidates the whole ring first. Returns `None` when every
  /// buffer is still held - the app should keep `wants_draw` set; a buffer
  /// release re-drives the paint.
  #[expect(
    clippy::cast_possible_wrap,
    reason = "shm buffer dimensions are bounded compositor geometry"
  )]
  pub fn acquire_buffer(
    &mut self,
    id: WindowId,
    w: u32,
    h: u32,
  ) -> Option<Frame<'_>> {
    let idx = self.window_index(id)?;
    let win = &mut self.windows[idx];

    // A resize invalidates every buffer's contents and size.
    if win.buf_dims != (w, h) {
      win.frames.clear();
      win.buf_dims = (w, h);
    }

    // Reuse a released buffer, else allocate a fresh one.
    let mut reuse = None;
    for i in 0..win.frames.len() {
      if win.pool.canvas(&win.frames[i].buffer).is_some() {
        reuse = Some(i);
        break;
      }
    }
    let (fidx, fresh) = match reuse {
      Some(i) => (i, false),
      None if win.frames.len() < MAX_BUFFERS => {
        let stride = w as i32 * 4;
        match win.pool.create_buffer(
          w as i32,
          h as i32,
          stride,
          wl_shm::Format::Argb8888,
        ) {
          Ok((buffer, _)) => {
            let buf_id = win.next_buf_id;
            win.next_buf_id += 1;
            win.frames.push(FrameBuf { buffer, id: buf_id });
            (win.frames.len() - 1, true)
          },
          Err(err) => {
            tracing::error!("allocate shm buffer: {err}");
            return None;
          },
        }
      },
      // All buffers still held by the compositor; a release wakes us again.
      None => return None,
    };

    let buffer_id = win.frames[fidx].id;
    let pixels = win.pool.canvas(&win.frames[fidx].buffer)?;
    Some(Frame {
      pixels,
      id: buffer_id,
      fresh,
    })
  }

  /// Present the buffer identified by `buffer_id`: attach it, present at the
  /// logical size through the viewport (or integer buffer scale as a fallback),
  /// damage the `dirty` rows (or fully when `full`), commit, and request a
  /// frame callback. `cell_h`/`pad_y` place per-row damage rectangles.
  #[expect(
    clippy::cast_possible_wrap,
    reason = "shm buffer dimensions are bounded compositor geometry"
  )]
  pub fn present_buffer(
    &mut self,
    id: WindowId,
    buffer_id: u64,
    dirty: &[u32],
    cell_h: u32,
    pad_y: u32,
    full: bool,
  ) {
    let Some(idx) = self.window_index(id) else {
      return;
    };
    let qh = self.qh.clone();
    let win = &mut self.windows[idx];
    let Some(fidx) = win.frames.iter().position(|f| f.id == buffer_id) else {
      return;
    };
    let (w, h) = win.buf_dims;
    let surface = win.window.wl_surface();
    if let Err(err) = win.frames[fidx].buffer.attach_to(surface) {
      tracing::error!("attach buffer: {err}");
      return;
    }
    // With a viewport the buffer presents at the logical destination, so its
    // own scale stays 1; without one, fall back to integer buffer scale.
    if let Some(vp) = &win.viewport {
      surface.set_buffer_scale(1);
      vp.set_destination(win.width.max(1) as i32, win.height.max(1) as i32);
    } else {
      surface.set_buffer_scale((win.scale120 / 120).max(1) as i32);
    }
    if full {
      surface.damage_buffer(0, 0, w as i32, h as i32);
    } else {
      for &y in dirty {
        let top = pad_y as i32 + y as i32 * cell_h as i32;
        surface.damage_buffer(0, top, w as i32, cell_h as i32);
      }
    }
    surface.frame(&qh, FrameCallbackData(surface.clone()));
    win.window.commit();
    win.frame_pending = true;
  }
}
