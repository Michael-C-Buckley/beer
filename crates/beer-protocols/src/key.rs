//! Keyboard encoding: translate decoded key events into the byte sequences a
//! terminal application expects, in both the legacy xterm/VT form and the kitty
//! keyboard progressive-enhancement form.

use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, Modifiers};

/// Encode a key press into bytes for the PTY in the legacy xterm/VT form, or
/// `None` if it produces no input. `app_cursor` selects the SS3 cursor-key form
/// (DECCKM); `app_keypad` selects the SS3 keypad form (DECKPAM).
#[must_use]
pub fn encode(
  event: &KeyEvent,
  mods: Modifiers,
  app_cursor: bool,
  app_keypad: bool,
) -> Option<Vec<u8>> {
  let seq = match event.keysym {
    Keysym::KP_Enter if app_keypad => ss3(b'M'),
    Keysym::Return | Keysym::KP_Enter => prefix_alt(b"\r", mods),
    Keysym::BackSpace => prefix_alt(b"\x7f", mods),
    Keysym::Tab if mods.shift => b"\x1b[Z".to_vec(),
    Keysym::Tab => b"\t".to_vec(),
    Keysym::Escape => b"\x1b".to_vec(),

    Keysym::Up => csi_letter(b'A', mods, app_cursor),
    Keysym::Down => csi_letter(b'B', mods, app_cursor),
    Keysym::Right => csi_letter(b'C', mods, app_cursor),
    Keysym::Left => csi_letter(b'D', mods, app_cursor),
    Keysym::Home => csi_letter(b'H', mods, app_cursor),
    Keysym::End => csi_letter(b'F', mods, app_cursor),

    Keysym::Insert => csi_tilde(2, mods),
    Keysym::Delete => csi_tilde(3, mods),
    Keysym::Page_Up => csi_tilde(5, mods),
    Keysym::Page_Down => csi_tilde(6, mods),

    // F1-F4 are SS3-introduced; F5+ use the CSI tilde forms.
    Keysym::F1 => fkey(b'P', mods),
    Keysym::F2 => fkey(b'Q', mods),
    Keysym::F3 => fkey(b'R', mods),
    Keysym::F4 => fkey(b'S', mods),
    Keysym::F5 => csi_tilde(15, mods),
    Keysym::F6 => csi_tilde(17, mods),
    Keysym::F7 => csi_tilde(18, mods),
    Keysym::F8 => csi_tilde(19, mods),
    Keysym::F9 => csi_tilde(20, mods),
    Keysym::F10 => csi_tilde(21, mods),
    Keysym::F11 => csi_tilde(23, mods),
    Keysym::F12 => csi_tilde(24, mods),

    // Application keypad (DECKPAM) sends SS3 sequences for the numeric keypad;
    // in numeric mode these fall through to their literal characters.
    Keysym::KP_0 if app_keypad => ss3(b'p'),
    Keysym::KP_1 if app_keypad => ss3(b'q'),
    Keysym::KP_2 if app_keypad => ss3(b'r'),
    Keysym::KP_3 if app_keypad => ss3(b's'),
    Keysym::KP_4 if app_keypad => ss3(b't'),
    Keysym::KP_5 if app_keypad => ss3(b'u'),
    Keysym::KP_6 if app_keypad => ss3(b'v'),
    Keysym::KP_7 if app_keypad => ss3(b'w'),
    Keysym::KP_8 if app_keypad => ss3(b'x'),
    Keysym::KP_9 if app_keypad => ss3(b'y'),
    Keysym::KP_Decimal if app_keypad => ss3(b'n'),
    Keysym::KP_Add if app_keypad => ss3(b'k'),
    Keysym::KP_Subtract if app_keypad => ss3(b'm'),
    Keysym::KP_Multiply if app_keypad => ss3(b'j'),
    Keysym::KP_Divide if app_keypad => ss3(b'o'),
    Keysym::KP_Separator if app_keypad => ss3(b'l'),
    Keysym::KP_Equal if app_keypad => ss3(b'X'),

    // Everything else: the xkb-composed text (which already folds in Ctrl),
    // with Alt sending an ESC prefix (meta).
    _ => {
      let text = event.utf8.as_ref()?;
      if text.is_empty() {
        return None;
      }
      prefix_alt(text.as_bytes(), mods)
    },
  };
  Some(seq)
}

/// A key event's kind, for the kitty keyboard protocol's event-type sub-field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyKind {
  /// Initial key press.
  Press   = 1,
  /// Automatic key repeat.
  Repeat  = 2,
  /// Key release.
  Release = 3,
}

/// How a functional (non-text) key is encoded in the kitty protocol.
enum Func {
  /// `CSI number ... u`.
  Number(u32),
  /// `CSI number ... ~`.
  Tilde(u32),
  /// `CSI 1 ... LETTER`.
  Letter(u8),
}

/// Map a keysym to its kitty functional encoding, or `None` for a text key
/// (whose code is its Unicode codepoint).
const fn functional(keysym: Keysym) -> Option<Func> {
  Some(match keysym {
    Keysym::Escape => Func::Number(27),
    Keysym::Return | Keysym::KP_Enter => Func::Number(13),
    Keysym::Tab | Keysym::ISO_Left_Tab => Func::Number(9),
    Keysym::BackSpace => Func::Number(127),
    Keysym::Insert => Func::Tilde(2),
    Keysym::Delete => Func::Tilde(3),
    Keysym::Page_Up => Func::Tilde(5),
    Keysym::Page_Down => Func::Tilde(6),
    Keysym::Up => Func::Letter(b'A'),
    Keysym::Down => Func::Letter(b'B'),
    Keysym::Right => Func::Letter(b'C'),
    Keysym::Left => Func::Letter(b'D'),
    Keysym::Home => Func::Letter(b'H'),
    Keysym::End => Func::Letter(b'F'),
    Keysym::F1 => Func::Letter(b'P'),
    Keysym::F2 => Func::Letter(b'Q'),
    Keysym::F3 => Func::Tilde(13),
    Keysym::F4 => Func::Letter(b'S'),
    Keysym::F5 => Func::Tilde(15),
    Keysym::F6 => Func::Tilde(17),
    Keysym::F7 => Func::Tilde(18),
    Keysym::F8 => Func::Tilde(19),
    Keysym::F9 => Func::Tilde(20),
    Keysym::F10 => Func::Tilde(21),
    Keysym::F11 => Func::Tilde(23),
    Keysym::F12 => Func::Tilde(24),
    _ => return None,
  })
}

/// The kitty key code for a lone modifier key, reported only in report-all
/// mode.
const fn modifier_key(keysym: Keysym) -> Option<u32> {
  Some(match keysym {
    Keysym::Caps_Lock => 57358,
    Keysym::Num_Lock => 57360,
    Keysym::Shift_L => 57441,
    Keysym::Control_L => 57442,
    Keysym::Alt_L => 57443,
    Keysym::Super_L => 57444,
    Keysym::Shift_R => 57447,
    Keysym::Control_R => 57448,
    Keysym::Alt_R => 57449,
    Keysym::Super_R => 57450,
    _ => return None,
  })
}

/// The kitty modifier bitfield (shift=1, alt=2, ctrl=4, super=8); the wire
/// parameter is `1 + this`.
fn kitty_mod_bits(mods: Modifiers) -> u32 {
  u32::from(mods.shift)
    | (u32::from(mods.alt) << 1)
    | (u32::from(mods.ctrl) << 2)
    | (u32::from(mods.logo) << 3)
}

/// The un-shifted Unicode codepoint of a text key (always the lowercase form,
/// per the protocol). `None` if the key produces no character.
fn base_codepoint(keysym: Keysym) -> Option<u32> {
  let c = keysym.key_char()?;
  Some(u32::from(if c.is_ascii_uppercase() {
    c.to_ascii_lowercase()
  } else {
    c
  }))
}

/// Build `CSI <field> [; mod[:event]] [; text] <terminator>`. The modifier
/// section is emitted when modifiers/event/text require it; the event sub-field
/// only when the event is not a plain press.
fn csi(
  field: &str,
  mod_param: u32,
  event: KeyKind,
  text: Option<&str>,
  term: u8,
) -> Vec<u8> {
  let mut s = String::from("\x1b[");
  s.push_str(field);
  let needs_mods = mod_param != 1 || event != KeyKind::Press || text.is_some();
  if needs_mods {
    s.push(';');
    s.push_str(&mod_param.to_string());
    if event != KeyKind::Press {
      s.push(':');
      s.push_str(&(event as u8).to_string());
    }
  }
  if let Some(text) = text {
    s.push(';');
    let codes: Vec<String> =
      text.chars().map(|c| u32::from(c).to_string()).collect();
    s.push_str(&codes.join(":"));
  }
  s.push(char::from(term));
  s.into_bytes()
}

/// Encode a key event under the kitty keyboard protocol with the given active
/// `flags`. Returns `None` when the event produces nothing (e.g. a text key
/// release without report-all mode). `app_cursor` only affects unmodified
/// cursor keys, matching legacy behaviour.
#[must_use]
pub fn kitty_encode(
  event: &KeyEvent,
  mods: Modifiers,
  flags: u8,
  kind: KeyKind,
  app_cursor: bool,
) -> Option<Vec<u8>> {
  let report_all = flags & 0b1000 != 0;
  let report_events = flags & 0b10 != 0;
  let report_alt = flags & 0b100 != 0;
  let report_text = flags & 0b1_0000 != 0;

  let bits = kitty_mod_bits(mods);
  let mod_param = bits + 1;
  // Events other than a press are only reported when asked for. Without
  // event reporting, repeats are re-sent as presses (legacy repeat), but a
  // release must produce nothing. Otherwise a release encodes as a press
  // and a single tap sends the key twice.
  let events_ok = report_events || report_all;
  let kind = if events_ok {
    kind
  } else if kind == KeyKind::Release {
    return None;
  } else {
    KeyKind::Press
  };

  // Lone modifier keys: only in report-all mode.
  if let Some(code) = modifier_key(event.keysym) {
    if !report_all {
      return None;
    }
    return Some(csi(&code.to_string(), mod_param, kind, None, b'u'));
  }

  if let Some(func) = functional(event.keysym) {
    // Enter/Tab/Backspace keep legacy bytes when unmodified and not in
    // report-all mode, so a shell stays usable.
    let legacy_special = matches!(
      event.keysym,
      Keysym::Return | Keysym::KP_Enter | Keysym::Tab | Keysym::BackSpace
    );
    if legacy_special && !report_all && bits == 0 {
      // A repeat re-sends the legacy byte (holding backspace keeps deleting);
      // only a release produces nothing.
      if kind == KeyKind::Release {
        return None;
      }
      return Some(match event.keysym {
        Keysym::BackSpace => b"\x7f".to_vec(),
        Keysym::Tab => b"\t".to_vec(),
        _ => b"\r".to_vec(),
      });
    }
    Some(match func {
      Func::Number(n) => csi(&n.to_string(), mod_param, kind, None, b'u'),
      Func::Tilde(n) => csi(&n.to_string(), mod_param, kind, None, b'~'),
      Func::Letter(l) => {
        if mod_param == 1 && kind == KeyKind::Press {
          // Unmodified: legacy CSI/SS3 form, honouring app-cursor.
          vec![0x1B, if app_cursor { b'O' } else { b'[' }, l]
        } else {
          csi("1", mod_param, kind, None, l)
        }
      },
    })
  } else {
    let cp = base_codepoint(event.keysym)?;
    if report_all {
      if cp == 0 {
        return None;
      }
      let text = if report_text {
        event
          .utf8
          .as_deref()
          .filter(|t| !t.is_empty() && t.chars().all(|c| !c.is_control()))
      } else {
        None
      };
      let field = alt_field(cp, event.keysym, mods, report_alt);
      return Some(csi(&field, mod_param, kind, text, b'u'));
    }
    // Without report-all, text keys send their text on press and repeat
    // (legacy repeat); a release under event reporting produces nothing.
    if kind == KeyKind::Release {
      return None;
    }
    // Plain or shift-only keys send their text; ctrl/alt/super → CSI u.
    if bits & !1 == 0 {
      let text = event.utf8.as_ref()?;
      if text.is_empty() {
        return None;
      }
      return Some(text.as_bytes().to_vec());
    }
    let field = alt_field(cp, event.keysym, mods, report_alt);
    Some(csi(&field, mod_param, KeyKind::Press, None, b'u'))
  }
}

/// The key-code field, with the shifted alternate appended (`code:shifted`)
/// when alternate-key reporting is on and shift is held.
fn alt_field(
  cp: u32,
  keysym: Keysym,
  mods: Modifiers,
  report_alt: bool,
) -> String {
  if report_alt
    && mods.shift
    && let Some(shifted) = keysym.key_char().map(u32::from)
    && shifted != cp
  {
    return format!("{cp}:{shifted}");
  }
  cp.to_string()
}

fn prefix_alt(bytes: &[u8], mods: Modifiers) -> Vec<u8> {
  if mods.alt {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(0x1B);
    out.extend_from_slice(bytes);
    out
  } else {
    bytes.to_vec()
  }
}

/// xterm modifier parameter: 1 + a bitfield of the held modifiers.
fn modifier_param(mods: Modifiers) -> u8 {
  1 + u8::from(mods.shift)
    + (u8::from(mods.alt) << 1)
    + (u8::from(mods.ctrl) << 2)
    + (u8::from(mods.logo) << 3)
}

/// Cursor/edit keys: `ESC [ X` (or `ESC O X` in application-cursor mode), and
/// `ESC [ 1 ; m X` when modifiers are held.
fn csi_letter(final_byte: u8, mods: Modifiers, app_cursor: bool) -> Vec<u8> {
  let m = modifier_param(mods);
  if m == 1 {
    vec![0x1B, if app_cursor { b'O' } else { b'[' }, final_byte]
  } else {
    let mut v = format!("\x1b[1;{m}").into_bytes();
    v.push(final_byte);
    v
  }
}

/// Keypad-style keys: `ESC [ n ~`, with `ESC [ n ; m ~` when modifiers are
/// held.
fn csi_tilde(n: u8, mods: Modifiers) -> Vec<u8> {
  let m = modifier_param(mods);
  if m == 1 {
    format!("\x1b[{n}~").into_bytes()
  } else {
    format!("\x1b[{n};{m}~").into_bytes()
  }
}

/// An SS3-introduced application-keypad sequence (`ESC O c`).
fn ss3(final_byte: u8) -> Vec<u8> {
  vec![0x1B, b'O', final_byte]
}

fn fkey(final_byte: u8, mods: Modifiers) -> Vec<u8> {
  let m = modifier_param(mods);
  if m == 1 {
    vec![0x1B, b'O', final_byte]
  } else {
    let mut v = format!("\x1b[1;{m}").into_bytes();
    v.push(final_byte);
    v
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn key(keysym: Keysym, utf8: Option<&str>) -> KeyEvent {
    KeyEvent {
      time: 0,
      raw_code: 0,
      keysym,
      utf8: utf8.map(str::to_owned),
    }
  }

  const NONE: Modifiers = Modifiers {
    ctrl:      false,
    alt:       false,
    shift:     false,
    caps_lock: false,
    logo:      false,
    num_lock:  false,
  };

  fn mods(ctrl: bool, shift: bool, alt: bool) -> Modifiers {
    Modifiers {
      ctrl,
      shift,
      alt,
      ..NONE
    }
  }

  #[test]
  fn plain_text_passes_through() {
    assert_eq!(
      encode(&key(Keysym::a, Some("a")), NONE, false, false),
      Some(b"a".to_vec())
    );
  }

  #[test]
  fn alt_prefixes_escape() {
    let m = Modifiers { alt: true, ..NONE };
    assert_eq!(
      encode(&key(Keysym::a, Some("a")), m, false, false),
      Some(b"\x1ba".to_vec())
    );
  }

  #[test]
  fn arrows_respect_application_mode() {
    assert_eq!(
      encode(&key(Keysym::Up, None), NONE, false, false),
      Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
      encode(&key(Keysym::Up, None), NONE, true, false),
      Some(b"\x1bOA".to_vec())
    );
  }

  #[test]
  fn keypad_respects_application_mode() {
    // Numeric mode: the keypad digit falls through to its literal character.
    assert_eq!(
      encode(&key(Keysym::KP_5, Some("5")), NONE, false, false),
      Some(b"5".to_vec())
    );
    // Application mode: SS3-introduced sequences for digit and operator keys.
    assert_eq!(
      encode(&key(Keysym::KP_5, Some("5")), NONE, false, true),
      Some(b"\x1bOu".to_vec())
    );
    assert_eq!(
      encode(&key(Keysym::KP_Enter, None), NONE, false, true),
      Some(b"\x1bOM".to_vec())
    );
    // KP_Enter is a plain carriage return outside application mode.
    assert_eq!(
      encode(&key(Keysym::KP_Enter, None), NONE, false, false),
      Some(b"\r".to_vec())
    );
  }

  #[test]
  fn modified_arrow_uses_csi_param() {
    let m = Modifiers { ctrl: true, ..NONE };
    assert_eq!(
      encode(&key(Keysym::Right, None), m, false, false),
      Some(b"\x1b[1;5C".to_vec())
    );
  }

  #[test]
  fn kitty_plain_text_key_stays_text() {
    // Disambiguate only: an unmodified letter is still sent as text.
    assert_eq!(
      kitty_encode(
        &key(Keysym::a, Some("a")),
        NONE,
        0b1,
        KeyKind::Press,
        false
      ),
      Some(b"a".to_vec())
    );
  }

  #[test]
  fn kitty_ctrl_letter_is_csi_u() {
    // ctrl+a → CSI 97 ; 5 u  (5 = 1 + ctrl).
    assert_eq!(
      kitty_encode(
        &key(Keysym::a, None),
        mods(true, false, false),
        0b1,
        KeyKind::Press,
        false
      ),
      Some(b"\x1b[97;5u".to_vec())
    );
  }

  #[test]
  fn kitty_escape_disambiguated() {
    assert_eq!(
      kitty_encode(
        &key(Keysym::Escape, None),
        NONE,
        0b1,
        KeyKind::Press,
        false
      ),
      Some(b"\x1b[27u".to_vec())
    );
  }

  #[test]
  fn kitty_associated_text_and_alternate() {
    // shift+a with report-all + associated-text → CSI 97 ; 2 ; 65 u.
    let flags = 0b1_1001; // disambiguate | report-all | associated-text
    assert_eq!(
      kitty_encode(
        &key(Keysym::A, Some("A")),
        mods(false, true, false),
        flags,
        KeyKind::Press,
        false
      ),
      Some(b"\x1b[97;2;65u".to_vec())
    );
    // ctrl+shift+a with alternate-key reporting → CSI 97:65 ; 6 u.
    assert_eq!(
      kitty_encode(
        &key(Keysym::A, None),
        mods(true, true, false),
        0b101,
        KeyKind::Press,
        false
      ),
      Some(b"\x1b[97:65;6u".to_vec())
    );
  }

  #[test]
  fn kitty_release_only_with_report_all() {
    let release = |flags| {
      kitty_encode(
        &key(Keysym::a, None),
        mods(true, false, false),
        flags,
        KeyKind::Release,
        false,
      )
    };
    // Event reporting alone does not report text-key release.
    assert_eq!(release(0b11), None);
    // Report-all does: CSI 97 ; 5 : 3 u.
    assert_eq!(release(0b1011), Some(b"\x1b[97;5:3u".to_vec()));
  }

  #[test]
  fn kitty_release_suppressed_without_event_reporting() {
    // A release under disambiguate-only (no report-event-types, no
    // report-all) must not be re-encoded as a press, or a single tap sends
    // the key twice. Applies to both functional keys and text keys.
    assert_eq!(
      kitty_encode(&key(Keysym::Up, None), NONE, 0b1, KeyKind::Release, false),
      None
    );
    assert_eq!(
      kitty_encode(
        &key(Keysym::a, Some("a")),
        NONE,
        0b1,
        KeyKind::Release,
        false
      ),
      None
    );
    // A repeat is still re-sent as a press (legacy repeat behaviour).
    assert_eq!(
      kitty_encode(&key(Keysym::Up, None), NONE, 0b1, KeyKind::Repeat, false),
      Some(b"\x1b[A".to_vec())
    );
  }

  #[test]
  fn kitty_legacy_and_text_keys_repeat_under_event_reporting() {
    // With report-event-types on (0b10) but not report-all, backspace/enter/
    // tab stay legacy and a repeat must keep re-sending the legacy byte -
    // holding backspace should keep deleting. A release still sends nothing.
    let flags = 0b11; // disambiguate | report-event-types
    assert_eq!(
      kitty_encode(
        &key(Keysym::BackSpace, None),
        NONE,
        flags,
        KeyKind::Repeat,
        false
      ),
      Some(b"\x7f".to_vec())
    );
    assert_eq!(
      kitty_encode(
        &key(Keysym::BackSpace, None),
        NONE,
        flags,
        KeyKind::Release,
        false
      ),
      None
    );
    // Plain text keys likewise repeat their text under event reporting.
    assert_eq!(
      kitty_encode(
        &key(Keysym::a, Some("a")),
        NONE,
        flags,
        KeyKind::Repeat,
        false
      ),
      Some(b"a".to_vec())
    );
    assert_eq!(
      kitty_encode(
        &key(Keysym::a, Some("a")),
        NONE,
        flags,
        KeyKind::Release,
        false
      ),
      None
    );
  }

  #[test]
  fn kitty_lock_keys_do_not_leak() {
    let numlock = Modifiers {
      num_lock: true,
      ..NONE
    };
    // Num lock on must not turn unmodified Enter into a CSI u sequence.
    assert_eq!(
      kitty_encode(
        &key(Keysym::Return, None),
        numlock,
        0b1,
        KeyKind::Press,
        false
      ),
      Some(b"\r".to_vec())
    );
    // Nor pollute the modifier parameter of a real chord.
    let ctrl_numlock = Modifiers {
      ctrl: true,
      num_lock: true,
      ..NONE
    };
    assert_eq!(
      kitty_encode(
        &key(Keysym::a, None),
        ctrl_numlock,
        0b1,
        KeyKind::Press,
        false
      ),
      Some(b"\x1b[97;5u".to_vec())
    );
  }

  #[test]
  fn kitty_functional_keys() {
    // Unmodified arrow keeps the legacy form (app-cursor honoured).
    assert_eq!(
      kitty_encode(&key(Keysym::Up, None), NONE, 0b1, KeyKind::Press, false),
      Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
      kitty_encode(&key(Keysym::Up, None), NONE, 0b1, KeyKind::Press, true),
      Some(b"\x1bOA".to_vec())
    );
    // Modified arrow → CSI 1 ; mods LETTER.
    assert_eq!(
      kitty_encode(
        &key(Keysym::Up, None),
        mods(true, false, false),
        0b1,
        KeyKind::Press,
        false
      ),
      Some(b"\x1b[1;5A".to_vec())
    );
    // F5 keeps its tilde form.
    assert_eq!(
      kitty_encode(&key(Keysym::F5, None), NONE, 0b1, KeyKind::Press, false),
      Some(b"\x1b[15~".to_vec())
    );
    // Unmodified Enter is still a carriage return; report-all makes it CSI u.
    assert_eq!(
      kitty_encode(
        &key(Keysym::Return, None),
        NONE,
        0b1,
        KeyKind::Press,
        false
      ),
      Some(b"\r".to_vec())
    );
    assert_eq!(
      kitty_encode(
        &key(Keysym::Return, None),
        NONE,
        0b1001,
        KeyKind::Press,
        false
      ),
      Some(b"\x1b[13u".to_vec())
    );
  }

  #[test]
  fn special_keys() {
    assert_eq!(
      encode(&key(Keysym::Return, None), NONE, false, false),
      Some(b"\r".to_vec())
    );
    assert_eq!(
      encode(&key(Keysym::BackSpace, None), NONE, false, false),
      Some(b"\x7f".to_vec())
    );
    assert_eq!(
      encode(&key(Keysym::Delete, None), NONE, false, false),
      Some(b"\x1b[3~".to_vec())
    );
    assert_eq!(
      encode(&key(Keysym::F5, None), NONE, false, false),
      Some(b"\x1b[15~".to_vec())
    );
  }
}
