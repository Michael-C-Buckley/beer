//! Keyboard encoding: translate decoded key events into the byte sequences a
//! terminal application expects (xterm/VT-style).

use std::io::Write as _;

use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, Modifiers};

use crate::grid::MouseEncoding;

/// Encode a key press into bytes for the PTY, or `None` if it produces no input.
pub fn encode(event: &KeyEvent, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let seq = match event.keysym {
        Keysym::Return | Keysym::KP_Enter => prefix_alt(b"\r".to_vec(), mods),
        Keysym::BackSpace => prefix_alt(b"\x7f".to_vec(), mods),
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

        // Everything else: the xkb-composed text (which already folds in Ctrl),
        // with Alt sending an ESC prefix (meta).
        _ => {
            let text = event.utf8.as_ref()?;
            if text.is_empty() {
                return None;
            }
            prefix_alt(text.as_bytes().to_vec(), mods)
        }
    };
    Some(seq)
}

/// Encode a mouse event for the application. `button` is the base code (0/1/2
/// for left/middle/right, 64/65 for wheel up/down); `col`/`row` are 0-based
/// screen cells; `motion` marks a drag/move report. Returns the bytes to send.
pub fn encode_mouse(
    encoding: MouseEncoding,
    button: u8,
    col: usize,
    row: usize,
    pressed: bool,
    motion: bool,
    mods: Modifiers,
) -> Vec<u8> {
    let mod_bits =
        (u8::from(mods.shift) << 2) | (u8::from(mods.alt) << 3) | (u8::from(mods.ctrl) << 4);
    let motion_bit = if motion { 32 } else { 0 };
    let mut out = Vec::new();
    match encoding {
        MouseEncoding::Sgr => {
            let cb = u32::from(button) + u32::from(motion_bit) + u32::from(mod_bits);
            let final_byte = if pressed { 'M' } else { 'm' };
            let _ = write!(out, "\x1b[<{cb};{};{}{final_byte}", col + 1, row + 1);
        }
        enc => {
            // Legacy form collapses every button release to code 3.
            let base = if !pressed && button <= 2 { 3 } else { button };
            let cb = 32 + base + motion_bit + mod_bits;
            out.extend_from_slice(b"\x1b[M");
            push_coord(&mut out, u32::from(cb), false);
            let utf8 = enc == MouseEncoding::Utf8;
            push_coord(&mut out, col as u32 + 33, utf8);
            push_coord(&mut out, row as u32 + 33, utf8);
        }
    }
    out
}

/// Push one legacy mouse coordinate byte, UTF-8 encoding values above 127 when
/// the extended (1005) mode is active, and clamping otherwise.
fn push_coord(out: &mut Vec<u8>, value: u32, utf8: bool) {
    if utf8 {
        let v = value.min(0x7ff);
        if v >= 0x80 {
            out.push(0xc0 | (v >> 6) as u8);
            out.push(0x80 | (v & 0x3f) as u8);
        } else {
            out.push(v as u8);
        }
    } else {
        out.push(value.min(255) as u8);
    }
}

fn prefix_alt(bytes: Vec<u8>, mods: Modifiers) -> Vec<u8> {
    if mods.alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bytes);
        out
    } else {
        bytes
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
        vec![0x1b, if app_cursor { b'O' } else { b'[' }, final_byte]
    } else {
        let mut v = format!("\x1b[1;{m}").into_bytes();
        v.push(final_byte);
        v
    }
}

/// Keypad-style keys: `ESC [ n ~`, with `ESC [ n ; m ~` when modifiers are held.
fn csi_tilde(n: u8, mods: Modifiers) -> Vec<u8> {
    let m = modifier_param(mods);
    if m == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{m}~").into_bytes()
    }
}

fn fkey(final_byte: u8, mods: Modifiers) -> Vec<u8> {
    let m = modifier_param(mods);
    if m == 1 {
        vec![0x1b, b'O', final_byte]
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
        ctrl: false,
        alt: false,
        shift: false,
        caps_lock: false,
        logo: false,
        num_lock: false,
    };

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(
            encode(&key(Keysym::a, Some("a")), NONE, false),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        let mods = Modifiers { alt: true, ..NONE };
        assert_eq!(
            encode(&key(Keysym::a, Some("a")), mods, false),
            Some(b"\x1ba".to_vec())
        );
    }

    #[test]
    fn arrows_respect_application_mode() {
        assert_eq!(
            encode(&key(Keysym::Up, None), NONE, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode(&key(Keysym::Up, None), NONE, true),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn modified_arrow_uses_csi_param() {
        let mods = Modifiers { ctrl: true, ..NONE };
        assert_eq!(
            encode(&key(Keysym::Right, None), mods, false),
            Some(b"\x1b[1;5C".to_vec())
        );
    }

    #[test]
    fn mouse_sgr_press_and_release() {
        // Left press at column 3, row 5 (0-based) → SGR with 1-based coords.
        assert_eq!(
            encode_mouse(MouseEncoding::Sgr, 0, 3, 5, true, false, NONE),
            b"\x1b[<0;4;6M".to_vec()
        );
        assert_eq!(
            encode_mouse(MouseEncoding::Sgr, 0, 3, 5, false, false, NONE),
            b"\x1b[<0;4;6m".to_vec()
        );
    }

    #[test]
    fn mouse_legacy_release_collapses_to_three() {
        // Legacy release: button bits become 3; values offset by 32.
        assert_eq!(
            encode_mouse(MouseEncoding::X10, 0, 0, 0, false, false, NONE),
            vec![0x1b, b'[', b'M', 32 + 3, 33, 33]
        );
    }

    #[test]
    fn mouse_motion_and_modifiers_set_bits() {
        let mods = Modifiers { ctrl: true, ..NONE };
        // Drag with the left button and ctrl held: 0 + 32(motion) + 16(ctrl).
        assert_eq!(
            encode_mouse(MouseEncoding::Sgr, 0, 0, 0, true, true, mods),
            b"\x1b[<48;1;1M".to_vec()
        );
    }

    #[test]
    fn special_keys() {
        assert_eq!(
            encode(&key(Keysym::Return, None), NONE, false),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            encode(&key(Keysym::BackSpace, None), NONE, false),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            encode(&key(Keysym::Delete, None), NONE, false),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode(&key(Keysym::F5, None), NONE, false),
            Some(b"\x1b[15~".to_vec())
        );
    }
}
