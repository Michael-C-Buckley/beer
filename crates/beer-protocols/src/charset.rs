//! G0/G1 character-set designation and the DEC special graphics (line-drawing)
//! translation.

/// A designated character set (`ESC ( c` / `ESC ) c`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Charset {
  Ascii,
  DecSpecial,
}

/// Map a designation byte to a [`Charset`]. `0` selects DEC special graphics;
/// everything else falls back to ASCII.
pub fn charset(byte: u8) -> Charset {
  match byte {
    b'0' => Charset::DecSpecial,
    _ => Charset::Ascii,
  }
}

/// Translate a byte under the DEC special graphics set (VT100 line drawing).
pub fn dec_special(c: char) -> char {
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
}
