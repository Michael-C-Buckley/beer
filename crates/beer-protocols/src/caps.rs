//! Terminfo capability values reported via XTGETTCAP (`DCS + q <name> ST`).

/// Look up a terminfo capability beer reports via XTGETTCAP, by its terminfo
/// name. `None` means the capability is unknown (the reply is then a negative
/// `DCS 0 + r`).
#[must_use]
pub fn cap_value(name: &[u8]) -> Option<&'static str> {
  match name {
    b"TN" => Some("beer"),
    b"Co" | b"colors" => Some("256"),
    b"RGB" => Some("8/8/8"),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn known_and_unknown_caps() {
    assert_eq!(cap_value(b"TN"), Some("beer"));
    assert_eq!(cap_value(b"colors"), Some("256"));
    assert_eq!(cap_value(b"nope"), None);
  }
}
