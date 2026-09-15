//! Pointer reporting, selection, and touch input.

use std::{num::NonZeroU16, time::Instant};

use beer_protocols::mouse;
use beer_window::{
  CursorIcon,
  PointerButton,
  PointerEvent,
  TouchEvent,
  WindowCtx,
};

use super::{AUTOSCROLL_MS, App, MULTI_CLICK_MS, TOUCH_HOLD_MS, TouchState};
use crate::{
  bindings::MouseButton,
  grid::{MouseEncoding, MouseProtocol},
};

const fn button_code(b: PointerButton) -> Option<u8> {
  match b {
    PointerButton::Left => Some(0),
    PointerButton::Middle => Some(1),
    PointerButton::Right => Some(2),
    PointerButton::Other(_) => None,
  }
}

const fn mouse_button(b: PointerButton) -> Option<MouseButton> {
  match b {
    PointerButton::Left => Some(MouseButton::Left),
    PointerButton::Middle => Some(MouseButton::Middle),
    PointerButton::Right => Some(MouseButton::Right),
    PointerButton::Other(_) => None,
  }
}

impl App {
  #[expect(
    clippy::cast_possible_truncation,
    reason = "scroll-line counts are clamped to a small range"
  )]
  pub(super) fn on_pointer_event(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    event: PointerEvent,
  ) {
    self.ensure_renderer(idx);
    let cell_h = f64::from(self.windows[idx].metrics.height);
    match event {
      PointerEvent::Enter { x, y, .. } => {
        self.windows[idx].pointer_pos = (x, y);
        self.windows[idx].pointer_hidden = false;
        self.update_hover(ctx, idx);
        self.pointer_drag(ctx, idx);
      },
      PointerEvent::Leave => {},
      PointerEvent::Motion { x, y } => {
        self.windows[idx].pointer_pos = (x, y);
        self.reveal_pointer(ctx, idx);
        if self.try_report_motion(idx) {
          return;
        }
        if !self.windows[idx].selecting {
          self.update_hover(ctx, idx);
        }
        self.pointer_drag(ctx, idx);
      },
      PointerEvent::Press { x, y, button, .. } => {
        self.windows[idx].pointer_pos = (x, y);
        if let Some(code) = button_code(button)
          && self.try_report_button(idx, code, true)
        {
          self.windows[idx].pressed_button = Some(code);
          return;
        }
        if let Some(mb) = mouse_button(button)
          && let Some(action) = self.bindings.mouse_action(mb, self.modifiers)
        {
          self.dispatch_action(ctx, idx, action);
          return;
        }
        if button == PointerButton::Left {
          let cell = self.cell_at(&self.windows[idx], x, y);
          self.windows[idx].press_cell = cell;
          self.pointer_press(idx);
        }
      },
      PointerEvent::Release { x, y, button } => {
        self.windows[idx].pointer_pos = (x, y);
        if let Some(code) = button_code(button)
          && self.try_report_button(idx, code, false)
        {
          if self.windows[idx].pressed_button == Some(code) {
            self.windows[idx].pressed_button = None;
          }
          return;
        }
        if button == PointerButton::Left {
          self.maybe_open_clicked_link(idx);
          self.pointer_release(ctx, idx);
        }
      },
      PointerEvent::Axis { scroll, .. } => {
        if scroll.dy == 0.0 {
          return;
        }
        let mult = self.config.mouse.scroll_multiplier.max(0.0);
        let per = if scroll.discrete {
          scroll.dy.abs() * 3.0
        } else if cell_h > 0.0 {
          scroll.dy.abs() / cell_h
        } else {
          return;
        };
        let lines = (per * mult).ceil().max(1.0) as isize;
        let up = scroll.dy < 0.0;
        if self.mouse_reporting(idx) {
          let code = if up { 64 } else { 65 };
          for _ in 0..lines.clamp(1, 8) {
            self.try_report_button(idx, code, true);
          }
          return;
        }
        let alt = self.windows[idx]
          .session
          .as_ref()
          .is_some_and(|s| s.term.grid().alt_active());
        if alt && self.config.mouse.alternate_scroll {
          self.alternate_scroll(idx, up, lines.clamp(1, 8));
          return;
        }
        self.scroll_view(idx, if up { lines } else { -lines });
      },
    }
  }

  #[expect(
    clippy::cast_possible_truncation,
    reason = "a click interval in ms is far below u32::MAX"
  )]
  fn pointer_press(&mut self, idx: usize) {
    let (px, py) = self.windows[idx].pointer_pos;
    let Some((row, col)) = self.cell_at(&self.windows[idx], px, py) else {
      return;
    };
    let count = match self.windows[idx].last_click {
      Some((t, r, c, n))
        if t.elapsed().as_millis() as u32 <= MULTI_CLICK_MS
          && r == row
          && c == col =>
      {
        n % 3 + 1
      },
      _ => 1,
    };
    self.windows[idx].last_click = Some((Instant::now(), row, col, count));
    let ctrl = self.modifiers.ctrl;
    let win = &mut self.windows[idx];
    let Some(session) = win.session.as_mut() else {
      return;
    };
    let grid = session.term.grid_mut();
    match count {
      2 => grid.select_word(row, col),
      3 => grid.select_line(row),
      _ if ctrl => grid.start_block_selection(row, col),
      _ => grid.start_selection(row, col),
    }
    win.selecting = true;
    win.needs_draw = true;
  }

  fn pointer_drag(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if !self.windows[idx].selecting {
      return;
    }
    let (px, py) = self.windows[idx].pointer_pos;
    let cell = self.cell_at(&self.windows[idx], px, py);
    if let Some((row, col)) = cell
      && let Some(session) = self.windows[idx].session.as_mut()
    {
      session.term.grid_mut().extend_selection(row, col);
      self.windows[idx].needs_draw = true;
    }
    // Arm/disarm edge autoscroll.
    let (py, height) =
      (self.windows[idx].pointer_pos.1, self.windows[idx].height);
    let dir = if py < 0.0 {
      1
    } else if py >= f64::from(height) {
      -1
    } else {
      0
    };
    self.windows[idx].autoscroll = dir;
    if dir != 0 && self.windows[idx].autoscroll_token.is_none() {
      let tok = self.alloc_token();
      self.windows[idx].autoscroll_token = Some(tok);
      ctx.arm_timer(tok, 0);
    }
  }

  pub(super) fn autoscroll_step(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
  ) {
    if !self.windows[idx].selecting || self.windows[idx].autoscroll == 0 {
      if let Some(tok) = self.windows[idx].autoscroll_token.take() {
        ctx.cancel_timer(tok);
      }
      return;
    }
    let dir = self.windows[idx].autoscroll;
    if let Some(session) = self.windows[idx].session.as_mut() {
      session.term.scroll_view(dir);
    }
    let height = self.windows[idx].height;
    let edge_y = if dir > 0 {
      0.0
    } else {
      f64::from(height) - 1.0
    };
    let px = self.windows[idx].pointer_pos.0;
    let cell = self.cell_at(&self.windows[idx], px, edge_y);
    if let Some((row, col)) = cell
      && let Some(session) = self.windows[idx].session.as_mut()
    {
      session.term.grid_mut().extend_selection(row, col);
    }
    self.windows[idx].needs_draw = true;
    if let Some(tok) = self.windows[idx].autoscroll_token {
      ctx.arm_timer(tok, AUTOSCROLL_MS);
    }
  }

  fn pointer_release(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if !self.windows[idx].selecting {
      return;
    }
    self.windows[idx].selecting = false;
    self.windows[idx].autoscroll = 0;
    if let Some(tok) = self.windows[idx].autoscroll_token.take() {
      ctx.cancel_timer(tok);
    }
    self.set_primary(ctx, idx);
  }

  fn mouse_reporting(&self, idx: usize) -> bool {
    self.windows[idx]
      .session
      .as_ref()
      .is_some_and(|s| s.term.grid().mouse_protocol() != MouseProtocol::Off)
      && !self.modifiers.shift
  }

  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel→cell mapping is clamped to the grid bounds"
  )]
  fn report_screen_cell(&self, idx: usize) -> Option<(usize, usize)> {
    let w = &self.windows[idx];
    let session = w.session.as_ref()?;
    let m = w.metrics;
    let (pad_x, pad_y) = (
      f64::from(self.to_phys(w, self.config.main.pad_x)),
      f64::from(self.to_phys(w, self.config.main.pad_y)),
    );
    let (ppx, ppy) = (
      self.to_phys_f(w, w.pointer_pos.0),
      self.to_phys_f(w, w.pointer_pos.1),
    );
    let grid = session.term.grid();
    let col = ((ppx - pad_x).max(0.0) as usize / m.width as usize)
      .min(grid.cols().saturating_sub(1));
    let row = ((ppy - pad_y).max(0.0) as usize / m.height as usize)
      .min(grid.rows().saturating_sub(1));
    Some((col, row))
  }

  /// The pointer position in physical pixels relative to the grid origin, for
  /// SGR-pixel (1016) mouse reports.
  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the pointer offset is non-negative and bounded by the surface"
  )]
  fn report_screen_pixel(&self, idx: usize) -> Option<(usize, usize)> {
    let w = &self.windows[idx];
    w.session.as_ref()?;
    let (pad_x, pad_y) = (
      f64::from(self.to_phys(w, self.config.main.pad_x)),
      f64::from(self.to_phys(w, self.config.main.pad_y)),
    );
    let x = (self.to_phys_f(w, w.pointer_pos.0) - pad_x).max(0.0) as usize;
    let y = (self.to_phys_f(w, w.pointer_pos.1) - pad_y).max(0.0) as usize;
    Some((x, y))
  }

  /// The coordinate a mouse report carries: pixels for the SGR-pixel encoding,
  /// otherwise the `cell` already resolved for deduplication.
  fn report_coord(
    &self,
    idx: usize,
    enc: MouseEncoding,
    cell: (usize, usize),
  ) -> (usize, usize) {
    if enc == MouseEncoding::SgrPixel {
      self.report_screen_pixel(idx).unwrap_or(cell)
    } else {
      cell
    }
  }

  fn try_report_button(&mut self, idx: usize, code: u8, pressed: bool) -> bool {
    let Some((proto, enc)) = self.windows[idx].session.as_ref().map(|s| {
      (
        s.term.grid().mouse_protocol(),
        s.term.grid().mouse_encoding(),
      )
    }) else {
      return false;
    };
    if proto == MouseProtocol::Off || self.modifiers.shift {
      return false;
    }
    if (pressed || proto != MouseProtocol::X10)
      && let Some((col, row)) = self.report_screen_cell(idx)
    {
      let (x, y) = self.report_coord(idx, enc, (col, row));
      let bytes =
        mouse::encode_mouse(enc, code, x, y, pressed, false, self.modifiers);
      self.write_to_pty(idx, &bytes);
      self.windows[idx].last_report_cell = Some((col, row));
    }
    true
  }

  fn try_report_motion(&mut self, idx: usize) -> bool {
    let Some((proto, enc)) = self.windows[idx].session.as_ref().map(|s| {
      (
        s.term.grid().mouse_protocol(),
        s.term.grid().mouse_encoding(),
      )
    }) else {
      return false;
    };
    if proto == MouseProtocol::Off || self.modifiers.shift {
      return false;
    }
    let wants = match proto {
      MouseProtocol::Any => true,
      MouseProtocol::Button => self.windows[idx].pressed_button.is_some(),
      _ => false,
    };
    if wants
      && let Some((col, row)) = self.report_screen_cell(idx)
      && self.windows[idx].last_report_cell != Some((col, row))
    {
      let code = self.windows[idx].pressed_button.unwrap_or(3);
      let (x, y) = self.report_coord(idx, enc, (col, row));
      let bytes =
        mouse::encode_mouse(enc, code, x, y, true, true, self.modifiers);
      self.write_to_pty(idx, &bytes);
      self.windows[idx].last_report_cell = Some((col, row));
    }
    true
  }

  pub(super) fn report_focus(&mut self, idx: usize, focused: bool) {
    if self.windows[idx]
      .session
      .as_ref()
      .is_some_and(|s| s.term.grid().focus_events())
    {
      self.write_to_pty(idx, if focused { b"\x1b[I" } else { b"\x1b[O" });
    }
  }

  fn link_under_pointer(&self, idx: usize) -> Option<NonZeroU16> {
    let (px, py) = self.windows[idx].pointer_pos;
    let (row, col) = self.cell_at(&self.windows[idx], px, py)?;
    self.windows[idx]
      .session
      .as_ref()?
      .term
      .grid()
      .link_at(row, col)
  }

  /// Hide the pointer while the user types, like `foot`. Restored on the next
  /// pointer motion by [`Self::reveal_pointer`].
  pub(super) fn hide_pointer(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if !self.windows[idx].pointer_hidden {
      self.windows[idx].pointer_hidden = true;
      ctx.set_cursor(self.windows[idx].id, CursorIcon::Hidden);
    }
  }

  /// Reveal a pointer hidden by typing, restoring the hover-appropriate cursor.
  fn reveal_pointer(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if self.windows[idx].pointer_hidden {
      self.windows[idx].pointer_hidden = false;
      self.update_hover(ctx, idx);
    }
  }

  fn update_hover(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let link = self.link_under_pointer(idx);
    let id = self.windows[idx].id;
    if link != self.windows[idx].hovered_link {
      self.windows[idx].hovered_link = link;
      self.windows[idx].snaps.clear();
      self.windows[idx].needs_draw = true;
    }
    // A hand over hyperlinks, the I-beam otherwise. Set it every hover update
    // so entering the window applies it even when the hovered link is
    // unchanged.
    ctx.set_cursor(
      id,
      if link.is_some() {
        CursorIcon::Pointer
      } else {
        CursorIcon::Text
      },
    );
  }

  fn maybe_open_clicked_link(&self, idx: usize) {
    let (px, py) = self.windows[idx].pointer_pos;
    let Some((row, col)) = self.cell_at(&self.windows[idx], px, py) else {
      return;
    };
    if self.windows[idx].press_cell != Some((row, col)) {
      return;
    }
    let uri = self.windows[idx].session.as_ref().and_then(|s| {
      s.term
        .grid()
        .link_at(row, col)
        .and_then(|id| s.term.grid().link_uri(id))
        .map(str::to_owned)
    });
    if let Some(uri) = uri {
      self.open_url(&uri);
    }
  }

  pub(super) fn on_touch_event(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    event: TouchEvent,
  ) {
    self.ensure_renderer(idx);
    match event {
      TouchEvent::Down { id, x, y } => self.touch_down(ctx, idx, id, x, y),
      TouchEvent::Up { id } => self.touch_up(ctx, idx, id),
      TouchEvent::Cancel => self.clear_touch(ctx, idx),
      TouchEvent::Motion { id, x, y } => {
        self.touch_motion(ctx, idx, id, x, y);
      },
    }
  }

  fn touch_down(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    id: i32,
    x: f64,
    y: f64,
  ) {
    if self.windows[idx].touch.is_some() {
      return;
    }
    let token = self.alloc_token();
    self.windows[idx].touch = Some(TouchState {
      id,
      x,
      start_y: y,
      last_y: y,
      acc: 0.0,
      selecting: false,
    });
    self.windows[idx].touch_token = Some(token);
    ctx.arm_timer(token, TOUCH_HOLD_MS);
  }

  fn touch_up(&mut self, ctx: &mut dyn WindowCtx, idx: usize, id: i32) {
    let Some(touch) = self.windows[idx].touch.as_ref() else {
      return;
    };
    if touch.id != id {
      return;
    }
    if touch.selecting {
      self.set_primary(ctx, idx);
    }
    self.clear_touch(ctx, idx);
  }

  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "touch scroll deltas are small line counts"
  )]
  fn touch_motion(
    &mut self,
    ctx: &mut dyn WindowCtx,
    idx: usize,
    id: i32,
    x: f64,
    y: f64,
  ) {
    let cell_h = f64::from(self.windows[idx].metrics.height);
    let Some(mut touch) = self.windows[idx].touch.take() else {
      return;
    };
    if touch.id != id {
      self.windows[idx].touch = Some(touch);
      return;
    }
    touch.x = x;
    if touch.selecting {
      self.extend_touch_selection(idx, x, y);
      self.windows[idx].touch = Some(touch);
      return;
    }
    if (y - touch.start_y).abs() >= cell_h / 2.0
      && let Some(token) = self.windows[idx].touch_token.take()
    {
      ctx.cancel_timer(token);
    }
    touch.acc += y - touch.last_y;
    touch.last_y = y;
    let lines = (touch.acc / cell_h) as isize;
    if lines != 0 {
      touch.acc = (lines as f64).mul_add(-cell_h, touch.acc);
      if let Some(session) = self.windows[idx].session.as_mut() {
        session.term.scroll_view(lines);
        self.windows[idx].needs_draw = true;
      }
    }
    self.windows[idx].touch = Some(touch);
  }

  fn extend_touch_selection(&mut self, idx: usize, x: f64, y: f64) {
    if let Some((row, col)) = self.cell_at(&self.windows[idx], x, y)
      && let Some(session) = self.windows[idx].session.as_mut()
    {
      session.term.grid_mut().extend_selection(row, col);
      self.windows[idx].needs_draw = true;
    }
  }

  fn clear_touch(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    if let Some(token) = self.windows[idx].touch_token.take() {
      ctx.cancel_timer(token);
    }
    self.windows[idx].touch = None;
  }
}
