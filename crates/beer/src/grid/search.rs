use super::Grid;

/// One scrollback-search hit: a run of `len` cells at absolute `(row, col)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Match {
  row: usize,
  col: usize,
  len: usize,
}

/// Incremental scrollback search: the query, every hit, and the focused one.
#[derive(Clone, Debug, Default)]
pub(super) struct SearchState {
  query:   String,
  matches: Vec<Match>,
  current: usize,
}

impl Grid {
  /// Set the search query and recompute matches over scrollback + the live
  /// screen, focusing the most recent hit and scrolling it into view. An
  /// empty query keeps search mode active but clears the hit list.
  pub fn set_search(&mut self, query: &str) {
    let matches = if query.is_empty() {
      Vec::new()
    } else {
      self.compute_matches(query)
    };
    let current = matches.len().saturating_sub(1);
    self.search = Some(SearchState {
      query: query.to_string(),
      matches,
      current,
    });
    self.jump_to_current();
  }

  /// Move the focused match `forward` (toward newer) or back (toward older),
  /// wrapping, and scroll it into view.
  pub fn search_step(&mut self, forward: bool) {
    if let Some(s) = self.search.as_mut()
      && !s.matches.is_empty()
    {
      let n = s.matches.len();
      s.current = if forward {
        (s.current + 1) % n
      } else {
        (s.current + n - 1) % n
      };
    }
    self.jump_to_current();
  }

  pub fn clear_search(&mut self) {
    self.search = None;
  }

  /// The current query, while search mode is active.
  pub fn search_query(&self) -> Option<&str> {
    self.search.as_ref().map(|s| s.query.as_str())
  }

  /// `(focused match index 1-based, total matches)` for the search prompt.
  pub fn search_count(&self) -> (usize, usize) {
    self.search.as_ref().map_or((0, 0), |s| {
      let total = s.matches.len();
      (if total == 0 { 0 } else { s.current + 1 }, total)
    })
  }

  /// Match spans `(lo, hi, is_current)` on absolute row `row`, for highlight.
  pub fn search_spans_on(&self, row: usize) -> Vec<(usize, usize, bool)> {
    let Some(s) = self.search.as_ref() else {
      return Vec::new();
    };
    s.matches
      .iter()
      .enumerate()
      .filter(|(_, m)| m.row == row && m.len > 0)
      .map(|(i, m)| (m.col, m.col + m.len - 1, i == s.current))
      .collect()
  }

  fn compute_matches(&self, query: &str) -> Vec<Match> {
    // Smart case: an uppercase letter in the query forces case sensitivity.
    let sensitive = query.chars().any(|c| c.is_ascii_uppercase());
    let fold = |c: char| {
      if sensitive { c } else { c.to_ascii_lowercase() }
    };
    let needle: Vec<char> = query.chars().map(fold).collect();
    if needle.is_empty() {
      return Vec::new();
    }
    let total = self.scrollback.len() + self.rows;
    let mut matches = Vec::new();
    for row in 0..total {
      let hay: Vec<char> =
        self.abs_row(row).iter().map(|cell| fold(cell.c)).collect();
      let mut i = 0;
      while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()] == needle[..] {
          matches.push(Match {
            row,
            col: i,
            len: needle.len(),
          });
          i += needle.len();
        } else {
          i += 1;
        }
      }
    }
    matches
  }

  /// Scroll the viewport so the focused match is on screen, centering it only
  /// when it would otherwise be off the visible range.
  #[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "scroll coordinates are bounded by the allocated grid and are \
              converted for signed clamping"
  )]
  fn jump_to_current(&mut self) {
    let Some(abs) = self
      .search
      .as_ref()
      .and_then(|s| s.matches.get(s.current))
      .map(|m| m.row)
    else {
      return;
    };
    let sb = self.scrollback.len();
    let top = sb - self.view_offset;
    let bottom = top + self.rows - 1;
    if abs < top || abs > bottom {
      let target = sb as isize + self.rows as isize / 2 - abs as isize;
      self.view_offset = target.clamp(0, sb as isize) as usize;
    }
  }

  /// Slide search hits up by `n` rows after scrollback eviction, dropping any
  /// that scrolled off the top.
  pub(super) fn shift_search(&mut self, n: usize) {
    if let Some(s) = self.search.as_mut() {
      s.matches.retain(|m| m.row >= n);
      for m in &mut s.matches {
        m.row -= n;
      }
      s.current = s.current.min(s.matches.len().saturating_sub(1));
    }
  }
}
