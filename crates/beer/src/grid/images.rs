//! Image placement and scaled-text block maintenance.

use super::{
  Cell,
  Flags,
  Grid,
  ImageRef,
  Line,
  line_image_origin,
  line_references_image,
  virtual_image_ref,
};
impl Grid {
  /// Stamp a graphics-protocol placement as a `cols` by `rows` cell rectangle
  /// with its top-left at the cursor, then move the cursor unless `keep_cursor`
  /// (the `C=1` policy) is set. Each cell records its `(dx, dy)` in the
  /// placement so the renderer composites the right image slice; the pixels and
  /// geometry live in the graphics engine. Cells beyond the screen are clipped.
  pub fn place_image(
    &mut self,
    image: u32,
    placement: u32,
    cols: usize,
    rows: usize,
    keep_cursor: bool,
  ) {
    let x0 = self.cursor.x;
    // Scroll the screen to make vertical room for the image. Only scroll
    // when the full screen is the active scroll region; a DECSTBM region
    // or the alternate screen keeps the image clipped as before.
    let y0 = if self.top == 0
      && self.bottom == self.rows - 1
      && self.alt_saved.is_none()
    {
      let y0_initial = self.cursor.y;
      let scroll_n = (y0_initial + rows)
        .saturating_sub(self.rows)
        .min(y0_initial);
      if scroll_n > 0 {
        self.scroll_up(scroll_n);
      }
      y0_initial - scroll_n
    } else {
      self.cursor.y
    };
    for dy in 0..rows {
      let cy = y0 + dy;
      if cy >= self.rows {
        break;
      }
      for dx in 0..cols {
        let cx = x0 + dx;
        if cx >= self.cols {
          break;
        }
        let cell = &mut self.lines[cy].cells[cx];
        cell.image = Some(ImageRef {
          image,
          placement,
          dx: u16::try_from(dx).unwrap_or(u16::MAX),
          dy: u16::try_from(dy).unwrap_or(u16::MAX),
        });
      }
    }
    if keep_cursor {
      return;
    }
    // Land the cursor just past the image on its bottom row, the way kitty
    // leaves it, clamped to the screen.
    self.cursor.y = (y0 + rows.saturating_sub(1)).min(self.rows - 1);
    self.cursor.x = (x0 + cols).min(self.cols - 1);
    self.wrap_pending = false;
  }

  pub fn place_image_relative(
    &mut self,
    image: u32,
    placement: u32,
    cols: usize,
    rows: usize,
    parent: (u32, u32, i32, i32),
  ) {
    let origin = self.lines.iter().enumerate().find_map(|(y, line)| {
      line_image_origin(line, (parent.0, parent.1)).map(|x| (x, y))
    });
    let Some((x, y)) = origin else { return };
    let saved = (self.cursor, self.wrap_pending);
    self.cursor.x = x
      .saturating_add_signed(parent.2 as isize)
      .min(self.cols - 1);
    self.cursor.y = y
      .saturating_add_signed(parent.3 as isize)
      .min(self.rows - 1);
    self.wrap_pending = false;
    self.place_image(image, placement, cols, rows, true);
    (self.cursor, self.wrap_pending) = saved;
  }

  /// Remove image placements: every cell whose reference matches `pred` is
  /// cleared back to a blank. With `pred` always true this erases all images.
  pub fn clear_images(
    &mut self,
    pred: impl Fn(&ImageRef) -> bool,
  ) -> Vec<(u32, u32)> {
    let blank = Cell::default();
    let mut removed = Vec::new();
    let mut touch = |line: &mut Line| {
      let mut previous = None;
      for cell in &mut line.cells {
        let reference = cell
          .image
          .or_else(|| virtual_image_ref(cell, &mut previous));
        if reference.is_some_and(|r| pred(&r)) {
          if let Some(reference) = reference {
            let key = (reference.image, reference.placement);
            if !removed.contains(&key) {
              removed.push(key);
            }
          }
          *cell = blank.clone();
        }
      }
    };
    self.lines.iter_mut().for_each(&mut touch);
    self.scrollback.iter_mut().for_each(&mut touch);
    if let Some(alt) = self.alt_saved.as_mut() {
      alt.iter_mut().for_each(&mut touch);
    }
    removed
  }

  pub fn clear_screen_images(
    &mut self,
    pred: impl Fn(usize, usize, &ImageRef) -> bool,
  ) -> Vec<(u32, u32)> {
    let mut targets = Vec::new();
    for (y, line) in self.lines.iter().enumerate() {
      let mut previous = None;
      for (x, cell) in line.cells.iter().enumerate() {
        if let Some(reference) = cell
          .image
          .or_else(|| virtual_image_ref(cell, &mut previous))
          && pred(x, y, &reference)
          && !targets.contains(&(reference.image, reference.placement))
        {
          targets.push((reference.image, reference.placement));
        }
      }
    }
    self.clear_images(|reference| {
      targets.contains(&(reference.image, reference.placement))
    })
  }

  pub fn image_referenced(&self, image: u32) -> bool {
    let has = |line: &Line| line_references_image(line, image);
    self.lines.iter().any(has)
      || self.scrollback.iter().any(has)
      || self
        .alt_saved
        .as_ref()
        .is_some_and(|lines| lines.iter().any(has))
  }

  /// The image placements intersecting the current cursor cell, for `d=c`
  /// deletes: returns each `(image, placement)` found there.
  pub fn images_at_cursor(&self) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    if let Some(r) = self.lines[self.cursor.y].cells[self.cursor.x].image {
      out.push((r.image, r.placement));
    }
    out
  }

  /// If `(x, y)` belongs to a text-sizing block, blank every cell of that
  /// block so a write into it cannot orphan continuation cells.
  pub(super) fn clear_sized_at(&mut self, x: usize, y: usize) {
    let Some(s) = self
      .lines
      .get(y)
      .and_then(|l| l.cells.get(x))
      .and_then(|c| c.sized.as_deref())
    else {
      return;
    };
    let (dx, dy, cols, rows) = (
      s.dx as usize,
      s.dy as usize,
      s.cols as usize,
      s.rows as usize,
    );
    let x0 = x.saturating_sub(dx);
    let y0 = y.saturating_sub(dy);
    let blank = self.pen_blank();
    let x_end = (x0 + cols).min(self.cols);
    for by in y0..(y0 + rows).min(self.rows) {
      for cell in &mut self.lines[by].cells[x0..x_end] {
        // Only blank cells that are themselves part of a sized block, so
        // a stale `(dx, dy)` (e.g. after a scroll split the block) can
        // never wipe unrelated plain text - at worst it leaves a fragment.
        if cell.sized.is_some() {
          *cell = blank.clone();
        }
      }
    }
  }

  /// Dissolve every text-sizing block that intersects cells `[from, to)` of row
  /// `y`, so any write into a block (erase, delete, shift) removes the whole
  /// block rather than leaving the renderer to draw an orphaned fragment.
  pub(super) fn dissolve_sized(&mut self, y: usize, from: usize, to: usize) {
    let to = to.min(self.cols);
    for x in from..to {
      if self
        .lines
        .get(y)
        .and_then(|l| l.cells.get(x))
        .is_some_and(|c| c.sized.is_some())
      {
        // Clearing the block blanks the rest of the range's members too,
        // so subsequent iterations find nothing and skip.
        self.clear_sized_at(x, y);
      }
    }
  }

  /// Drop all text-sizing blocks: their cells become plain characters. Used
  /// before a resize, since a scaled block must not be split across a rewrap;
  /// applications using `OSC 66` repaint on resize regardless.
  pub(super) fn clear_sized_runs(&mut self) {
    let strip = |line: &mut Line| {
      for cell in &mut line.cells {
        if cell.sized.take().is_some() {
          cell.flags.remove(Flags::SIZED_CONT);
        }
      }
    };
    self.lines.iter_mut().for_each(strip);
    self.scrollback.iter_mut().for_each(strip);
    if let Some(alt) = self.alt_saved.as_mut() {
      alt.iter_mut().for_each(strip);
    }
  }
}
