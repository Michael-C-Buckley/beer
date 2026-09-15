//! Cursor editing, scrolling, and rectangular operations.

use std::mem;

use super::{Cell, Flags, Grid, Line, Rect, Underline};

impl Grid {
  pub(super) fn shift_right(&mut self, n: usize) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    let end = self.right_edge();
    // Shifting cells sideways would scramble a scaled block's back-references.
    self.dissolve_sized(y, x, end);
    let blank = self.pen_blank();
    let row = &mut self.lines[y].cells;
    for i in (x + n..end).rev() {
      row[i] = row[i - n].clone();
    }
    for cell in &mut row[x..(x + n).min(end)] {
      *cell = blank.clone();
    }
  }

  pub(super) fn pen_blank(&self) -> Cell {
    // A space carrying only the current background (back-colour erase).
    Cell {
      bg: self.pen.bg,
      ..Cell::default()
    }
  }

  pub(super) const fn region(&self) -> (usize, usize) {
    if self.origin {
      (self.top, self.bottom)
    } else {
      (0, self.rows - 1)
    }
  }

  /// The column region the cursor addresses within: the left/right margins in
  /// origin mode when they are enabled, else the full width.
  pub(super) const fn hregion(&self) -> (usize, usize) {
    if self.origin && self.lr_margins {
      (self.left, self.right)
    } else {
      (0, self.cols - 1)
    }
  }

  /// The column a wrap returns to: the left margin when the cursor is inside an
  /// enabled left/right region, else column 0.
  pub(super) const fn left_edge(&self) -> usize {
    if self.lr_margins
      && self.cursor.x >= self.left
      && self.cursor.x <= self.right
    {
      self.left
    } else {
      0
    }
  }

  /// The exclusive column bound for wrapping and in-line edits: one past the
  /// right margin when the cursor is inside an enabled region, else the width.
  pub(super) const fn right_edge(&self) -> usize {
    if self.lr_margins
      && self.cursor.x >= self.left
      && self.cursor.x <= self.right
    {
      self.right + 1
    } else {
      self.cols
    }
  }

  /// Copy the `[left..=right]` span of row `src` into row `dst`, for a scroll
  /// confined by the left/right margins.
  pub(super) fn copy_span(&mut self, dst: usize, src: usize) {
    let (l, r) = (self.left, self.right);
    self.dissolve_sized(dst, l, r + 1);
    self.dissolve_sized(src, l, r + 1);
    let span: Vec<Cell> = self.lines[src].cells[l..=r].to_vec();
    self.lines[dst].cells[l..=r].clone_from_slice(&span);
  }

  /// Blank the `[left..=right]` span of row `y` with the pen background.
  pub(super) fn blank_span(&mut self, y: usize) {
    let (l, r) = (self.left, self.right);
    self.dissolve_sized(y, l, r + 1);
    let blank = self.pen_blank();
    for cell in &mut self.lines[y].cells[l..=r] {
      *cell = blank.clone();
    }
  }

  pub fn move_to(&mut self, x: usize, y: usize) {
    let (rt, rb) = self.region();
    let (cl, cr) = self.hregion();
    self.cursor.x = (x + cl).min(cr).max(cl);
    self.cursor.y = (y + rt).min(rb).max(rt);
    self.wrap_pending = false;
  }

  pub fn move_to_col(&mut self, x: usize) {
    self.cursor.x = x.min(self.cols - 1);
    self.wrap_pending = false;
  }

  pub fn move_to_row(&mut self, y: usize) {
    let (rt, rb) = self.region();
    self.cursor.y = (y + rt).min(rb).max(rt);
    self.wrap_pending = false;
  }

  pub fn cursor_up(&mut self, n: usize) {
    let (rt, _) = self.region();
    self.cursor.y = self.cursor.y.saturating_sub(n).max(rt);
    self.wrap_pending = false;
  }

  pub fn cursor_down(&mut self, n: usize) {
    let (_, rb) = self.region();
    self.cursor.y = (self.cursor.y + n).min(rb);
    self.wrap_pending = false;
  }

  pub fn cursor_fwd(&mut self, n: usize) {
    self.cursor.x = (self.cursor.x + n).min(self.cols - 1);
    self.wrap_pending = false;
  }

  pub const fn cursor_back(&mut self, n: usize) {
    self.cursor.x = self.cursor.x.saturating_sub(n);
    self.wrap_pending = false;
  }

  pub const fn save_cursor(&mut self) {
    self.saved = self.cursor;
  }

  pub fn restore_cursor(&mut self) {
    self.cursor = self.saved;
    self.cursor.x = self.cursor.x.min(self.cols - 1);
    self.cursor.y = self.cursor.y.min(self.rows - 1);
    self.wrap_pending = false;
  }

  pub const fn carriage_return(&mut self) {
    self.cursor.x = 0;
    self.wrap_pending = false;
  }

  pub const fn backspace(&mut self) {
    self.cursor.x = self.cursor.x.saturating_sub(1);
    self.wrap_pending = false;
  }

  /// LF/VT/FF: move down one row, scrolling at the region bottom.
  pub fn line_feed(&mut self) {
    if self.cursor.y == self.bottom {
      self.scroll_up(1);
    } else if self.cursor.y < self.rows - 1 {
      self.cursor.y += 1;
    }
    self.wrap_pending = false;
  }

  /// RI: move up one row, scrolling down at the region top.
  pub fn reverse_index(&mut self) {
    if self.cursor.y == self.top {
      self.scroll_down(1);
    } else if self.cursor.y > 0 {
      self.cursor.y -= 1;
    }
    self.wrap_pending = false;
  }

  /// NEL: carriage return plus line feed.
  pub fn next_line(&mut self) {
    self.carriage_return();
    self.line_feed();
  }

  pub fn tab(&mut self) {
    let mut x = self.cursor.x + 1;
    while x < self.cols && !self.tabs[x] {
      x += 1;
    }
    self.cursor.x = x.min(self.cols - 1);
    self.wrap_pending = false;
  }

  pub fn set_tab(&mut self) {
    if self.cursor.x < self.cols {
      self.tabs[self.cursor.x] = true;
    }
  }

  pub fn clear_tab(&mut self) {
    if self.cursor.x < self.cols {
      self.tabs[self.cursor.x] = false;
    }
  }

  pub fn clear_all_tabs(&mut self) {
    self.tabs.iter_mut().for_each(|t| *t = false);
  }

  pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
    if top < bottom && bottom < self.rows {
      self.top = top;
      self.bottom = bottom;
    } else {
      self.top = 0;
      self.bottom = self.rows - 1;
    }
    self.move_to(0, 0);
  }

  /// Whether left/right margins are enabled (DECLRMM, DECSET `?69`).
  pub const fn lr_margins_enabled(&self) -> bool {
    self.lr_margins
  }

  /// DECLRMM (DECSET `?69`): enable or disable left/right margins. Disabling
  /// resets the margins to the full width.
  pub const fn set_lr_margins_mode(&mut self, on: bool) {
    self.lr_margins = on;
    if !on {
      self.left = 0;
      self.right = self.cols - 1;
    }
  }

  /// DECSLRM (`CSI Pl ; Pr s`): set the left/right margins when DECLRMM is on,
  /// then home the cursor. Out-of-order or out-of-range values reset to full.
  pub fn set_lr_margins(&mut self, left: usize, right: usize) {
    if !self.lr_margins {
      return;
    }
    if left < right && right < self.cols {
      self.left = left;
      self.right = right;
    } else {
      self.left = 0;
      self.right = self.cols - 1;
    }
    self.move_to(0, 0);
  }

  pub fn scroll_up(&mut self, n: usize) {
    let n = n.min(self.bottom - self.top + 1);
    let full_width = self.left == 0 && self.right == self.cols - 1;
    // Lines leaving the top of the *whole* main screen become scrollback; a
    // DECSTBM region scroll (top > 0), a margin-confined scroll, or the alt
    // screen does not.
    if self.top == 0 && full_width && self.alt_saved.is_none() {
      for y in 0..n {
        let line = mem::replace(&mut self.lines[y], Line::blank(self.cols));
        self.scrollback.push_back(line);
      }
      let mut evicted = 0;
      while self.scrollback.len() > self.scrollback_cap {
        self.scrollback.pop_front();
        evicted += 1;
      }
      if evicted > 0 {
        self.shift_selection(evicted);
        self.shift_search(evicted);
      }
      // Keep a scrolled-back viewport anchored to the same content.
      if self.view_offset > 0 {
        self.view_offset = (self.view_offset + n).min(self.scrollback.len());
      }
    }
    if full_width {
      for y in self.top..=self.bottom {
        if y + n <= self.bottom {
          self.lines.swap(y, y + n);
        }
      }
      for y in (self.bottom + 1 - n)..=self.bottom {
        self.blank_row(y);
      }
    } else {
      for y in self.top..=self.bottom {
        if y + n <= self.bottom {
          self.copy_span(y, y + n);
        } else {
          self.blank_span(y);
        }
      }
    }
  }

  pub fn scroll_down(&mut self, n: usize) {
    let n = n.min(self.bottom - self.top + 1);
    let full_width = self.left == 0 && self.right == self.cols - 1;
    for y in (self.top..=self.bottom).rev() {
      if y >= self.top + n {
        if full_width {
          self.lines.swap(y, y - n);
        } else {
          self.copy_span(y, y - n);
        }
      } else if full_width {
        self.blank_row(y);
      } else {
        self.blank_span(y);
      }
    }
  }

  pub(super) fn blank_row(&mut self, y: usize) {
    let cols = self.cols;
    self.dissolve_sized(y, 0, cols);
    let blank = self.pen_blank();
    let line = &mut self.lines[y];
    for cell in &mut line.cells {
      *cell = blank.clone();
    }
    line.wrapped = false;
  }

  /// DECALN (`ESC # 8`): fill the screen with `E` in default attributes for
  /// alignment testing, reset the scroll region, and home the cursor.
  pub fn decaln(&mut self) {
    self.top = 0;
    self.bottom = self.rows - 1;
    for y in 0..self.rows {
      self.dissolve_sized(y, 0, self.cols);
      let line = &mut self.lines[y];
      for cell in &mut line.cells {
        *cell = Cell {
          c: 'E',
          ..Cell::default()
        };
      }
      line.wrapped = false;
    }
    self.cursor.x = 0;
    self.cursor.y = 0;
    self.wrap_pending = false;
  }

  /// ED: 0=below, 1=above, 2/3=all.
  pub fn erase_display(&mut self, mode: u16) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    match mode {
      0 => {
        self.erase_in_row(y, x, self.cols);
        for r in (y + 1)..self.rows {
          self.blank_row(r);
        }
      },
      1 => {
        for r in 0..y {
          self.blank_row(r);
        }
        self.erase_in_row(y, 0, x + 1);
      },
      _ => {
        for r in 0..self.rows {
          self.blank_row(r);
        }
      },
    }
    self.wrap_pending = false;
  }

  /// EL: 0=right, 1=left, 2=line.
  pub fn erase_line(&mut self, mode: u16) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    match mode {
      0 => self.erase_in_row(y, x, self.cols),
      1 => self.erase_in_row(y, 0, x + 1),
      _ => self.erase_in_row(y, 0, self.cols),
    }
    self.wrap_pending = false;
  }

  /// ECH: erase n characters from the cursor without moving it.
  pub fn erase_chars(&mut self, n: usize) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    self.erase_in_row(y, x, (x + n).min(self.cols));
  }

  pub(super) fn erase_in_row(&mut self, y: usize, from: usize, to: usize) {
    let to = to.min(self.cols);
    self.dissolve_sized(y, from, to);
    let blank = self.pen_blank();
    for cell in &mut self.lines[y].cells[from..to] {
      *cell = blank.clone();
    }
  }

  /// ICH: insert n blanks at the cursor, shifting the rest right.
  pub fn insert_chars(&mut self, n: usize) {
    let saved = self.insert;
    self.insert = true;
    self.shift_right(n.min(self.cols));
    self.insert = saved;
  }

  /// DCH: delete n characters at the cursor, shifting the rest left.
  pub fn delete_chars(&mut self, n: usize) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    let end = self.right_edge();
    let n = n.min(end - x);
    // Shifting cells sideways would scramble a scaled block's back-references.
    self.dissolve_sized(y, x, end);
    let blank = self.pen_blank();
    let row = &mut self.lines[y].cells;
    for i in x..end {
      row[i] = if i + n < end {
        row[i + n].clone()
      } else {
        blank.clone()
      };
    }
  }

  /// IL: insert n blank lines at the cursor row, within the scroll region and
  /// (when set) the left/right margins.
  pub fn insert_lines(&mut self, n: usize) {
    if self.cursor.y < self.top || self.cursor.y > self.bottom {
      return;
    }
    if self.lr_margins
      && (self.cursor.x < self.left || self.cursor.x > self.right)
    {
      return;
    }
    let n = n.min(self.bottom - self.cursor.y + 1);
    if self.left == 0 && self.right == self.cols - 1 {
      for y in (self.cursor.y..=self.bottom).rev() {
        if y >= self.cursor.y + n {
          self.lines.swap(y, y - n);
        }
      }
      for y in self.cursor.y..(self.cursor.y + n) {
        self.blank_row(y);
      }
    } else {
      for y in (self.cursor.y..=self.bottom).rev() {
        if y >= self.cursor.y + n {
          self.copy_span(y, y - n);
        } else {
          self.blank_span(y);
        }
      }
    }
  }

  /// DL: delete n lines at the cursor row, within the scroll region and (when
  /// set) the left/right margins.
  pub fn delete_lines(&mut self, n: usize) {
    if self.cursor.y < self.top || self.cursor.y > self.bottom {
      return;
    }
    if self.lr_margins
      && (self.cursor.x < self.left || self.cursor.x > self.right)
    {
      return;
    }
    let n = n.min(self.bottom - self.cursor.y + 1);
    if self.left == 0 && self.right == self.cols - 1 {
      for y in self.cursor.y..=self.bottom {
        if y + n <= self.bottom {
          self.lines.swap(y, y + n);
        }
      }
      for y in (self.bottom + 1 - n)..=self.bottom {
        self.blank_row(y);
      }
    } else {
      for y in self.cursor.y..=self.bottom {
        if y + n <= self.bottom {
          self.copy_span(y, y + n);
        } else {
          self.blank_span(y);
        }
      }
    }
  }

  /// Resolve `rect` (origin-relative, 0-based, inclusive) to absolute grid
  /// coordinates clamped to the addressable region.
  pub(super) fn clamp_rect(&self, rect: Rect) -> Rect {
    let (rt, rb) = self.region();
    let (cl, cr) = self.hregion();
    let top = (rect.top + rt).min(rb);
    let left = (rect.left + cl).min(cr);
    Rect {
      top,
      left,
      bottom: (rect.bottom + rt).min(rb).max(top),
      right: (rect.right + cl).min(cr).max(left),
    }
  }

  /// DECFRA: fill an inclusive rectangle with `c` in the current pen.
  pub fn fill_rect(&mut self, c: char, rect: Rect) {
    let a = self.clamp_rect(rect);
    let mut cell = self.pen.clone();
    cell.c = c;
    cell.combining = None;
    cell.sized = None;
    cell.flags.remove(Flags::WIDE_CONT);
    for y in a.top..=a.bottom {
      self.dissolve_sized(y, a.left, a.right + 1);
      for cell_ref in &mut self.lines[y].cells[a.left..=a.right] {
        *cell_ref = cell.clone();
      }
    }
  }

  /// DECERA: erase an inclusive rectangle to the pen background.
  pub fn erase_rect(&mut self, rect: Rect) {
    let a = self.clamp_rect(rect);
    for y in a.top..=a.bottom {
      self.erase_in_row(y, a.left, a.right + 1);
    }
  }

  /// DECCARA: set/clear flag attributes (and optionally the underline style)
  /// over an inclusive rectangle, leaving the characters in place.
  pub fn change_attrs_rect(
    &mut self,
    rect: Rect,
    set: Flags,
    clear: Flags,
    underline: Option<Underline>,
  ) {
    let a = self.clamp_rect(rect);
    for y in a.top..=a.bottom {
      for cell in &mut self.lines[y].cells[a.left..=a.right] {
        cell.flags.insert(set);
        cell.flags.remove(clear);
        if let Some(u) = underline {
          cell.underline = u;
        }
      }
    }
  }

  /// DECCRA: copy an inclusive source rectangle to a destination top-left,
  /// snapshotting first so overlapping copies are well defined.
  pub fn copy_rect(&mut self, src: Rect, dst_top: usize, dst_left: usize) {
    let s = self.clamp_rect(src);
    let (rt, _) = self.region();
    let (cl, _) = self.hregion();
    let dt = (dst_top + rt).min(self.rows - 1);
    let dl = (dst_left + cl).min(self.cols - 1);
    let mut buf: Vec<Vec<Cell>> = Vec::with_capacity(s.bottom - s.top + 1);
    for y in s.top..=s.bottom {
      buf.push(self.lines[y].cells[s.left..=s.right].to_vec());
    }
    for (dy, row) in buf.into_iter().enumerate() {
      let ty = dt + dy;
      if ty >= self.rows {
        break;
      }
      self.dissolve_sized(ty, dl, (dl + row.len()).min(self.cols));
      for (dx, cell) in row.into_iter().enumerate() {
        let tx = dl + dx;
        if tx < self.cols {
          self.lines[ty].cells[tx] = cell;
        }
      }
    }
  }
}
