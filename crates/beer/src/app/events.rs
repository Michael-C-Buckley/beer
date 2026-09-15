//! Window creation, IPC clients, and timers.

use std::{
  io::ErrorKind,
  os::{fd::AsRawFd as _, unix::net::UnixStream},
  path::PathBuf,
  time::Instant,
};

use beer_window::{DecorationMode, WindowCtx, WindowId, WindowOptions};

use super::{
  ANIM_MS,
  ANIM_TOKEN,
  App,
  BLINK_MS,
  BLINK_TOKEN,
  IPC_SWEEP_TOKEN,
  MAX_IPC_ACCEPTS_PER_WAKE,
  MAX_PENDING_IPC_CLIENTS,
  RAPID_BLINK_MS,
  RAPID_BLINK_TOKEN,
  WinState,
  WindowLaunch,
  WindowOverrides,
  WindowTimer,
  cursor_shape_from,
  sync_timer,
};
use crate::{
  bindings::Bindings,
  config::{Config, Decorations},
  ipc,
  theme::Theme,
};

impl App {
  pub(super) fn open(
    &mut self,
    ctx: &mut dyn WindowCtx,
    launch: WindowLaunch,
  ) -> WindowId {
    let id = ctx.open_window(&WindowOptions {
      app_id:      launch
        .overrides
        .app_id
        .clone()
        .unwrap_or_else(|| self.config.main.app_id.clone()),
      title:       launch
        .overrides
        .title
        .clone()
        .unwrap_or_else(|| self.config.main.title.clone()),
      maximized:   self.config.main.maximized,
      decorations: match self.config.main.decorations {
        Decorations::Server => DecorationMode::Server,
        Decorations::Client => DecorationMode::Client,
        Decorations::None => DecorationMode::None,
      },
    });
    let pty_token = self.alloc_token();
    let window_number = self.next_window;
    self
      .windows
      .push(WinState::new(id, launch, window_number, pty_token));
    self.next_window += 1;
    id
  }

  pub(super) fn accept_clients(&mut self, ctx: &mut dyn WindowCtx) {
    for _ in 0..MAX_IPC_ACCEPTS_PER_WAKE {
      let Some(listener) = self.ipc_listener.as_ref() else {
        return;
      };
      match listener.accept() {
        Ok((stream, _)) => self.register_client(ctx, stream),
        Err(e) if e.kind() == ErrorKind::WouldBlock => return,
        Err(e) => {
          tracing::warn!("ipc accept: {e}");
          return;
        },
      }
    }
  }

  fn register_client(&mut self, ctx: &mut dyn WindowCtx, stream: UnixStream) {
    if self.ipc_clients.len() >= MAX_PENDING_IPC_CLIENTS {
      return;
    }
    if stream.set_nonblocking(true).is_err() {
      return;
    }
    let token = self.alloc_token();
    ctx.watch_readable(stream.as_raw_fd(), token);
    self.ipc_clients.insert(
      token,
      (stream, ipc::RequestReader::default(), Instant::now()),
    );
    if !self.ipc_sweep_armed {
      self.ipc_sweep_armed = true;
      ctx.arm_timer(IPC_SWEEP_TOKEN, 1000);
    }
  }

  pub(super) fn read_client(&mut self, ctx: &mut dyn WindowCtx, token: u64) {
    let Some((stream, reader, _)) = self.ipc_clients.get_mut(&token) else {
      return;
    };
    match reader.read_from(&*stream) {
      Ok(None) => {},
      Ok(Some(req)) => {
        let client = stream.try_clone().ok();
        self.ipc_clients.remove(&token);
        ctx.unwatch(token);
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
          client,
        });
      },
      Err(err) => {
        tracing::warn!("read ipc request: {err}");
        self.ipc_clients.remove(&token);
        ctx.unwatch(token);
      },
    }
  }

  fn expire_clients(&mut self, ctx: &mut dyn WindowCtx) {
    let stale: Vec<u64> = self
      .ipc_clients
      .iter()
      .filter(|(_, (_, _, t))| t.elapsed().as_secs() >= 5)
      .map(|(&tok, _)| tok)
      .collect();
    for tok in stale {
      self.ipc_clients.remove(&tok);
      ctx.unwatch(tok);
    }
    if !self.ipc_clients.is_empty() {
      self.ipc_sweep_armed = true;
      ctx.arm_timer(IPC_SWEEP_TOKEN, 1000);
    }
  }

  pub(super) fn update_activity_timers(&mut self, ctx: &mut dyn WindowCtx) {
    let blinking = self.windows.iter().any(|window| {
      window
        .session
        .as_ref()
        .is_some_and(|session| session.term.grid().needs_blink())
    });
    sync_timer(ctx, &mut self.blink_armed, blinking, BLINK_TOKEN, BLINK_MS);
    let rapid = self.windows.iter().any(|window| {
      window
        .session
        .as_ref()
        .is_some_and(|session| session.term.grid().needs_rapid_blink())
    });
    sync_timer(
      ctx,
      &mut self.rapid_armed,
      rapid,
      RAPID_BLINK_TOKEN,
      RAPID_BLINK_MS,
    );
    let animating = self.windows.iter().any(|window| {
      window
        .session
        .as_ref()
        .is_some_and(|session| session.term.is_animating())
    });
    if animating && !self.anim_armed {
      self.anim_armed = true;
      ctx.arm_timer(ANIM_TOKEN, u64::from(ANIM_MS));
    } else if !animating && self.anim_armed {
      self.anim_armed = false;
      ctx.cancel_timer(ANIM_TOKEN);
    }
  }

  pub(super) fn handle_global_timer(
    &mut self,
    ctx: &mut dyn WindowCtx,
    token: u64,
  ) -> bool {
    match token {
      BLINK_TOKEN => {
        self.normal_blink_tick();
        self.update_activity_timers(ctx);
      },
      RAPID_BLINK_TOKEN => {
        self.rapid_blink_tick();
        self.update_activity_timers(ctx);
      },
      ANIM_TOKEN => {
        self.anim_armed = false;
        for window in &mut self.windows {
          if window
            .session
            .as_mut()
            .is_some_and(|session| session.term.animation_tick(ANIM_MS))
          {
            window.snaps.clear();
            window.needs_draw = true;
          }
        }
        self.update_activity_timers(ctx);
      },
      IPC_SWEEP_TOKEN => {
        self.ipc_sweep_armed = false;
        self.expire_clients(ctx);
      },
      _ => return false,
    }
    true
  }

  fn normal_blink_tick(&mut self) {
    self.blink_armed = false;
    self.blink_on = !self.blink_on;
    for window in &mut self.windows {
      if window
        .session
        .as_ref()
        .is_some_and(|session| session.term.grid().needs_blink())
      {
        window.needs_draw = true;
      }
    }
  }

  fn rapid_blink_tick(&mut self) {
    self.rapid_armed = false;
    self.rapid_on = !self.rapid_on;
    for window in &mut self.windows {
      if window
        .session
        .as_ref()
        .is_some_and(|session| session.term.grid().needs_rapid_blink())
      {
        window.needs_draw = true;
      }
    }
  }

  pub(super) fn window_timer(
    &self,
    token: u64,
  ) -> Option<(usize, WindowTimer)> {
    for (idx, window) in self.windows.iter().enumerate() {
      let kind = if window.flash_token == Some(token) {
        WindowTimer::Flash
      } else if window.sync_token == Some(token) {
        WindowTimer::Sync
      } else if window.resize_token == Some(token) {
        WindowTimer::Resize
      } else if window.touch_token == Some(token) {
        WindowTimer::Touch
      } else if window.autoscroll_token == Some(token) {
        WindowTimer::Autoscroll
      } else {
        continue;
      };
      return Some((idx, kind));
    }
    None
  }

  pub(super) fn handle_window_timer(
    &mut self,
    ctx: &mut dyn WindowCtx,
    token: u64,
    idx: usize,
    kind: WindowTimer,
  ) {
    match kind {
      WindowTimer::Flash => {
        let window = &mut self.windows[idx];
        window.flashing = false;
        window.snaps.clear();
        window.needs_draw = true;
        window.flash_token = None;
        ctx.cancel_timer(token);
      },
      WindowTimer::Sync => {
        if let Some(session) = self.windows[idx].session.as_mut() {
          session.term.grid_mut().set_sync(false);
        }
        self.windows[idx].sync_token = None;
        self.windows[idx].needs_draw = true;
        ctx.cancel_timer(token);
      },
      WindowTimer::Resize => {
        self.windows[idx].resize_token = None;
        self.resize_grid(idx);
        self.windows[idx].needs_draw = true;
        ctx.cancel_timer(token);
      },
      WindowTimer::Touch => self.start_touch_selection(idx),
      WindowTimer::Autoscroll => self.autoscroll_step(ctx, idx),
    }
  }

  fn start_touch_selection(&mut self, idx: usize) {
    self.windows[idx].touch_token = None;
    let point = self.windows[idx].touch.as_ref().and_then(|touch| {
      self.cell_at(&self.windows[idx], touch.x, touch.last_y)
    });
    if let Some((row, col)) = point
      && let Some(session) = self.windows[idx].session.as_mut()
    {
      session.term.grid_mut().start_selection(row, col);
      if let Some(touch) = self.windows[idx].touch.as_mut() {
        touch.selecting = true;
      }
      self.windows[idx].needs_draw = true;
    }
  }

  pub(super) fn reload_config(&mut self, ctx: &mut dyn WindowCtx) {
    let new = Config::load(&self.config_paths);
    self.bindings = Bindings::from_config(
      &new.key_bindings,
      &new.text_bindings,
      &new.mouse_bindings,
    );
    if new.main.font != self.config.main.font
      || new.main.font_size != self.config.main.font_size
    {
      self.font_size = new.main.font_size;
    }
    for window in &mut self.windows {
      if let Some(session) = window.session.as_mut() {
        session.term.set_theme(Theme::from_config(&new.colors));
        let grid = session.term.grid_mut();
        grid.set_word_delimiters(new.main.word_delimiters.clone());
        grid.set_scrollback_cap(new.scrollback.lines);
        if let Some(shape) = cursor_shape_from(new.cursor.style.as_deref()) {
          grid.set_cursor_shape(shape);
        }
        grid.set_cursor_blink(new.cursor.blink);
      }
    }
    self.config = new;
    // Font, padding, and blending may all have changed; drop every cached
    // renderer so each rebuilds from the new config.
    self.renderers.clear();
    for idx in 0..self.windows.len() {
      self.windows[idx].snaps.clear();
      self.resize_grid(idx);
      self.windows[idx].needs_draw = true;
    }
    self.update_activity_timers(ctx);
  }

  /// Request a repaint for every window whose displayed state changed since the
  /// last flush; called at each trait-method boundary.
  pub(super) fn flush_redraw(&mut self, ctx: &mut dyn WindowCtx) {
    for w in &mut self.windows {
      if w.needs_draw {
        w.needs_draw = false;
        ctx.request_redraw(w.id);
      }
    }
  }
}
