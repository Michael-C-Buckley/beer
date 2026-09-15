//! Screen resizing and soft-wrap reflow.

use std::{collections::VecDeque, mem};

use super::{Cell, Grid, Line, PromptKind, default_tabs};

impl Grid {
  /// Resize the screen. On the main screen this reflows: soft-wrapped runs are
  /// rejoined into logical lines and rewrapped to the new width, across both
  /// scrollback and the live screen, keeping the cursor on its content. The
  /// alternate screen just clips, since its apps repaint on resize.
  pub fn resize(&mut self, cols: usize, rows: usize) {
    let cols = cols.max(1);
    let rows = rows.max(1);
    self.clear_sized_runs();
    if self.alt_saved.is_some() {
      self.clip_resize(cols, rows);
    } else {
      self.reflow_resize(cols, rows);
    }
    self.cols = cols;
    self.rows = rows;
    self.top = 0;
    self.bottom = rows - 1;
    self.left = 0;
    self.right = cols - 1;
    self.tabs = default_tabs(cols);
    self.cursor.x = self.cursor.x.min(cols - 1);
    self.cursor.y = self.cursor.y.min(rows - 1);
    self.wrap_pending = false;
    self.view_offset = 0;
  }

  /// Clip-resize the live screen and the saved primary (alternate screen).
  fn clip_resize(&mut self, cols: usize, rows: usize) {
    for line in &mut self.lines {
      line.cells.resize(cols, Cell::default());
    }
    self.lines.resize(rows, Line::blank(cols));
    if let Some(saved) = self.alt_saved.as_mut() {
      for line in saved.iter_mut() {
        line.cells.resize(cols, Cell::default());
      }
      saved.resize(rows, Line::blank(cols));
    }
    self.clear_selection();
    self.clear_search();
  }

  /// Reflow scrollback + live content to a new width, rewrapping soft-wrapped
  /// paragraphs and repositioning the cursor onto its character.
  fn reflow_resize(&mut self, cols: usize, rows: usize) {
    let cursor_abs = self.scrollback.len() + self.cursor.y;
    let total = self.scrollback.len() + self.lines.len();

    // 1. Rejoin soft-wrapped rows into logical lines. Track which logical line
    //    the cursor falls in and its offset within that line.
    let mut logicals: Vec<Vec<Cell>> = Vec::new();
    let mut logical_marks: Vec<Option<PromptKind>> = Vec::new();
    let mut acc: Vec<Cell> = Vec::new();
    let mut acc_mark: Option<PromptKind> = None;
    let mut cur_logical = 0usize;
    let mut cur_off = 0usize;
    for abs in 0..total {
      if abs == cursor_abs {
        cur_logical = logicals.len();
        cur_off = acc.len() + self.cursor.x;
      }
      let line = if abs < self.scrollback.len() {
        &self.scrollback[abs]
      } else {
        &self.lines[abs - self.scrollback.len()]
      };
      // The mark on the first physical row of a logical line carries over.
      if acc.is_empty() {
        acc_mark = line.prompt;
      }
      acc.extend_from_slice(&line.cells);
      if !line.wrapped {
        logicals.push(mem::take(&mut acc));
        logical_marks.push(acc_mark.take());
      }
    }
    if !acc.is_empty() {
      logicals.push(acc);
      logical_marks.push(acc_mark);
    }
    // Drop trailing all-blank lines (empty screen below the content), but
    // never above the cursor's line, so the cursor keeps its row.
    let last_content = logicals
      .iter()
      .rposition(|l| l.iter().any(|c| *c != Cell::default()))
      .unwrap_or(0);
    logicals.truncate(last_content.max(cur_logical) + 1);
    logical_marks.truncate(last_content.max(cur_logical) + 1);

    // 2. Rewrap each logical line to the new width, recording where the cursor
    //    lands. Trailing blanks are dropped so a hard line does not rewrap its
    //    padding onto extra rows.
    let mut new_lines: Vec<Line> = Vec::new();
    let mut new_cursor_abs = 0usize;
    let mut new_cursor_col = 0usize;
    for (li, mut logical) in logicals.into_iter().enumerate() {
      let trim = logical
        .iter()
        .rposition(|c| *c != Cell::default())
        .map_or(0, |p| p + 1);
      logical.truncate(trim);
      let first = new_lines.len();
      let chunks = logical.len().div_ceil(cols).max(1);
      for ci in 0..chunks {
        let start = ci * cols;
        let end = (start + cols).min(logical.len());
        let mut cells = logical.get(start..end).unwrap_or(&[]).to_vec();
        cells.resize(cols, Cell::default());
        new_lines.push(Line {
          cells,
          wrapped: ci + 1 < chunks,
          // The mark belongs to the first physical row of the line.
          prompt: if ci == 0 { logical_marks[li] } else { None },
        });
      }
      if li == cur_logical {
        let off = cur_off.min(logical.len());
        let chunk = (off / cols).min(chunks - 1);
        new_cursor_abs = first + chunk;
        new_cursor_col = (off - chunk * cols).min(cols - 1);
      }
    }

    // 3. The last `rows` lines are the live screen; the rest is scrollback.
    let live_start = new_lines.len().saturating_sub(rows);
    let mut scrollback: VecDeque<Line> =
      new_lines.drain(0..live_start).collect();
    let mut live = new_lines;
    while live.len() < rows {
      live.push(Line::blank(cols));
    }
    while scrollback.len() > self.scrollback_cap {
      scrollback.pop_front();
    }

    self.cursor.y = new_cursor_abs.saturating_sub(live_start).min(rows - 1);
    self.cursor.x = new_cursor_col;
    self.lines = live;
    self.scrollback = scrollback;
    self.clear_selection();
    self.clear_search();
  }
}
