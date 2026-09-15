//! Alternate-screen, viewport, and prompt-history operations.

use std::mem;

#[cfg(test)] use super::Flags;
use super::{
  Cell,
  Cursor,
  Grid,
  Line,
  MouseEncoding,
  MouseProtocol,
  PromptKind,
  default_tabs,
};

impl Grid {
  pub fn enter_alt_screen(&mut self) {
    if self.alt_saved.is_some() {
      return;
    }
    self.view_offset = 0;
    let blank = vec![Line::blank(self.cols); self.rows];
    self.alt_saved = Some(mem::replace(&mut self.lines, blank));
  }

  pub fn leave_alt_screen(&mut self) {
    if let Some(main) = self.alt_saved.take() {
      self.lines = main;
    }
  }

  /// RIS (`ESC c`): return to the main screen and reset the grid-side modes to
  /// their power-on defaults, then clear the screen. Scrollback is kept.
  pub fn hard_reset(&mut self) {
    self.leave_alt_screen();
    self.reset_pen();
    self.top = 0;
    self.bottom = self.rows - 1;
    self.left = 0;
    self.right = self.cols - 1;
    self.lr_margins = false;
    self.autowrap = true;
    self.origin = false;
    self.insert = false;
    self.wrap_pending = false;
    self.tabs = default_tabs(self.cols);
    self.cursor = Cursor::default();
    self.saved = Cursor::default();
    self.cursor_visible = true;
    self.cursor_color = None;
    self.app_cursor = false;
    self.app_keypad = false;
    self.bracketed_paste = false;
    self.sync = false;
    self.focus_events = false;
    self.mouse_protocol = MouseProtocol::Off;
    self.mouse_encoding = MouseEncoding::X10;
    self.kitty_current = 0;
    self.kitty_stack.clear();
    self.erase_display(2);
  }

  /// Scroll the viewport by `delta` lines: positive = back into history,
  /// negative = toward the live screen. No-op on the alternate screen.
  #[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "scrollback positions are bounded by the grid and adjusted in \
              signed coordinates"
  )]
  pub fn scroll_view(&mut self, delta: isize) {
    if self.alt_saved.is_some() {
      return;
    }
    let max = self.scrollback.len() as isize;
    self.view_offset =
      (self.view_offset as isize + delta).clamp(0, max) as usize;
  }

  pub const fn scroll_to_bottom(&mut self) {
    self.view_offset = 0;
  }

  /// Whether the viewport is showing the live screen (not scrolled back).
  pub const fn view_at_bottom(&self) -> bool {
    self.view_offset == 0
  }

  /// Lines above the live bottom and the maximum available history distance.
  pub fn scroll_position(&self) -> (usize, usize) {
    (self.view_offset, self.scrollback.len())
  }

  /// One page (a screenful) of lines, for page-scroll bindings.
  pub fn page(&self) -> usize {
    self.rows.max(1)
  }

  /// The cells shown at viewport row `y` (0 = top of the window), accounting
  /// for the scrollback offset. May differ from `cols` in width if the line
  /// predates a resize (no reflow yet), so callers must not assume length.
  pub fn view_row(&self, y: usize) -> &[Cell] {
    let start = self.scrollback.len() - self.view_offset;
    let idx = start + y;
    if idx < self.scrollback.len() {
      &self.scrollback[idx].cells
    } else {
      &self.lines[idx - self.scrollback.len()].cells
    }
  }

  /// For a viewport cell `(y, x)` that is the left edge of a text-sizing block
  /// (its `dx == 0`), return the block's leading cell - which holds the text
  /// and the full descriptor - together with this row's `dy` within the block.
  /// `None` if `(y, x)` is not a left-edge sized cell, or the leading row is
  /// scrolled above the viewport top (the block is then clipped, not drawn).
  pub fn sized_lead(&self, y: usize, x: usize) -> Option<(&Cell, usize)> {
    let dy = {
      let s = self.view_row(y).get(x)?.sized.as_ref()?;
      if s.dx != 0 {
        return None;
      }
      s.dy as usize
    };
    if dy > y {
      return None;
    }
    let lead = self.view_row(y - dy).get(x)?;
    lead
      .sized
      .as_ref()
      .filter(|ls| ls.dx == 0 && ls.dy == 0)
      .map(|_| (lead, dy))
  }

  /// The absolute row currently shown at viewport row `y`.
  pub fn view_to_abs(&self, y: usize) -> usize {
    self.scrollback.len() - self.view_offset + y
  }

  /// The line at an absolute row (scrollback first, then the live screen).
  pub(super) fn line_at_abs(&self, abs: usize) -> &Line {
    if abs < self.scrollback.len() {
      &self.scrollback[abs]
    } else {
      &self.lines[abs - self.scrollback.len()]
    }
  }

  /// Cells of an absolute row (scrollback first, then the live screen).
  pub(super) fn abs_row(&self, row: usize) -> &[Cell] {
    if row < self.scrollback.len() {
      &self.scrollback[row].cells
    } else {
      &self.lines[row - self.scrollback.len()].cells
    }
  }

  /// Total rows across scrollback and the live screen.
  pub(super) fn total_lines(&self) -> usize {
    self.scrollback.len() + self.lines.len()
  }

  /// The OSC 133 mark on an absolute row, if any.
  pub(super) fn abs_prompt(&self, row: usize) -> Option<PromptKind> {
    if row < self.scrollback.len() {
      self.scrollback[row].prompt
    } else {
      self
        .lines
        .get(row - self.scrollback.len())
        .and_then(|l| l.prompt)
    }
  }

  /// Attach an OSC 133 prompt mark to the live line under the cursor.
  pub fn set_prompt_mark(&mut self, kind: PromptKind) {
    let y = self.cursor.y;
    if let Some(line) = self.lines.get_mut(y) {
      line.prompt = Some(kind);
    }
  }

  /// Scroll the viewport to the previous (`up`) or next prompt, placing that
  /// prompt line at the top of the window. No-op on the alternate screen or
  /// when there is no prompt in that direction.
  #[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "prompt positions are bounded by the grid and adjusted in signed \
              coordinates"
  )]
  pub fn jump_prompt(&mut self, up: bool) {
    if self.alt_saved.is_some() {
      return;
    }
    let top = self.scrollback.len().saturating_sub(self.view_offset);
    let total = self.total_lines();
    let is_prompt = |k: Option<PromptKind>| k == Some(PromptKind::PromptStart);
    let target = if up {
      (0..top).rev().find(|&r| is_prompt(self.abs_prompt(r)))
    } else {
      ((top + 1)..total).find(|&r| is_prompt(self.abs_prompt(r)))
    };
    if let Some(t) = target {
      let offset = self.scrollback.len() as isize - t as isize;
      self.view_offset =
        offset.clamp(0, self.scrollback.len() as isize) as usize;
    }
  }

  /// Text of the most recent command's output: the rows from the last
  /// output-start (OSC 133 C) up to the command-end (D) or next prompt.
  pub fn last_command_output(&self) -> Option<String> {
    let total = self.total_lines();
    let start = (0..total)
      .rev()
      .find(|&r| self.abs_prompt(r) == Some(PromptKind::OutputStart))?;
    let mut lines: Vec<String> = Vec::new();
    for r in start..total {
      if r > start
        && matches!(
          self.abs_prompt(r),
          Some(PromptKind::CmdEnd | PromptKind::PromptStart)
        )
      {
        break;
      }
      lines.push(self.row_slice_text(r, 0, usize::MAX).trim_end().to_string());
    }
    // Drop trailing blank rows (e.g. the empty live screen below the output).
    while lines.last().is_some_and(String::is_empty) {
      lines.pop();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Some(out)
  }

  pub const fn set_bracketed_paste(&mut self, on: bool) {
    self.bracketed_paste = on;
  }

  pub const fn bracketed_paste(&self) -> bool {
    self.bracketed_paste
  }

  pub const fn set_sync(&mut self, on: bool) {
    self.sync = on;
  }

  pub const fn sync_active(&self) -> bool {
    self.sync
  }

  pub const fn set_mouse_protocol(&mut self, protocol: MouseProtocol) {
    self.mouse_protocol = protocol;
  }

  pub const fn mouse_protocol(&self) -> MouseProtocol {
    self.mouse_protocol
  }

  pub const fn set_mouse_encoding(&mut self, encoding: MouseEncoding) {
    self.mouse_encoding = encoding;
  }

  pub const fn mouse_encoding(&self) -> MouseEncoding {
    self.mouse_encoding
  }

  pub const fn set_focus_events(&mut self, on: bool) {
    self.focus_events = on;
  }

  pub const fn focus_events(&self) -> bool {
    self.focus_events
  }

  /// The visible text of one row, trailing blanks trimmed.
  #[cfg(test)]
  pub fn row_text(&self, y: usize) -> String {
    self.lines[y]
      .cells
      .iter()
      .filter(|cell| {
        !cell.flags.contains(Flags::WIDE_CONT)
          && !cell.flags.contains(Flags::SIZED_CONT)
      })
      .map(|cell| cell.c)
      .collect::<String>()
      .trim_end()
      .to_string()
  }

  /// The visible viewport as text, one line per row with trailing blank rows
  /// and blank cells trimmed. Used to pipe the on-screen contents.
  pub fn visible_text(&self) -> String {
    let start = self.view_to_abs(0);
    self.logical_text(start, start + self.rows)
  }

  /// The full scrollback plus the live screen as text. Used to pipe or dump
  /// the whole history.
  pub fn scrollback_text(&self) -> String {
    self.logical_text(0, self.scrollback.len() + self.rows)
  }

  fn logical_text(&self, start: usize, end: usize) -> String {
    let mut out = String::new();
    for row in start..end {
      let text = self.row_slice_text(row, 0, self.abs_row(row).len());
      if self.line_at_abs(row).wrapped && row + 1 < end {
        out.push_str(&text);
      } else {
        out.push_str(text.trim_end());
        out.push('\n');
      }
    }
    while out.ends_with('\n') {
      out.pop();
    }
    out
  }
}
