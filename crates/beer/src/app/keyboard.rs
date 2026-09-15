//! Keyboard input and terminal action handling.

use std::{
  io::Write as _,
  path::PathBuf,
  process::{Command, Stdio},
};

use beer_protocols::key;
use beer_window::{KeyEvent, KeyKind, WindowCtx};

use super::{App, WindowLaunch, WindowOverrides, hint_labels, write_all};
use crate::bindings::Action;
impl App {
  pub(super) fn write_to_pty(&mut self, idx: usize, bytes: &[u8]) {
    if let Some(session) = self.windows[idx].session.as_mut()
      && let Err(err) = write_all(session.pty.master(), bytes)
    {
      tracing::warn!("write to pty: {err}");
    }
  }

  pub(super) fn send_to_shell(&mut self, idx: usize, bytes: &[u8]) {
    if let Some(session) = self.windows[idx].session.as_mut() {
      session.term.scroll_to_bottom();
      session.term.grid_mut().clear_selection();
      let _ = write_all(session.pty.master(), bytes);
    }
    self.windows[idx].needs_draw = true;
  }

  pub(super) fn handle_key(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    event: &KeyEvent,
  ) {
    let kind = if self.windows[idx].keys_down.insert(event.raw_code) {
      KeyKind::Press
    } else {
      KeyKind::Repeat
    };
    self.hide_pointer(ctx, idx);
    if self.windows[idx].confirm_close {
      self.confirm_key(ctx, idx, event);
      return;
    }
    if self.windows[idx].unicode_input.is_some() {
      self.unicode_key(idx, event);
      return;
    }
    if self.windows[idx].url_mode {
      self.url_key(ctx, idx, event);
      return;
    }
    if self.windows[idx].searching {
      self.search_key(idx, event);
      return;
    }
    if let Some(action) = self.bindings.action(event, self.modifiers) {
      self.dispatch_action(ctx, idx, action);
      return;
    }
    if let Some(text) = self.bindings.text(event, self.modifiers) {
      let bytes = text.to_vec();
      self.send_to_shell(idx, &bytes);
      return;
    }
    let (app_cursor, app_keypad, kitty) = self.windows[idx]
      .session
      .as_ref()
      .map_or((false, false, 0), |s| {
        let g = s.term.grid();
        (g.app_cursor(), g.app_keypad(), g.kitty_flags())
      });
    let bytes = if kitty != 0 {
      key::kitty_encode(event, self.modifiers, kitty, kind, app_cursor)
    } else {
      key::encode(event, self.modifiers, app_cursor, app_keypad)
    };
    if let Some(bytes) = bytes {
      self.send_to_shell(idx, &bytes);
    }
  }

  pub(super) fn handle_key_release(&mut self, idx: usize, event: &KeyEvent) {
    self.windows[idx].keys_down.remove(&event.raw_code);
    let (app_cursor, kitty) =
      self.windows[idx].session.as_ref().map_or((false, 0), |s| {
        (s.term.grid().app_cursor(), s.term.grid().kitty_flags())
      });
    if kitty == 0 {
      return;
    }
    if let Some(bytes) = key::kitty_encode(
      event,
      self.modifiers,
      kitty,
      KeyKind::Release,
      app_cursor,
    ) {
      self.send_to_shell(idx, &bytes);
    }
  }

  fn unicode_key(&mut self, idx: usize, event: &KeyEvent) {
    use beer_window::Keysym;
    match event.keysym {
      Keysym::Escape => self.windows[idx].unicode_input = None,
      Keysym::BackSpace => {
        if let Some(buf) = self.windows[idx].unicode_input.as_mut() {
          buf.pop();
        }
      },
      Keysym::Return | Keysym::KP_Enter | Keysym::space => {
        let buf = self.windows[idx].unicode_input.take().unwrap_or_default();
        if let Some(c) = u32::from_str_radix(buf.trim(), 16)
          .ok()
          .and_then(char::from_u32)
        {
          let mut bytes = [0u8; 4];
          let s = c.encode_utf8(&mut bytes).as_bytes().to_vec();
          self.send_to_shell(idx, &s);
        }
      },
      _ => {
        if let Some(text) = event.utf8.as_ref() {
          let hex: String =
            text.chars().filter(char::is_ascii_hexdigit).collect();
          if let Some(buf) = self.windows[idx].unicode_input.as_mut()
            && buf.len() + hex.len() <= 6
          {
            buf.push_str(&hex);
          }
        }
      },
    }
    self.windows[idx].needs_draw = true;
  }

  fn url_key(&mut self, ctx: &mut dyn WindowCtx, idx: usize, event: &KeyEvent) {
    use beer_window::Keysym;
    match event.keysym {
      Keysym::Escape => self.exit_url_mode(idx),
      Keysym::BackSpace => {
        self.windows[idx].url_input.pop();
        self.windows[idx].needs_draw = true;
      },
      _ => {
        let Some(text) = event.utf8.as_ref() else {
          return;
        };
        for c in text.chars().filter(char::is_ascii_alphabetic) {
          self.windows[idx].url_input.push(c.to_ascii_lowercase());
        }
        let win = &self.windows[idx];
        if let Some(i) = win.url_labels.iter().position(|l| *l == win.url_input)
        {
          let url = win.url_hits[i].url.clone();
          let copy = win.url_copy;
          self.exit_url_mode(idx);
          if copy {
            self.clipboard.clone_from(&url);
            ctx.claim_clipboard(url);
          } else {
            self.open_url(&url);
          }
        } else if !win.url_labels.iter().any(|l| l.starts_with(&win.url_input))
        {
          self.exit_url_mode(idx);
        } else {
          self.windows[idx].needs_draw = true;
        }
      },
    }
  }

  /// Handle a key while the close-confirmation prompt is up: `y` closes, `n`
  /// or Escape cancels.
  fn confirm_key(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    event: &KeyEvent,
  ) {
    use beer_window::Keysym;
    let id = self.windows[idx].id;
    match event.keysym {
      Keysym::y | Keysym::Y => {
        self.windows[idx].confirm_close = false;
        self.close(ctx, id);
      },
      Keysym::n | Keysym::N | Keysym::Escape => {
        self.windows[idx].confirm_close = false;
        self.windows[idx].needs_draw = true;
      },
      _ => {},
    }
  }

  fn search_key(&mut self, idx: usize, event: &KeyEvent) {
    use beer_window::Keysym;
    let win = &mut self.windows[idx];
    let Some(session) = win.session.as_mut() else {
      return;
    };
    let grid = session.term.grid_mut();
    match event.keysym {
      Keysym::Escape => {
        grid.clear_search();
        win.searching = false;
      },
      Keysym::Return | Keysym::KP_Enter | Keysym::Up | Keysym::Page_Up => {
        grid.search_step(false);
      },
      Keysym::Down | Keysym::Page_Down => grid.search_step(true),
      Keysym::BackSpace => {
        let mut q = grid.search_query().unwrap_or("").to_string();
        q.pop();
        grid.set_search(&q);
      },
      _ => {
        if let Some(text) = event.utf8.as_ref() {
          let printable: String =
            text.chars().filter(|c| !c.is_control()).collect();
          if !printable.is_empty() {
            let mut q = grid.search_query().unwrap_or("").to_string();
            q.push_str(&printable);
            grid.set_search(&q);
          }
        }
      },
    }
    win.needs_draw = true;
  }

  pub(super) fn dispatch_action(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    action: Action,
  ) {
    match action {
      Action::Copy => self.set_clipboard(ctx, idx),
      Action::Paste => ctx.request_paste(self.windows[idx].id, false),
      Action::PastePrimary => ctx.request_paste(self.windows[idx].id, true),
      Action::ScrollPageUp => self.scroll_page(idx, true),
      Action::ScrollPageDown => self.scroll_page(idx, false),
      Action::ScrollTop => self.scroll_view(idx, isize::MAX),
      Action::ScrollBottom => {
        if let Some(s) = self.windows[idx].session.as_mut() {
          s.term.scroll_to_bottom();
          self.windows[idx].needs_draw = true;
        }
      },
      Action::SearchStart => self.toggle_search(idx),
      Action::FontIncrease => {
        self.change_font_size(ctx, idx, self.font_size + 1);
      },
      Action::FontDecrease => {
        self.change_font_size(ctx, idx, self.font_size.saturating_sub(1));
      },
      Action::FontReset => {
        self.change_font_size(ctx, idx, self.config.main.font_size);
      },
      Action::Fullscreen => {
        let win = &mut self.windows[idx];
        win.fullscreen = !win.fullscreen;
        ctx.set_fullscreen(win.id, win.fullscreen);
      },
      Action::NewWindow => self.spawn_new_window(ctx, idx),
      Action::JumpPromptUp => self.jump_prompt(idx, true),
      Action::JumpPromptDown => self.jump_prompt(idx, false),
      Action::PipeCommandOutput => self.pipe_command_output(idx),
      Action::PipeVisible => self.pipe_visible(idx),
      Action::PipeScrollback => self.pipe_scrollback(idx),
      Action::UrlMode => self.enter_url_mode(idx, false),
      Action::UrlCopy => self.enter_url_mode(idx, true),
      Action::UnicodeInput => {
        self.windows[idx].unicode_input = Some(String::new());
        self.windows[idx].needs_draw = true;
      },
    }
  }

  pub(super) fn scroll_view(&mut self, idx: usize, delta: isize) {
    if let Some(s) = self.windows[idx].session.as_mut() {
      s.term.scroll_view(delta);
      self.windows[idx].needs_draw = true;
    }
  }

  #[expect(
    clippy::cast_possible_wrap,
    reason = "a page height never approaches isize::MAX"
  )]
  fn scroll_page(&mut self, idx: usize, up: bool) {
    if let Some(s) = self.windows[idx].session.as_mut() {
      let page = s.term.page() as isize;
      s.term.scroll_view(if up { page } else { -page });
      self.windows[idx].needs_draw = true;
    }
  }

  fn toggle_search(&mut self, idx: usize) {
    let win = &mut self.windows[idx];
    win.searching = !win.searching;
    if let Some(s) = win.session.as_mut() {
      if win.searching {
        s.term.grid_mut().set_search("");
      } else {
        s.term.grid_mut().clear_search();
      }
    }
    win.needs_draw = true;
  }

  fn jump_prompt(&mut self, idx: usize, up: bool) {
    if let Some(s) = self.windows[idx].session.as_mut() {
      s.term.grid_mut().jump_prompt(up);
      self.windows[idx].needs_draw = true;
    }
  }

  fn enter_url_mode(&mut self, idx: usize, copy: bool) {
    let Some(session) = self.windows[idx].session.as_ref() else {
      return;
    };
    let hits = session.term.grid().visible_urls();
    if hits.is_empty() {
      return;
    }
    let labels = hint_labels(hits.len());
    let win = &mut self.windows[idx];
    win.url_labels = labels;
    win.url_hits = hits;
    win.url_input = String::new();
    win.url_mode = true;
    win.url_copy = copy;
    win.needs_draw = true;
  }

  fn exit_url_mode(&mut self, idx: usize) {
    let win = &mut self.windows[idx];
    win.url_mode = false;
    win.url_copy = false;
    win.url_hits.clear();
    win.url_labels.clear();
    win.url_input.clear();
    win.snaps.clear();
    win.needs_draw = true;
  }

  fn spawn_new_window(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let cwd = self.windows[idx]
      .session
      .as_ref()
      .and_then(|s| s.term.cwd())
      .map(PathBuf::from);
    self.open(ctx, WindowLaunch {
      cwd,
      env: Vec::new(),
      command: Vec::new(),
      hold: false,
      overrides: WindowOverrides::default(),
      client: None,
    });
  }

  pub(super) fn alternate_scroll(
    &mut self,
    idx: usize,
    up: bool,
    count: isize,
  ) {
    let app_cursor = self.windows[idx]
      .session
      .as_ref()
      .is_some_and(|s| s.term.grid().app_cursor());
    let seq: &[u8] = match (up, app_cursor) {
      (true, false) => b"\x1b[A",
      (true, true) => b"\x1bOA",
      (false, false) => b"\x1b[B",
      (false, true) => b"\x1bOB",
    };
    for _ in 0..count {
      self.write_to_pty(idx, seq);
    }
  }

  #[expect(
    clippy::disallowed_methods,
    reason = "configured URL opener is a user feature"
  )]
  pub(super) fn open_url(&self, url: &str) {
    let Some((program, args)) = self.config.url.launch.split_first() else {
      return;
    };
    let _ = Command::new(program)
      .args(args)
      .arg(url)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .inspect_err(|err| tracing::warn!("open url {url:?}: {err}"));
  }

  fn pipe_command_output(&self, idx: usize) {
    let text = self.windows[idx]
      .session
      .as_ref()
      .and_then(|s| s.term.grid().last_command_output());
    if let Some(text) = text {
      self.pipe_text(idx, &text);
    }
  }

  fn pipe_visible(&self, idx: usize) {
    if let Some(s) = self.windows[idx].session.as_ref() {
      let text = s.term.grid().visible_text();
      self.pipe_text(idx, &text);
    }
  }

  fn pipe_scrollback(&self, idx: usize) {
    if let Some(s) = self.windows[idx].session.as_ref() {
      let text = s.term.grid().scrollback_text();
      self.pipe_text(idx, &text);
    }
  }

  /// Spawn the configured pipe command, feeding `text` to its stdin. Shared by
  /// the pipe-command-output, pipe-visible, and pipe-scrollback actions.
  #[expect(
    clippy::disallowed_methods,
    reason = "configured pipe command is a user feature"
  )]
  fn pipe_text(&self, idx: usize, text: &str) {
    let argv = &self.config.shell_integration.pipe_command;
    let Some((program, args)) = argv.split_first() else {
      return;
    };
    let session = self.windows[idx].session.as_ref();
    let mut cmd = Command::new(program);
    cmd
      .args(args)
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::null());
    if let Some(cwd) = session.and_then(|s| s.term.cwd()) {
      cmd.current_dir(cwd);
    }
    if let Ok(mut child) = cmd.spawn()
      && let Some(mut stdin) = child.stdin.take()
    {
      let _ = stdin.write_all(text.as_bytes());
    }
  }
}
