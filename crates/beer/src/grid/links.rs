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

/// Find common URL forms in `chars`, returning `(start, end)` index ranges.
/// The scheme is a run of `[A-Za-z][A-Za-z0-9+.-]*` before `://`; the body runs
/// to the first non-URL character, with trailing sentence punctuation trimmed.
fn find_urls(chars: &[char]) -> Vec<(usize, usize)> {
  let mut out = Vec::new();
  let mut i = 0;
  while i < chars.len() {
    if let Some(range) = www_at(chars, i)
      .or_else(|| scheme_url_at(chars, i))
      .or_else(|| short_scheme_at(chars, i))
    {
      i = range.1;
      out.push(range);
    } else {
      i += 1;
    }
  }
  out
}

fn www_at(chars: &[char], start: usize) -> Option<(usize, usize)> {
  chars[start..]
    .starts_with(&['w', 'w', 'w', '.'])
    .then(|| {
      let end = url_end(chars, start + 4);
      (start, end)
    })
    .filter(|(_, end)| *end > start + 4)
}

fn scheme_url_at(chars: &[char], colon: usize) -> Option<(usize, usize)> {
  if chars.get(colon..colon + 3) != Some(&[':', '/', '/']) {
    return None;
  }
  let start = scheme_start(chars, colon, false);
  let end = url_end(chars, colon + 3);
  (start < colon && chars[start].is_ascii_alphabetic() && end > colon + 3)
    .then_some((start, end))
}

fn short_scheme_at(chars: &[char], colon: usize) -> Option<(usize, usize)> {
  if chars.get(colon) != Some(&':') {
    return None;
  }
  let start = scheme_start(chars, colon, true);
  let scheme: String = chars[start..colon].iter().collect();
  let end = url_end(chars, colon + 1);
  (matches!(scheme.as_str(), "mailto" | "tel" | "magnet" | "news")
    && end > colon + 1)
    .then_some((start, end))
}

fn scheme_start(chars: &[char], end: usize, letters_only: bool) -> usize {
  let mut start = end;
  while start > 0 {
    let c = chars[start - 1];
    if c.is_ascii_alphabetic()
      || (!letters_only && (c.is_ascii_digit() || matches!(c, '+' | '-' | '.')))
    {
      start -= 1;
    } else {
      break;
    }
  }
  start
}

fn url_end(chars: &[char], minimum: usize) -> usize {
  let mut end = minimum;
  while end < chars.len() && is_url_char(chars[end]) {
    end += 1;
  }
  while end > minimum
    && matches!(
      chars[end - 1],
      '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '\'' | '"'
    )
  {
    end -= 1;
  }
  end
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
          url: {
            let raw: String = chars[s..e].iter().collect();
            if raw.starts_with("www.") {
              format!("https://{raw}")
            } else {
              raw
            }
          },
          row,
          col,
        }
      })
      .collect()
  }
}
