use super::*;

/// What determines one rendered row's pixels: its cells, the cursor on it, the
/// selection span over it, and the blink phase. Two equal `RowSnap`s render
/// identically, so a buffer holding an equal snapshot needs no repaint.
#[derive(Clone, PartialEq, Debug)]
struct RowSnap {
  cells:   Vec<Cell>,
  /// `(col, shape, focused)` when the cursor is drawn on this row.
  cursor:  Option<(usize, CursorShape, bool)>,
  /// Inclusive selected column span on this row.
  sel:     Option<(usize, usize)>,
  /// Search-match spans `(lo, hi, is_current)` highlighted on this row.
  search:  Vec<(usize, usize, bool)>,
  /// Search-prompt text drawn over this row (only the bottom row, when
  /// active).
  overlay: Option<String>,
  /// IME preedit `(start_col, text)` drawn inline over this row (cursor row).
  preedit: Option<(usize, String)>,
  /// Blink phase, but only varied when the row actually has blinking ink, so
  /// non-blinking rows stay equal across phase toggles.
  blink:   bool,
}

/// One shm buffer plus the per-row snapshot of what it currently displays.
#[derive(Debug)]
pub(super) struct FrameBuf {
  buffer: Buffer,
  rows:   Vec<RowSnap>,
}

/// Snapshot the determinants of viewport row `y`'s pixels.
fn row_snap(grid: &Grid, y: usize, focused: bool, blink_on: bool) -> RowSnap {
  let abs = grid.view_to_abs(y);
  let cells = grid.view_row(y).to_vec();
  let cursor = if grid.view_at_bottom() && grid.cursor().1 == y {
    let visible = grid.cursor_visible() && (!grid.cursor_blink() || blink_on);
    visible.then(|| (grid.cursor().0, grid.cursor_shape(), focused))
  } else {
    None
  };
  let has_blink = cells
    .iter()
    .any(|c| c.flags.contains(crate::grid::Flags::BLINK));
  RowSnap {
    cells,
    cursor,
    sel: grid.selection_span_on(abs),
    search: grid.search_spans_on(abs),
    overlay: None,
    preedit: None,
    blink: if has_blink { blink_on } else { true },
  }
}
impl App {
  /// Render only the rows of window `idx` that changed since the chosen buffer
  /// last displayed them, damage just those rows, and commit with a
  /// frame-callback request.
  pub(super) fn present(&mut self, idx: usize) {
    self.windows[idx].needs_draw = false;
    // URL hint labels overlay the grid but are not part of the row snapshot,
    // so force a full redraw while the labels are showing.
    if self.windows[idx].url_mode {
      self.windows[idx].frames.clear();
    }
    // Render into a buffer sized in physical pixels (logical × scale); the
    // viewport presents it back at the logical surface size.
    let (w, h) = self.phys_dims(idx);
    let m = self.renderer.metrics();
    let focused = self.windows[idx].focused;
    let blink_on = self.blink_on;

    // A resize invalidates every buffer's contents and size.
    if self.windows[idx].buf_dims != (w, h) {
      self.windows[idx].frames.clear();
      self.windows[idx].buf_dims = (w, h);
    }

    // A buffer used for the first time has uninitialized margins; paint the
    // whole thing (background + padding) once, then damage it in full below.
    let pad_y = self.to_phys(idx, self.config.main.pad_y) as i32;

    // Disjoint field borrows: the window carries the surface/pool/frames and
    // the renderer stays on `App`, so both can be held at once.
    let win = &mut self.windows[idx];
    let renderer = &mut self.renderer;

    let Some(session) = win.session.as_ref() else {
      return;
    };
    let grid = session.term.grid();
    // The visual bell inverts fg/bg for the duration of the flash.
    let flashed = win.flashing.then(|| session.term.theme().inverted());
    let theme = flashed.as_ref().unwrap_or(session.term.theme());
    let rows = grid.rows();
    let mut cur: Vec<RowSnap> = (0..rows)
      .map(|y| row_snap(grid, y, focused, blink_on))
      .collect();

    // The search prompt occupies the bottom row while search mode is active.
    // Recording it in the snapshot keeps the row's damage/diff correct.
    let bar_text = if let Some(hex) = &win.unicode_input {
      Some(format!("unicode: U+{}", hex.to_uppercase()))
    } else {
      win.searching.then(|| {
        let (n, total) = grid.search_count();
        format!(
          "search: {}  [{n}/{total}]",
          grid.search_query().unwrap_or("")
        )
      })
    };
    if let Some(text) = &bar_text
      && rows > 0
    {
      cur[rows - 1].overlay = Some(text.clone());
    }

    // The IME preedit is drawn inline at the cursor while composing.
    if !win.preedit.is_empty() && grid.view_at_bottom() {
      let (cx, cy) = grid.cursor();
      if cy < rows {
        cur[cy].preedit = Some((cx, win.preedit.clone()));
      }
    }

    // Reuse a buffer the compositor has released, else grow the ring.
    let stride = w as i32 * 4;
    let mut fidx = None;
    for i in 0..win.frames.len() {
      if win.pool.canvas(&win.frames[i].buffer).is_some() {
        fidx = Some(i);
        break;
      }
    }
    let fidx = match fidx {
      Some(i) => i,
      None if win.frames.len() < MAX_BUFFERS => {
        match win.pool.create_buffer(
          w as i32,
          h as i32,
          stride,
          wl_shm::Format::Argb8888,
        ) {
          Ok((buffer, _)) => {
            win.frames.push(FrameBuf {
              buffer,
              rows: Vec::new(),
            });
            win.frames.len() - 1
          },
          Err(err) => {
            tracing::error!("allocate shm buffer: {err}");
            return;
          },
        }
      },
      // All buffers are still held by the compositor; a release event will
      // wake us and `needs_draw` (re-set below) retries then.
      None => {
        win.needs_draw = true;
        return;
      },
    };

    // Rows that differ from what this buffer last showed (all, if fresh).
    let prev = &win.frames[fidx].rows;
    let dirty: Vec<usize> = (0..rows)
      .filter(|&y| prev.get(y) != Some(&cur[y]))
      .collect();
    if dirty.is_empty() {
      return;
    }

    let fresh = win.frames[fidx].rows.is_empty();
    let Some(canvas) = win.pool.canvas(&win.frames[fidx].buffer) else {
      return;
    };
    let dims = (w as usize, h as usize);
    let frame = crate::render::Frame {
      theme,
      focused,
      blink_on,
      hovered_link: win.hovered_link,
      images: session.term.graphics(),
    };
    if fresh {
      renderer.clear(canvas, dims, theme);
    }
    for &y in &dirty {
      renderer.render_row(canvas, dims, grid, &frame, y);
    }
    // Draw the search prompt over the (now repainted) bottom row.
    if let Some(text) = &bar_text
      && dirty.contains(&(rows - 1))
    {
      renderer.render_search_bar(canvas, dims, theme, rows - 1, text);
    }
    // Draw the IME preedit inline over its (repainted) cursor row.
    for &y in &dirty {
      if let Some((col, text)) = &cur[y].preedit {
        renderer.render_preedit(canvas, dims, theme, y, *col, text);
      }
    }
    // Draw URL hint labels on top, narrowing to those matching the input.
    if win.url_mode {
      for (hit, label) in win.url_hits.iter().zip(&win.url_labels) {
        if label.starts_with(&win.url_input) {
          renderer.render_label(canvas, dims, theme, hit.row, hit.col, label);
        }
      }
    }
    win.frames[fidx].rows = cur;

    let surface = win.window.wl_surface();
    if let Err(err) = win.frames[fidx].buffer.attach_to(surface) {
      tracing::error!("attach buffer: {err}");
      return;
    }
    // With a viewport the buffer is presented at the logical destination, so
    // its own scale stays 1; without one, fall back to integer buffer scale.
    if let Some(vp) = &win.viewport {
      surface.set_buffer_scale(1);
      vp.set_destination(win.width.max(1) as i32, win.height.max(1) as i32);
    } else {
      surface.set_buffer_scale((win.scale120 / 120).max(1) as i32);
    }
    if fresh {
      surface.damage_buffer(0, 0, w as i32, h as i32);
    } else {
      for &y in &dirty {
        let top = pad_y + y as i32 * m.height as i32;
        surface.damage_buffer(0, top, w as i32, m.height as i32);
      }
    }
    surface.frame(&self.qh, FrameCallbackData(surface.clone()));
    win.window.commit();
    win.frame_pending = true;
  }
}
