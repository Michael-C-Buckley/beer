//! Backend event callbacks.

use std::{mem, os::fd::AsRawFd as _, path::PathBuf};

use beer_window::{
  App as WindowApp,
  ImeEvent,
  KeyEvent,
  Modifiers,
  PointerEvent,
  TouchEvent,
  WindowCtx,
  WindowId,
};

use super::{
  App,
  IPC_LISTEN_TOKEN,
  RESIZE_REFLOW_MS,
  WindowLaunch,
  WindowOverrides,
};
use crate::ipc;

impl WindowApp for App {
  fn start(&mut self, ctx: &mut dyn WindowCtx) {
    if self.server {
      match ipc::bind_listener() {
        Ok((listener, path)) => {
          let _ = listener.set_nonblocking(true);
          ctx.watch_readable(listener.as_raw_fd(), IPC_LISTEN_TOKEN);
          self.ipc_listener = Some(listener);
          self.ipc_socket = path;
        },
        Err(err) => {
          tracing::error!("bind daemon socket: {err}");
          ctx.exit(1);
        },
      }
    } else {
      let req = mem::take(&mut self.initial);
      let overrides = WindowOverrides {
        title:  req.title,
        app_id: req.app_id,
      };
      self.open(ctx, WindowLaunch {
        cwd: req.cwd.map(PathBuf::from),
        env: req.env,
        command: req.command,
        hold: req.hold,
        overrides,
        client: None,
      });
    }
    self.flush_redraw(ctx);
  }

  fn on_configure(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    width: u32,
    height: u32,
    activated: bool,
    resizing: bool,
  ) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    // A pixel-size change makes the backend reallocate the buffer ring under
    // new ids; drop the snapshots keyed by the old ids so they cannot linger.
    if (width, height) != (self.windows[idx].width, self.windows[idx].height) {
      self.windows[idx].snaps.clear();
    }
    self.windows[idx].width = width;
    self.windows[idx].height = height;
    self.windows[idx].focused = activated;
    if activated {
      self.focused = idx;
    }
    if self.windows[idx].session.is_none() {
      self.spawn_session(ctx, idx);
    } else if resizing {
      // Some compositors never send a final configure without RESIZING. Reflow
      // after a quiet interval, or immediately when one does arrive.
      let token = if let Some(token) = self.windows[idx].resize_token {
        ctx.cancel_timer(token);
        token
      } else {
        let token = self.alloc_token();
        self.windows[idx].resize_token = Some(token);
        token
      };
      ctx.arm_timer(token, RESIZE_REFLOW_MS);
    } else {
      if let Some(token) = self.windows[idx].resize_token.take() {
        ctx.cancel_timer(token);
      }
      self.resize_grid(idx);
    }
    if let Some(idx) = self.win_index(id) {
      self.windows[idx].needs_draw = true;
    }
    self.flush_redraw(ctx);
  }

  fn on_scale(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, scale120: u32) {
    if let Some(idx) = self.win_index(id) {
      self.set_scale(idx, scale120);
    }
    self.flush_redraw(ctx);
  }

  fn on_focus(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, focused: bool) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    self.windows[idx].focused = focused;
    if focused {
      self.focused = idx;
    } else {
      self.windows[idx].keys_down.clear();
    }
    self.report_focus(idx, focused);
    self.windows[idx].needs_draw = true;
    self.flush_redraw(ctx);
  }

  fn on_close(&mut self, ctx: &mut dyn WindowCtx, id: WindowId) {
    if let Some(idx) = self.win_index(id)
      && self.config.main.confirm_close
      && self.windows[idx]
        .session
        .as_ref()
        .is_some_and(|s| s.pty.has_foreground_job())
    {
      self.windows[idx].confirm_close = true;
      self.windows[idx].needs_draw = true;
      self.flush_redraw(ctx);
      return;
    }
    self.close(ctx, id);
    self.flush_redraw(ctx);
  }

  fn on_key(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: &KeyEvent,
    mods: Modifiers,
  ) {
    self.modifiers = mods;
    if let Some(idx) = self.win_index(id) {
      self.handle_key(ctx, idx, event);
    }
    self.flush_redraw(ctx);
  }

  fn on_key_release(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: &KeyEvent,
    mods: Modifiers,
  ) {
    self.modifiers = mods;
    if let Some(idx) = self.win_index(id) {
      self.handle_key_release(idx, event);
    }
    self.flush_redraw(ctx);
  }

  fn on_ime(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, event: ImeEvent) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    match event {
      ImeEvent::Enable => self.ime_set_cursor(ctx, idx),
      ImeEvent::Disable => {
        let win = &mut self.windows[idx];
        win.preedit.clear();
        win.ime_preedit_pending.clear();
        win.ime_commit_pending.clear();
        win.ime_delete_pending = (0, 0);
        win.needs_draw = true;
      },
      ImeEvent::Preedit(text) => self.windows[idx].ime_preedit_pending = text,
      ImeEvent::Commit(text) => {
        self.windows[idx].ime_commit_pending.push_str(&text);
      },
      ImeEvent::DeleteSurrounding { before, after } => {
        self.windows[idx].ime_delete_pending = (before, after);
      },
      ImeEvent::Done => self.ime_done(ctx, idx),
    }
    self.flush_redraw(ctx);
  }

  fn on_pointer(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: PointerEvent,
    mods: Modifiers,
  ) {
    self.modifiers = mods;
    if let Some(idx) = self.win_index(id) {
      self.on_pointer_event(ctx, idx, event);
    }
    self.flush_redraw(ctx);
  }

  fn on_touch(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    event: TouchEvent,
  ) {
    if let Some(idx) = self.win_index(id) {
      self.on_touch_event(ctx, idx, event);
    }
    self.flush_redraw(ctx);
  }

  fn on_paste(
    &mut self,
    ctx: &mut dyn WindowCtx,
    id: WindowId,
    data: &[u8],
    _primary: bool,
  ) {
    if let Some(idx) = self.win_index(id) {
      self.paste_bytes(idx, data);
    }
    self.flush_redraw(ctx);
  }

  fn clipboard_text(&self, primary: bool) -> Option<String> {
    let text = if primary {
      &self.primary_clip
    } else {
      &self.clipboard
    };
    Some(text.clone())
  }

  fn on_readable(&mut self, ctx: &mut dyn WindowCtx, token: u64) {
    if token == IPC_LISTEN_TOKEN {
      self.accept_clients(ctx);
    } else if let Some(idx) =
      self.windows.iter().position(|w| w.pty_token == token)
    {
      self.read_pty(ctx, idx);
    } else if self.ipc_clients.contains_key(&token) {
      self.read_client(ctx, token);
    }
    self.flush_redraw(ctx);
  }

  fn on_timer(&mut self, ctx: &mut dyn WindowCtx, token: u64) {
    if !self.handle_global_timer(ctx, token)
      && let Some((idx, kind)) = self.window_timer(token)
    {
      self.handle_window_timer(ctx, token, idx, kind);
    }
    self.flush_redraw(ctx);
  }

  fn on_reload(&mut self, ctx: &mut dyn WindowCtx) {
    self.reload_config(ctx);
    self.flush_redraw(ctx);
  }

  fn on_sigchld(&mut self, ctx: &mut dyn WindowCtx) {
    self.reap_children(ctx);
    self.flush_redraw(ctx);
  }

  fn render(&mut self, ctx: &mut dyn WindowCtx, id: WindowId) {
    if let Some(idx) = self.win_index(id) {
      self.paint(ctx, idx);
    }
  }
}
