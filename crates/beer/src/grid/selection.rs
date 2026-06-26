use super::*;

impl Grid {
    /// Slide an active selection up by `n` rows after scrollback eviction,
    /// dropping it if either endpoint scrolled off the top.
    pub(super) fn shift_selection(&mut self, n: usize) {
        if let Some((a, b)) = self.selection {
            if a.row < n || b.row < n {
                self.selection = None;
            } else {
                self.selection = Some((
                    Point {
                        row: a.row - n,
                        ..a
                    },
                    Point {
                        row: b.row - n,
                        ..b
                    },
                ));
            }
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Begin a linear selection at an absolute point (drag anchor).
    pub fn start_selection(&mut self, row: usize, col: usize) {
        let p = Point { row, col };
        self.selection = Some((p, p));
        self.selection_block = false;
    }

    /// Begin a rectangular (block) selection at an absolute point.
    pub fn start_block_selection(&mut self, row: usize, col: usize) {
        let p = Point { row, col };
        self.selection = Some((p, p));
        self.selection_block = true;
    }

    /// Move the selection head (drag), keeping the anchor fixed.
    pub fn extend_selection(&mut self, row: usize, col: usize) {
        if let Some((_, head)) = self.selection.as_mut() {
            *head = Point { row, col };
        }
    }

    /// Select the word at an absolute point, breaking on whitespace and the
    /// default delimiter set.
    pub fn select_word(&mut self, row: usize, col: usize) {
        let delims = &self.word_delimiters;
        let line = self.abs_row(row);
        if col >= line.len() || !is_word(line[col].c, delims) {
            self.start_selection(row, col);
            return;
        }
        let mut lo = col;
        while lo > 0 && is_word(line[lo - 1].c, delims) {
            lo -= 1;
        }
        let mut hi = col;
        while hi + 1 < line.len() && is_word(line[hi + 1].c, delims) {
            hi += 1;
        }
        self.selection = Some((Point { row, col: lo }, Point { row, col: hi }));
        self.selection_block = false;
    }

    /// Select the whole line at an absolute row.
    pub fn select_line(&mut self, row: usize) {
        let last = self.abs_row(row).len().saturating_sub(1);
        self.selection = Some((Point { row, col: 0 }, Point { row, col: last }));
        self.selection_block = false;
    }

    /// The rectangle `(top_row, bottom_row, left_col, right_col)` of a block
    /// selection.
    fn block_rect(&self) -> Option<(usize, usize, usize, usize)> {
        let (a, b) = self.selection?;
        Some((
            a.row.min(b.row),
            a.row.max(b.row),
            a.col.min(b.col),
            a.col.max(b.col),
        ))
    }

    /// Normalized selection (start <= end in reading order), if any.
    fn ordered_selection(&self) -> Option<(Point, Point)> {
        self.selection.map(|(a, b)| {
            if (a.row, a.col) <= (b.row, b.col) {
                (a, b)
            } else {
                (b, a)
            }
        })
    }

    /// Whether the cell at an absolute `(row, col)` falls inside the selection.
    pub fn is_selected(&self, row: usize, col: usize) -> bool {
        if self.selection_block {
            let Some((r0, r1, c0, c1)) = self.block_rect() else {
                return false;
            };
            return row >= r0 && row <= r1 && col >= c0 && col <= c1;
        }
        let Some((start, end)) = self.ordered_selection() else {
            return false;
        };
        if row < start.row || row > end.row {
            return false;
        }
        let lo = if row == start.row { start.col } else { 0 };
        let hi = if row == end.row { end.col } else { usize::MAX };
        col >= lo && col <= hi
    }

    /// The inclusive `(lo, hi)` column span selected on absolute row `row`, if
    /// any part of that row is selected.
    pub fn selection_span_on(&self, row: usize) -> Option<(usize, usize)> {
        if self.selection_block {
            let (r0, r1, c0, c1) = self.block_rect()?;
            return (row >= r0 && row <= r1).then_some((c0, c1));
        }
        let (start, end) = self.ordered_selection()?;
        if row < start.row || row > end.row {
            return None;
        }
        let lo = if row == start.row { start.col } else { 0 };
        let hi = if row == end.row {
            end.col
        } else {
            self.abs_row(row).len().saturating_sub(1)
        };
        Some((lo, hi))
    }

    /// The selected text, with trailing blanks trimmed per line and rows joined
    /// by newlines. `None` if there is no selection.
    pub fn selection_text(&self) -> Option<String> {
        if self.selection_block {
            let (r0, r1, c0, c1) = self.block_rect()?;
            let mut out = String::new();
            for row in r0..=r1 {
                out.push_str(self.row_slice_text(row, c0, c1 + 1).trim_end());
                if row != r1 {
                    out.push('\n');
                }
            }
            return Some(out);
        }
        let (start, end) = self.ordered_selection()?;
        let mut out = String::new();
        for row in start.row..=end.row {
            let line = self.abs_row(row);
            let lo = if row == start.row { start.col } else { 0 };
            let hi = if row == end.row {
                (end.col + 1).min(line.len())
            } else {
                line.len()
            };
            out.push_str(self.row_slice_text(row, lo, hi).trim_end());
            if row != end.row {
                out.push('\n');
            }
        }
        Some(out)
    }

    /// The characters of an absolute row in `[from, to)`, skipping wide and
    /// text-sizing continuation cells. A leading text-sizing cell that holds a
    /// packed run (`OSC 66 w>0`) contributes that whole run.
    pub(super) fn row_slice_text(&self, row: usize, from: usize, to: usize) -> String {
        let mut out = String::new();
        for cell in self
            .abs_row(row)
            .get(from..to.min(self.abs_row(row).len()))
            .unwrap_or(&[])
            .iter()
            .filter(|c| !c.flags.contains(Flags::WIDE_CONT) && !c.flags.contains(Flags::SIZED_CONT))
        {
            if let Some(run) = cell.sized.as_ref().and_then(|s| s.run.as_ref()) {
                out.push_str(run);
                continue;
            }
            out.push(cell.c);
            if let Some(marks) = &cell.combining {
                out.push_str(marks);
            }
        }
        out
    }
}
