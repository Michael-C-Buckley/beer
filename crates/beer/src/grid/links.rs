use std::num::NonZeroU16;

use super::{Flags, Grid};

/// A URL detected in the visible viewport, with the `(row, col)` of its first
/// character in viewport coordinates.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UrlHit {
  pub url: String,
  pub row: usize,
  pub col: usize,
}

/// Whether `c` may appear inside a URL (excludes whitespace, controls, and the
/// delimiters that conventionally bound a URL in flowing text).
fn is_url_char(c: char) -> bool {
  !c.is_whitespace()
    && !c.is_control()
    && !matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^')
}

/// Find `scheme://…` URLs in `chars`, returning `(start, end)` index ranges.
/// The scheme is a run of `[A-Za-z][A-Za-z0-9+.-]*` before `://`; the body runs
/// to the first non-URL character, with trailing sentence punctuation trimmed.
fn find_urls(chars: &[char]) -> Vec<(usize, usize)> {
  let mut out = Vec::new();
  let mut i = 0;
  while i + 2 < chars.len() {
    if chars[i] == ':' && chars[i + 1] == '/' && chars[i + 2] == '/' {
      // Backtrack over the scheme.
      let mut start = i;
      while start > 0 {
        let c = chars[start - 1];
        if c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.') {
          start -= 1;
        } else {
          break;
        }
      }
      if start < i && chars[start].is_ascii_alphabetic() {
        let mut end = i + 3;
        while end < chars.len() && is_url_char(chars[end]) {
          end += 1;
        }
        // Trim trailing punctuation that is usually sentence-level.
        while end > i + 3
          && matches!(
            chars[end - 1],
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '\'' | '"'
          )
        {
          end -= 1;
        }
        if end > i + 3 {
          out.push((start, end));
          i = end;
          continue;
        }
      }
    }
    i += 1;
  }
  out
}

impl Grid {
  /// Set (or clear) the active OSC 8 hyperlink applied to printed cells. An
  /// empty/`None` URI ends the current link.
  pub fn set_link(&mut self, uri: Option<&str>) {
    self.pen.link = match uri {
      Some(uri) if !uri.is_empty() => Some(self.intern_link(uri)),
      _ => None,
    };
  }

  /// Intern a hyperlink URI, returning its 1-based id (deduplicated; the table
  /// is capped so a pathological stream cannot grow it without bound).
  fn intern_link(&mut self, uri: &str) -> NonZeroU16 {
    if let Some(i) = self.links.iter().position(|u| u.as_ref() == uri) {
      let id = u16::try_from(i + 1).unwrap_or(u16::MAX);
      return NonZeroU16::new(id).unwrap_or(NonZeroU16::MIN);
    }
    // u16::MAX distinct links is far past any real document; reuse the last
    // slot once saturated rather than overflow the id space.
    if self.links.len() < usize::from(u16::MAX) - 1 {
      self.links.push(uri.into());
    } else if let Some(last) = self.links.last_mut() {
      *last = uri.into();
    }
    let id = u16::try_from(self.links.len()).unwrap_or(u16::MAX);
    NonZeroU16::new(id).unwrap_or(NonZeroU16::MIN)
  }

  /// The URI for a hyperlink id, if it is still in the table.
  pub fn link_uri(&self, id: NonZeroU16) -> Option<&str> {
    self.links.get(usize::from(id.get()) - 1).map(AsRef::as_ref)
  }

  /// The hyperlink id of the cell at an absolute `(row, col)`, if any.
  pub fn link_at(&self, abs_row: usize, col: usize) -> Option<NonZeroU16> {
    self.abs_row(abs_row).get(col).and_then(|c| c.link)
  }
  /// Detect plain-text URLs across the visible viewport, returning each with
  /// the viewport `(row, col)` of its first character. Soft-wrapped rows are
  /// joined so a URL split across a wrap is found whole; hard line breaks end
  /// a URL.
  pub fn visible_urls(&self) -> Vec<UrlHit> {
    let mut chars: Vec<char> = Vec::new();
    let mut pos: Vec<(usize, usize)> = Vec::new();
    for y in 0..self.rows {
      let line = self.line_at_abs(self.view_to_abs(y));
      for (x, cell) in line.cells.iter().enumerate() {
        if cell.flags.contains(Flags::WIDE_CONT) {
          continue;
        }
        chars.push(cell.c);
        pos.push((y, x));
      }
      if !line.wrapped {
        chars.push('\n'); // a hard break terminates any URL
        pos.push((y, usize::MAX));
      }
    }
    find_urls(&chars)
      .into_iter()
      .map(|(s, e)| {
        let (row, col) = pos[s];
        UrlHit {
          url: chars[s..e].iter().collect(),
          row,
          col,
        }
      })
      .collect()
  }
}
