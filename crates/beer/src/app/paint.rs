//! Frame damage tracking and presentation.

use std::mem;

use beer_window::WindowCtx;

use super::{App, SYNC_TIMEOUT_MS, row_matches, row_snap, status_bar_text};

impl App {
  /// Repaint window `idx` if its displayed state changed: diff rows against the
  /// acquired buffer's snapshot, render the dirty ones, present.
  #[expect(
    clippy::cast_possible_truncation,
    reason = "row indices are bounded by the grid height"
  )]
  pub(super) fn paint(&mut self, ctx: &mut dyn WindowCtx, idx: usize) {
    let id = self.windows[idx].id;
    if self.windows[idx].session.is_none() {
      return;
    }
    self.ensure_renderer(idx);
    // Synchronized output (DECSET 2026) withholds frames; arm a timeout so a
    // stuck `2026h` cannot freeze the window.
    let sync = self.windows[idx]
      .session
      .as_ref()
      .is_some_and(|s| s.term.grid().sync_active());
    if sync {
      if self.windows[idx].sync_token.is_none() {
        let tok = self.alloc_token();
        self.windows[idx].sync_token = Some(tok);
        ctx.arm_timer(tok, SYNC_TIMEOUT_MS);
      }
      return;
    }
    if let Some(tok) = self.windows[idx].sync_token.take() {
      ctx.cancel_timer(tok);
    }

    let (w, h) = self.phys_dims(&self.windows[idx]);
    let m = self.windows[idx].metrics;
    let scale = self.windows[idx].scale120;
    let pad_y = self.to_phys(&self.windows[idx], self.config.main.pad_y);
    let focused = self.windows[idx].focused;
    let blink_on = self.blink_on;
    let rapid_on = self.rapid_on;
    // URL labels overlay the grid but are not in the row snapshot, so force a
    // full redraw while labels show by clearing the snapshot cache.
    if self.windows[idx].url_mode {
      self.windows[idx].snaps.clear();
    }

    let Some(frame) = ctx.acquire(id, w, h) else {
      // Every buffer is still held by the compositor; keep the redraw pending
      // so a buffer release re-drives this paint.
      ctx.request_redraw(id);
      return;
    };
    let buf_id = frame.id;
    let pixels = frame.pixels;
    let dims = (w as usize, h as usize);

    let win = &self.windows[idx];
    let Some(session) = win.session.as_ref() else {
      return;
    };
    let grid = session.term.grid();
    let flashed = win.flashing.then(|| session.term.theme().inverted());
    let theme = flashed.as_ref().unwrap_or_else(|| session.term.theme());
    let rows = grid.rows();
    let bar_text = status_bar_text(win, grid, self.config.scrollback.indicator);
    let preedit = if !win.preedit.is_empty() && grid.view_at_bottom() {
      let (cx, cy) = grid.cursor();
      (cy < rows).then_some((cy, cx, win.preedit.as_str()))
    } else {
      None
    };

    let empty = Vec::new();
    let prev = win.snaps.get(&buf_id).unwrap_or(&empty);
    let fresh = frame.fresh || prev.is_empty();
    let dirty: Vec<usize> = (0..rows)
      .filter(|&y| {
        let overlay = (y + 1 == rows).then_some(bar_text.as_deref()).flatten();
        let pe = preedit.filter(|&(r, ..)| r == y).map(|(_, c, t)| (c, t));
        fresh
          || !row_matches(
            prev.get(y),
            grid,
            y,
            focused,
            (blink_on, rapid_on),
            overlay,
            pe,
          )
      })
      .collect();
    if dirty.is_empty() {
      return;
    }

    let rframe = crate::render::Frame {
      theme,
      focused,
      blink_on,
      rapid_on,
      hovered_link: win.hovered_link,
      images: session.term.graphics(),
    };
    // Render through this window's scale renderer (disjoint from `windows`).
    if let Some(renderer) = self.renderers.get_mut(&scale) {
      if fresh {
        renderer.clear(pixels, dims, theme);
      }
      for &y in &dirty {
        renderer.render_row(pixels, dims, grid, &rframe, y);
      }
      if let Some(text) = &bar_text
        && dirty.contains(&(rows - 1))
      {
        renderer.render_search_bar(pixels, dims, theme, rows - 1, text);
      }
      for &y in &dirty {
        if let Some((r, c, t)) = preedit
          && r == y
        {
          renderer.render_preedit(pixels, dims, theme, y, c, t);
        }
      }
      if win.url_mode {
        for (hit, label) in win.url_hits.iter().zip(&win.url_labels) {
          if label.starts_with(&win.url_input) {
            renderer.render_label(pixels, dims, theme, hit.row, hit.col, label);
          }
        }
      }
    }

    // Own the preedit text so the snapshot update can take a mutable borrow of
    // the window (the `&str` above borrows it).
    let preedit_owned = preedit.map(|(r, c, t)| (r, c, t.to_owned()));
    // Update this buffer's snapshot for the next diff.
    let mut snaps =
      mem::take(self.windows[idx].snaps.entry(buf_id).or_default());
    let win = &self.windows[idx];
    let Some(session) = win.session.as_ref() else {
      return;
    };
    let grid = session.term.grid();
    for &y in &dirty {
      let overlay = (y + 1 == rows).then_some(bar_text.as_deref()).flatten();
      let pe = preedit_owned
        .as_ref()
        .filter(|(r, ..)| *r == y)
        .map(|(_, c, t)| (*c, t.as_str()));
      let s = row_snap(grid, y, focused, blink_on, rapid_on, overlay, pe);
      if y < snaps.len() {
        snaps[y] = s;
      } else {
        snaps.push(s);
      }
    }
    snaps.truncate(rows);
    self.windows[idx].snaps.insert(buf_id, snaps);

    let dirty_u32: Vec<u32> = dirty.iter().map(|&y| y as u32).collect();
    ctx.present(id, buf_id, &dirty_u32, m.height, pad_y, fresh);
  }
}
