//! PTY session lifecycle.

use std::{mem, os::fd::AsRawFd as _};

use beer_window::{WindowCtx, WindowId};

use super::{
  App,
  Session,
  cursor_shape_from,
  logical_at,
  phys_at,
  readable_now,
  status_code,
  write_all,
};
use crate::{
  ipc,
  pty::{Pty, SpawnOptions},
  theme::Theme,
  vt::{Progress, Term},
};

impl App {
  /// Size a freshly created window to the configured `initial-cols`/
  /// `initial-rows` when the compositor left the initial size to the client,
  /// leaving the logical size at its 1x1 sentinel. A compositor-dictated size
  /// is kept as-is.
  fn apply_initial_size(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let w = &self.windows[idx];
    if w.width > 1 && w.height > 1 {
      return;
    }
    let (m, scale120) = (w.metrics, w.scale120);
    let pad = (
      phys_at(self.config.main.pad_x, scale120),
      phys_at(self.config.main.pad_y, scale120),
    );
    let phys_w = u32::from(self.config.main.initial_cols) * m.width + 2 * pad.0;
    let phys_h =
      u32::from(self.config.main.initial_rows) * m.height + 2 * pad.1;
    let width = logical_at(phys_w, scale120).max(1);
    let height = logical_at(phys_h, scale120).max(1);
    self.windows[idx].width = width;
    self.windows[idx].height = height;
    ctx.request_size(self.windows[idx].id, width, height);
  }

  /// Spawn the shell for window `idx` at its current size and watch its master.
  #[expect(clippy::cast_possible_truncation, reason = "cell metrics fit u16")]
  pub(super) fn spawn_session(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    self.ensure_renderer(idx);
    self.apply_initial_size(ctx, idx);
    let (cols, rows) = self.grid_dims(&self.windows[idx]);
    let m = self.windows[idx].metrics;
    let cell = (m.width as u16, m.height as u16);
    let id = self.windows[idx].id;
    let cwd = self.windows[idx].pending_cwd.clone();
    let env = mem::take(&mut self.windows[idx].pending_env);
    let command = mem::take(&mut self.windows[idx].pending_command);
    let window_number = self.windows[idx].window_number;
    let pty = match Pty::spawn(SpawnOptions {
      cols,
      rows,
      cell,
      term: &self.config.main.term,
      cwd: cwd.as_deref(),
      env: &env,
      command: &command,
      window_id: window_number,
      shell_integration: self.config.shell_integration.enabled,
    }) {
      Ok(pty) => pty,
      Err(err) => {
        tracing::error!("spawn shell: {err:#}");
        self.close(ctx, id);
        return;
      },
    };
    ctx.watch_readable(pty.master().as_raw_fd(), self.windows[idx].pty_token);
    let mut term = Term::new(cols as usize, rows as usize);
    term.set_theme(Theme::from_config(&self.config.colors));
    let grid = term.grid_mut();
    grid.set_word_delimiters(self.config.main.word_delimiters.clone());
    grid.set_scrollback_cap(self.config.scrollback.lines);
    if let Some(shape) = cursor_shape_from(self.config.cursor.style.as_deref())
    {
      grid.set_cursor_shape(shape);
    }
    grid.set_cursor_blink(self.config.cursor.blink);
    self.windows[idx].session = Some(Session {
      pty,
      parser: vte::Parser::new(),
      term,
    });
    self.update_activity_timers(ctx);
  }

  /// Read available bytes from window `idx`'s pty, feed the parser,
  /// post-process.
  #[expect(
    clippy::absolute_paths,
    reason = "the pty read uses rustix's explicit platform io type"
  )]
  pub(super) fn read_pty(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let id = self.windows[idx].id;
    let cell = self.windows[idx].metrics;
    let mut buf = [0u8; 4096];
    // Drain everything the shell has queued before repainting, so a redraw the
    // application emits as one burst (e.g. a graphics frame swap) is never
    // shown half-applied. Each read follows a zero-timeout poll so the
    // blocking master never stalls the loop; the loop ends the moment no more
    // bytes are waiting, and a later arrival re-fires this level-triggered
    // source.
    let mut fed = false;
    loop {
      let res = {
        let Some(session) = self.windows[idx].session.as_ref() else {
          return;
        };
        if !readable_now(session.pty.master()) {
          break;
        }
        rustix::io::read(session.pty.master(), &mut buf)
      };
      let n = match res {
        Ok(0) => {
          self.child_exited(ctx, id);
          return;
        },
        Ok(n) => n,
        Err(rustix::io::Errno::INTR) => continue,
        Err(rustix::io::Errno::AGAIN) => break,
        Err(_) => {
          self.child_exited(ctx, id);
          return;
        },
      };
      if let Some(session) = self.windows[idx].session.as_mut() {
        let Session { parser, term, .. } = session;
        term.feed(parser, &buf[..n], (cell.width, cell.height));
      }
      fed = true;
    }
    if fed {
      self.after_feed(ctx, idx);
    }
  }

  /// Reap window `id`'s exited shell, mirror its status to any client, close
  /// it.
  fn child_exited(&mut self, ctx: &mut dyn WindowCtx, id: WindowId) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    let mut code = 0u8;
    if let Some(session) = self.windows[idx].session.as_mut() {
      match session.pty.wait() {
        Ok(status) => code = status_code(status),
        Err(err) => tracing::warn!("reap shell: {err}"),
      }
    }
    self.finish_child(ctx, id, code);
  }

  /// Propagate a reaped child's exit `code` (to the daemon client and, for the
  /// last window, the process code) and close its window.
  fn finish_child(&mut self, ctx: &mut dyn WindowCtx, id: WindowId, code: u8) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    if self.windows[idx].hold {
      ctx.unwatch(self.windows[idx].pty_token);
      self.windows[idx].held_exit = Some(code);
      self.windows[idx].needs_draw = true;
      return;
    }
    if self.windows.len() == 1 {
      self.exit_code = code;
    }
    if let Some(client) = self.windows[idx].client.take() {
      ipc::send_exit(client, code);
    }
    self.close(ctx, id);
  }

  /// SIGCHLD: reap any window whose child has exited, without blocking.
  /// Prompter than waiting for the pty to reach EOF.
  pub(super) fn reap_children(&mut self, ctx: &mut dyn WindowCtx) {
    let ids: Vec<WindowId> = self.windows.iter().map(|w| w.id).collect();
    for id in ids {
      let Some(idx) = self.win_index(id) else {
        continue;
      };
      if self.windows[idx].held_exit.is_some() {
        continue;
      }
      let code = self.windows[idx].session.as_mut().and_then(|s| {
        match s.pty.try_wait() {
          Ok(Some(status)) => Some(status_code(status)),
          Ok(None) => None,
          Err(err) => {
            tracing::warn!("reap shell: {err}");
            None
          },
        }
      });
      if let Some(code) = code {
        self.finish_child(ctx, id, code);
      }
    }
  }

  /// After feeding parsed output: send replies, sync title, apply OSC 52 /
  /// notifications / bell, request a repaint.
  fn after_feed(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let id = self.windows[idx].id;
    let (rang, ops, notes) = {
      let win = &mut self.windows[idx];
      let Some(session) = win.session.as_mut() else {
        return;
      };
      let reply = session.term.take_response();
      if !reply.is_empty() {
        let _ = write_all(session.pty.master(), &reply);
      }
      let new_title = match session.term.progress() {
        Some(Progress::Normal(percent)) => {
          format!("{percent}% - {}", session.term.title().unwrap_or("beer"))
        },
        Some(Progress::Error) => {
          format!("error - {}", session.term.title().unwrap_or("beer"))
        },
        Some(Progress::Paused) => {
          format!("paused - {}", session.term.title().unwrap_or("beer"))
        },
        Some(Progress::Indeterminate) => {
          format!("working - {}", session.term.title().unwrap_or("beer"))
        },
        None => session.term.title().unwrap_or("beer").to_owned(),
      };
      if new_title != win.title.as_deref().unwrap_or("beer") {
        win.title = Some(new_title.clone());
        ctx.set_title(id, &new_title);
      }
      (
        session.term.take_bell(),
        session.term.take_clipboard_ops(),
        session.term.take_notifications(),
      )
    };
    if !ops.is_empty() {
      self.handle_clipboard_ops(ctx, idx, ops);
    }
    for note in notes {
      self.send_notification(idx, &note);
    }
    if rang {
      self.ring_bell(ctx, idx);
    }
    self.update_activity_timers(ctx);
    ctx.request_redraw(id);
  }

  /// Tear a window down: unwatch its sources and remove it; exit when the last
  /// window closes unless this is a resident server.
  pub(super) fn close(&mut self, ctx: &mut dyn WindowCtx, id: WindowId) {
    let Some(idx) = self.win_index(id) else {
      return;
    };
    for tok in [
      Some(self.windows[idx].pty_token),
      self.windows[idx].autoscroll_token,
      self.windows[idx].resize_token,
      self.windows[idx].flash_token,
      self.windows[idx].sync_token,
      self.windows[idx].touch_token,
    ]
    .into_iter()
    .flatten()
    {
      ctx.unwatch(tok);
      ctx.cancel_timer(tok);
    }
    let code = self.windows[idx].held_exit.unwrap_or(0);
    if self.windows.len() == 1 && self.windows[idx].held_exit.is_some() {
      self.exit_code = code;
    }
    if let Some(client) = self.windows[idx].client.take() {
      ipc::send_exit(client, code);
    }
    self.windows.remove(idx);
    self.update_activity_timers(ctx);
    ctx.close_window(id);
    if self.windows.is_empty() {
      if !self.resident {
        ctx.exit(self.exit_code);
      }
    } else if self.focused >= self.windows.len() {
      self.focused = self.windows.len() - 1;
    }
  }
}
