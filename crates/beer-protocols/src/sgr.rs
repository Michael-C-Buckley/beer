//! SGR (Select Graphic Rendition) parsing: the colour and underline forms that
//! need more than a single parameter. Plain attribute toggles (bold, reverse,
//! ...) stay in the dispatcher; what lives here is the multi-parameter parsing
//! that benefits from being testable in isolation.

use crate::style::{Color, Underline};

/// Map an SGR 4 parameter (`4` or `4:x`) to an underline style.
pub fn underline_from(param: &[u16]) -> Underline {
  match param.get(1).copied().unwrap_or(1) {
    0 => Underline::None,
    2 => Underline::Double,
    3 => Underline::Curly,
    4 => Underline::Dotted,
    5 => Underline::Dashed,
    _ => Underline::Single,
  }
}

/// Parse an SGR 38/48/58 extended colour, given the full parameter list and the
/// index of the introducer. Returns the colour and how many top-level
/// parameters it consumed (1 for the colon-subparameter form, more for the
/// legacy semicolon form).
pub fn ext_color(items: &[&[u16]], i: usize) -> (Option<Color>, usize) {
  let head = items[i];
  if head.len() >= 2 {
    return (color_from_subparams(&head[1..]), 1);
  }
  match items.get(i + 1).and_then(|s| s.first().copied()) {
    Some(5) => {
      let idx = items
        .get(i + 2)
        .and_then(|s| s.first().copied())
        .unwrap_or(0);
      (Some(Color::Indexed(idx as u8)), 3)
    },
    Some(2) => {
      let get = |k: usize| {
        items
          .get(i + k)
          .and_then(|s| s.first().copied())
          .unwrap_or(0)
      };
      (
        Some(Color::Rgb(get(2) as u8, get(3) as u8, get(4) as u8)),
        5,
      )
    },
    _ => (None, 1),
  }
}

/// Parse the colon-subparameter colour form: `5:idx` (indexed) or
/// `2[:cs]:r:g:b` (direct RGB, with an optional colour-space id that is
/// ignored).
fn color_from_subparams(sub: &[u16]) -> Option<Color> {
  match sub.first().copied() {
    Some(5) => sub.get(1).map(|&i| Color::Indexed(i as u8)),
    Some(2) => {
      // Either `2:r:g:b` or `2:colorspace:r:g:b`.
      let rgb = if sub.len() >= 5 {
        &sub[2..5]
      } else {
        &sub[1..]
      };
      match rgb {
        [r, g, b, ..] => Some(Color::Rgb(*r as u8, *g as u8, *b as u8)),
        _ => None,
      }
    },
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn underline_styles() {
    assert_eq!(underline_from(&[4]), Underline::Single);
    assert_eq!(underline_from(&[4, 3]), Underline::Curly);
    assert_eq!(underline_from(&[4, 0]), Underline::None);
  }

  #[test]
  fn extended_colour_semicolon_and_colon() {
    // `38;2;10;20;30` direct RGB.
    let items: Vec<&[u16]> = vec![&[38], &[2], &[10], &[20], &[30]];
    assert_eq!(ext_color(&items, 0), (Some(Color::Rgb(10, 20, 30)), 5));
    // `38:2:40:50:60` colon subparameters.
    let items: Vec<&[u16]> = vec![&[38, 2, 40, 50, 60]];
    assert_eq!(ext_color(&items, 0), (Some(Color::Rgb(40, 50, 60)), 1));
    // `38;5;1` indexed.
    let items: Vec<&[u16]> = vec![&[38], &[5], &[1]];
    assert_eq!(ext_color(&items, 0), (Some(Color::Indexed(1)), 3));
  }
}
