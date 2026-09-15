//! [`beer_window::WindowCtx`] for [`Platform`]: the platform actions the app
//! performs on windows - present a frame, set the title/cursor, own the
//! clipboard, open/close toplevels, and register event sources (pty, IPC,
//! timers) whose callbacks re-enter the app.

use std::{
  io::{self, Error, ErrorKind, Read as _, Write},
  os::fd::{BorrowedFd, RawFd},
  time::Duration,
};

use beer_window::{CursorIcon, Frame, WindowCtx, WindowId, WindowOptions};
use calloop::{
  Interest,
  Mode,
  PostAction,
  generic::Generic,
  timer::{TimeoutAction, Timer},
};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use smithay_client_toolkit::{
  activation::RequestData,
  data_device_manager::{ReadPipe, WritePipe},
  reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape,
  shell::{
    WaylandSurface,
    xdg::window::WindowDecorations,
  },
  shm::slot::SlotPool,
};
use wayland_protocols::wp::content_type::v1::client::wp_content_type_v1;

use crate::state::PlatformWindow;

/// MIME types beer offers and accepts for clipboard text.
const TEXT_MIMES: &[&str] = &[
  "text/plain;charset=utf-8",
  "text/plain;charset=UTF-8",
  "UTF8_STRING",
  "STRING",
  "text/plain",
  "TEXT",
];

/// Pick the first MIME we understand from an offer's advertised set.
fn pick_mime(mimes: &[String]) -> Option<String> {
  mimes
    .iter()
    .find(|m| TEXT_MIMES.contains(&m.as_str()))
    .cloned()
}

/// Initial shm pool size; buffers grow as the surface is configured.
const DEFAULT_POOL_BYTES: usize = 640 * 480 * 4;

/// Refuse selection transfers larger than this to keep an untrusted owner from
/// exhausting the terminal process.
const MAX_SELECTION_BYTES: usize = 16 * 1024 * 1024;

fn append_selection(data: &mut Vec<u8>, chunk: &[u8]) -> bool {
  if data.len().saturating_add(chunk.len()) > MAX_SELECTION_BYTES {
    return false;
  }
  data.extend_from_slice(chunk);
  true
}

fn write_selection(
  writer: &mut impl Write,
  text: &[u8],
  offset: &mut usize,
) -> io::Result<PostAction> {
  if text.is_empty() {
    return Ok(PostAction::Remove);
  }
  let written = match writer.write(&text[*offset..]) {
    Ok(written) => written,
    Err(err)
      if matches!(
        err.kind(),
        ErrorKind::Interrupted | ErrorKind::WouldBlock
      ) =>
    {
      return Ok(PostAction::Continue);
    },
    Err(err) => return Err(err),
  };
  if written == 0 {
    return Err(Error::from(ErrorKind::WriteZero));
  }
  *offset += written;
  Ok(if *offset == text.len() {
    PostAction::Remove
  } else {
    PostAction::Continue
  })
}

use crate::state::Platform;

impl Platform {
  fn win(&self, id: WindowId) -> Option<&PlatformWindow> {
    self.window_index(id).map(|i| &self.windows[i])
  }

  fn win_mut(&mut self, id: WindowId) -> Option<&mut PlatformWindow> {
    self.window_index(id).map(|i| &mut self.windows[i])
  }
}

impl WindowCtx for Platform {
  fn size(&self, id: WindowId) -> Option<(u32, u32)> {
    self.win(id).map(|w| (w.width, w.height))
  }

  fn scale120(&self, id: WindowId) -> u32 {
    self.win(id).map_or(120, |w| w.scale120)
  }

  fn set_title(&mut self, id: WindowId, title: &str) {
    if let Some(win) = self.win_mut(id)
      && win.title.as_deref() != Some(title)
    {
      win.window.set_title(title);
      win.title = Some(title.to_owned());
    }
  }

  fn set_fullscreen(&mut self, id: WindowId, on: bool) {
    if let Some(win) = self.win_mut(id) {
      win.fullscreen = on;
      if on {
        win.window.set_fullscreen(None);
      } else {
        win.window.unset_fullscreen();
      }
    }
  }

  fn request_size(&mut self, id: WindowId, width: u32, height: u32) {
    if let Some(win) = self.win_mut(id) {
      win.width = width.max(1);
      win.height = height.max(1);
    }
  }

  fn set_cursor(&mut self, id: WindowId, icon: CursorIcon) {
    let serial = self
      .window_index(id)
      .map_or(0, |i| self.windows[i].pointer_enter_serial);
    let Some(seat) = self.seats.get(self.active_seat) else {
      return;
    };
    let shape = match icon {
      CursorIcon::Default => Shape::Default,
      CursorIcon::Text => Shape::Text,
      CursorIcon::Pointer => Shape::Pointer,
      // Hiding attaches no cursor surface to the pointer.
      CursorIcon::Hidden => {
        if let Some(pointer) = seat.pointer.as_ref() {
          pointer.set_cursor(serial, None, 0, 0);
        }
        return;
      },
    };
    if let Some(dev) = seat.cursor_shape_device.as_ref() {
      dev.set_shape(serial, shape);
    }
  }

  fn set_idle_inhibit(&mut self, id: WindowId, inhibit: bool) {
    let Some(idx) = self.window_index(id) else {
      return;
    };
    if inhibit && self.windows[idx].idle_inhibitor.is_none() {
      if let Some(mgr) = &self.idle_inhibit_manager {
        let inhibitor = mgr.create_inhibitor(
          self.windows[idx].window.wl_surface(),
          &self.qh,
          (),
        );
        self.windows[idx].idle_inhibitor = Some(inhibitor);
      }
    } else if !inhibit
      && let Some(inhibitor) = self.windows[idx].idle_inhibitor.take()
    {
      inhibitor.destroy();
    }
  }

  fn request_attention(&mut self, id: WindowId) {
    if let Some(activation) = self.activation.as_ref()
      && let Some(win) = self.win(id)
    {
      let data = RequestData {
        app_id:          Some(win.app_id.clone()),
        seat_and_serial: None,
        surface:         Some(win.window.wl_surface().clone()),
        udata:           (),
      };
      activation.request_token(&self.qh, data);
    }
  }

  fn acquire(
    &mut self,
    id: WindowId,
    phys_w: u32,
    phys_h: u32,
  ) -> Option<Frame<'_>> {
    self.acquire_buffer(id, phys_w, phys_h)
  }

  fn present(
    &mut self,
    id: WindowId,
    buffer: u64,
    dirty: &[u32],
    cell_h: u32,
    pad_y: u32,
    full: bool,
  ) {
    self.present_buffer(id, buffer, dirty, cell_h, pad_y, full);
  }

  fn request_redraw(&mut self, id: WindowId) {
    if let Some(win) = self.win_mut(id) {
      win.wants_draw = true;
    }
  }

  fn set_ime_cursor(&mut self, _id: WindowId, x: i32, y: i32, w: i32, h: i32) {
    if let Some(seat) = self.seats.get(self.active_seat)
      && let Some(ti) = seat.text_input.as_ref()
    {
      ti.set_cursor_rectangle(x, y, w.max(1), h.max(1));
      ti.commit();
    }
  }

  fn claim_clipboard(&mut self, text: String) {
    let Some(device) = self
      .seats
      .get(self.active_seat)
      .and_then(|s| s.data_device.as_ref())
    else {
      return;
    };
    let source = self
      .data_device_manager
      .create_copy_paste_source(&self.qh, TEXT_MIMES.iter().copied());
    source.set_selection(device, self.serial);
    self.clipboard = text;
    self.copy_source = Some(source);
  }

  fn claim_primary(&mut self, text: String) {
    let (Some(manager), Some(device)) = (
      self.primary_manager.as_ref(),
      self
        .seats
        .get(self.active_seat)
        .and_then(|s| s.primary_device.as_ref()),
    ) else {
      return;
    };
    let source =
      manager.create_selection_source(&self.qh, TEXT_MIMES.iter().copied());
    source.set_selection(device, self.serial);
    self.primary_clip = text;
    self.primary_source = Some(source);
  }

  fn request_paste(&mut self, id: WindowId, primary: bool) {
    let offer = if primary {
      self
        .seats
        .get(self.active_seat)
        .and_then(|s| s.primary_device.as_ref())
        .and_then(|d| d.data().selection_offer())
        .and_then(|o| o.receive(o.with_mime_types(pick_mime)?).ok())
    } else {
      self
        .seats
        .get(self.active_seat)
        .and_then(|s| s.data_device.as_ref())
        .and_then(|d| d.data().selection_offer())
        .and_then(|o| {
          o.with_mime_types(pick_mime).and_then(|m| o.receive(m).ok())
        })
    };
    if let Some(pipe) = offer {
      self.read_paste(id, pipe, primary);
    }
  }

  fn open_window(&mut self, opts: &WindowOptions) -> WindowId {
    let id = self.alloc_window_id();
    let surface = self.compositor.create_surface(&self.qh);
    let window = self.xdg_shell.create_window(
      surface,
      match opts.decorations {
        beer_window::DecorationMode::Server => WindowDecorations::RequestServer,
        beer_window::DecorationMode::Client => WindowDecorations::RequestClient,
        beer_window::DecorationMode::None => WindowDecorations::None,
      },
      &self.qh,
    );
    window.set_title(&opts.title);
    window.set_app_id(&opts.app_id);
    window.set_min_size(Some((1, 1)));
    if opts.maximized {
      window.set_maximized();
    }

    let viewport = self
      .viewporter
      .as_ref()
      .map(|vp| vp.get_viewport(window.wl_surface(), &self.qh, ()));
    let fractional_scale = viewport.as_ref().and_then(|_| {
      self
        .fractional_manager
        .as_ref()
        .map(|mgr| mgr.get_fractional_scale(window.wl_surface(), &self.qh, id))
    });
    let content_type = self.content_type_manager.as_ref().map(|mgr| {
      mgr.get_surface_content_type(window.wl_surface(), &self.qh, ())
    });
    if let Some(ct) = &content_type {
      ct.set_content_type(wp_content_type_v1::Type::None);
    }

    let pool = match SlotPool::new(DEFAULT_POOL_BYTES, &self.shm) {
      Ok(pool) => pool,
      Err(err) => {
        tracing::error!("create shm slot pool: {err}");
        if self.windows.is_empty() {
          self.exit = true;
        }
        return id;
      },
    };

    let win = PlatformWindow {
      id,
      pool,
      window,
      idle_inhibitor: None,
      content_type,
      viewport,
      fractional_scale,
      scale120: 120,
      app_id: opts.app_id.clone(),
      title: None,
      fullscreen: false,
      width: 1,
      height: 1,
      frames: Vec::new(),
      next_buf_id: 0,
      buf_dims: (0, 0),
      frame_pending: false,
      wants_draw: false,
      pointer_enter_serial: 0,
      focused: false,
    };
    win.window.commit();
    self.windows.push(win);
    id
  }

  fn close_window(&mut self, id: WindowId) {
    let Some(idx) = self.window_index(id) else {
      return;
    };
    self.windows.remove(idx);
    self.touch_focus.retain(|_, w| *w != id);
    // Removing a window below the focused one shifts it down by one; keep the
    // index pointing at the same window rather than its neighbour.
    if idx < self.focused_window {
      self.focused_window -= 1;
    }
    if !self.windows.is_empty() && self.focused_window >= self.windows.len() {
      self.focused_window = self.windows.len() - 1;
    }
  }

  #[expect(
    unsafe_code,
    reason = "duplicating the app-owned fd to hand the loop an owned copy"
  )]
  fn watch_readable(&mut self, fd: RawFd, token: u64) {
    // SAFETY: `fd` is owned by the app for the source's lifetime; we only
    // duplicate it and never close the original here.
    let owned = match unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()
    {
      Ok(owned) => owned,
      Err(err) => {
        tracing::warn!("dup watched fd: {err}");
        return;
      },
    };
    let source = Generic::new(owned, Interest::READ, Mode::Level);
    let reg = self.loop_handle.insert_source(source, move |_, _, state| {
      state.app.on_readable(&mut state.plat, token);
      Ok(PostAction::Continue)
    });
    match reg {
      Ok(tok) => {
        self.sources.insert(token, tok);
      },
      Err(err) => tracing::warn!("watch fd: {err}"),
    }
  }

  fn unwatch(&mut self, token: u64) {
    if let Some(tok) = self.sources.remove(&token) {
      self.loop_handle.remove(tok);
    }
  }

  fn arm_timer(&mut self, token: u64, millis: u64) {
    let timer = Timer::from_duration(Duration::from_millis(millis));
    let reg = self
      .loop_handle
      .insert_source(timer, move |_, _meta, state| {
        state.app.on_timer(&mut state.plat, token);
        TimeoutAction::Drop
      });
    match reg {
      Ok(tok) => {
        self.sources.insert(token, tok);
      },
      Err(err) => tracing::warn!("arm timer: {err}"),
    }
  }

  fn cancel_timer(&mut self, token: u64) {
    if let Some(tok) = self.sources.remove(&token) {
      self.loop_handle.remove(tok);
    }
  }

  fn exit(&mut self, code: u8) {
    self.exit = true;
    self.exit_code = code;
  }
}

impl Platform {
  /// Drain a clipboard read-pipe on the loop; hand the bytes to the app at EOF.
  fn read_paste(&self, id: WindowId, pipe: ReadPipe, primary: bool) {
    let mut data: Vec<u8> = Vec::new();
    let reg = self
      .loop_handle
      .insert_source(pipe, move |(), file, state| {
        let mut tmp = [0u8; 4096];
        let mut file = file.as_ref();
        match file.read(&mut tmp) {
          Ok(0) => {
            state.app.on_paste(&mut state.plat, id, &data, primary);
            PostAction::Remove
          },
          Ok(n) => {
            if append_selection(&mut data, &tmp[..n]) {
              PostAction::Continue
            } else {
              tracing::warn!("selection exceeds {MAX_SELECTION_BYTES} bytes");
              PostAction::Remove
            }
          },
          Err(e)
            if matches!(
              e.kind(),
              ErrorKind::Interrupted | ErrorKind::WouldBlock
            ) =>
          {
            PostAction::Continue
          },
          Err(e) => {
            tracing::warn!("read paste pipe: {e}");
            PostAction::Remove
          },
        }
      });
    if let Err(err) = reg {
      tracing::warn!("register paste pipe: {err}");
    }
  }

  /// Queue clipboard text for a receiver without blocking the event loop.
  pub fn serve_selection(&self, text: String, pipe: WritePipe) {
    let flags = match fcntl_getfl(&pipe) {
      Ok(flags) => flags | OFlags::NONBLOCK,
      Err(err) => {
        tracing::warn!("get selection pipe flags: {err}");
        return;
      },
    };
    if let Err(err) = fcntl_setfl(&pipe, flags) {
      tracing::warn!("make selection pipe nonblocking: {err}");
      return;
    }
    let mut offset = 0;
    let reg = self.loop_handle.insert_source(pipe, move |(), file, _| {
      let mut file = file.as_ref();
      match write_selection(&mut file, text.as_bytes(), &mut offset) {
        Ok(action) => action,
        Err(err) => {
          tracing::warn!("write selection pipe: {err}");
          PostAction::Remove
        },
      }
    });
    if let Err(err) = reg {
      tracing::warn!("register selection pipe: {err}");
    }
  }
}

#[cfg(test)]
mod tests {
  use std::io::{self, ErrorKind, Write};

  use calloop::PostAction;

  use super::{MAX_SELECTION_BYTES, append_selection, write_selection};

  #[derive(Default)]
  struct PartialWriter {
    bytes: Vec<u8>,
    error: Option<ErrorKind>,
  }

  impl Write for PartialWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
      if let Some(error) = self.error.take() {
        return Err(error.into());
      }
      let len = bytes.len().min(2);
      self.bytes.extend_from_slice(&bytes[..len]);
      Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  #[test]
  fn selection_limit_rejects_the_overflowing_chunk() {
    let mut data = vec![0; MAX_SELECTION_BYTES - 1];
    assert!(!append_selection(&mut data, &[1, 2]));
    assert_eq!(data.len(), MAX_SELECTION_BYTES - 1);
  }

  #[test]
  fn selection_limit_accepts_exactly_the_limit() {
    let mut data = Vec::new();
    assert!(append_selection(&mut data, &vec![0; MAX_SELECTION_BYTES]));
    assert_eq!(data.len(), MAX_SELECTION_BYTES);
  }

  #[test]
  fn selection_writer_retries_partial_writes() {
    let mut writer = PartialWriter::default();
    let mut offset = 0;
    assert_eq!(
      write_selection(&mut writer, b"hello", &mut offset).unwrap(),
      PostAction::Continue
    );
    assert_eq!(
      write_selection(&mut writer, b"hello", &mut offset).unwrap(),
      PostAction::Continue
    );
    assert_eq!(
      write_selection(&mut writer, b"hello", &mut offset).unwrap(),
      PostAction::Remove
    );
    assert_eq!(writer.bytes, b"hello");
  }

  #[test]
  fn selection_writer_resumes_after_transient_errors() {
    let mut writer = PartialWriter::default();
    let mut offset = 0;
    write_selection(&mut writer, b"hello", &mut offset).unwrap();
    for error in [ErrorKind::WouldBlock, ErrorKind::Interrupted] {
      writer.error = Some(error);
      assert_eq!(
        write_selection(&mut writer, b"hello", &mut offset).unwrap(),
        PostAction::Continue
      );
      assert_eq!(offset, 2);
      assert_eq!(writer.bytes, b"he");
    }
    write_selection(&mut writer, b"hello", &mut offset).unwrap();
    assert_eq!(
      write_selection(&mut writer, b"hello", &mut offset).unwrap(),
      PostAction::Remove
    );
    assert_eq!(writer.bytes, b"hello");
  }

  #[test]
  fn selection_writer_handles_empty_and_failed_transfers() {
    let mut buffer = &mut [][..];
    let mut offset = 0;
    assert_eq!(
      write_selection(&mut buffer, b"", &mut offset).unwrap(),
      PostAction::Remove
    );
    assert_eq!(
      write_selection(&mut buffer, b"text", &mut offset)
        .unwrap_err()
        .kind(),
      ErrorKind::WriteZero
    );
    let mut writer = PartialWriter {
      error: Some(ErrorKind::BrokenPipe),
      ..Default::default()
    };
    assert_eq!(
      write_selection(&mut writer, b"text", &mut offset)
        .unwrap_err()
        .kind(),
      ErrorKind::BrokenPipe
    );
    assert_eq!(offset, 0);
  }
}
