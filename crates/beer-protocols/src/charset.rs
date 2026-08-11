//! G0/G1 character-set designation and the DEC special graphics (line-drawing)
//! translation.

/// A designated character set (`ESC ( c` and the G1/G2/G3 variants).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Charset {
  /// ASCII character set.
  Ascii,
  /// DEC special graphics character set.
  DecSpecial,
  /// United Kingdom national set.
  Uk,
}

/// Map a designation byte to a [`Charset`]. `0` selects DEC special graphics,
/// `A` the UK national set; everything else falls back to ASCII.
#[must_use]
pub const fn charset(byte: u8) -> Charset {
  match byte {
    b'0' => Charset::DecSpecial,
    b'A' => Charset::Uk,
    _ => Charset::Ascii,
  }
}

/// Translate a printed character under the active character set.
#[must_use]
pub const fn translate(set: Charset, c: char) -> char {
  match set {
    Charset::DecSpecial => dec_special(c),
    // The UK set differs from ASCII only in mapping `#` to the pound sign.
    Charset::Uk if c == '#' => '£',
    _ => c,
  }
}

/// Translate a byte under the DEC special graphics set (VT100 line drawing).
#[must_use]
pub const fn dec_special(c: char) -> char {
  match c {
    '`' => '◆',
    'a' => '▒',
    'f' => '°',
    'g' => '±',
    'j' => '┘',
    'k' => '┐',
    'l' => '┌',
    'm' => '└',
    'n' => '┼',
    'o' => '⎺',
    'p' => '⎻',
    'q' => '─',
    'r' => '⎼',
    's' => '⎽',
    't' => '├',
    'u' => '┤',
    'v' => '┴',
    'w' => '┬',
    'x' => '│',
    'y' => '≤',
    'z' => '≥',
    '~' => '·',
    _ => c,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn line_drawing_translates() {
    assert_eq!(charset(b'0'), Charset::DecSpecial);
    assert_eq!(charset(b'B'), Charset::Ascii);
    assert_eq!(dec_special('q'), '─');
    assert_eq!(dec_special('x'), '│');
    assert_eq!(dec_special('A'), 'A'); // unmapped passes through
  }

  #[test]
  fn translate_applies_designated_set() {
    assert_eq!(charset(b'A'), Charset::Uk);
    assert_eq!(translate(Charset::Uk, '#'), '£');
    assert_eq!(translate(Charset::Uk, '$'), '$');
    assert_eq!(translate(Charset::DecSpecial, 'q'), '─');
    assert_eq!(translate(Charset::Ascii, '#'), '#');
  }
}
