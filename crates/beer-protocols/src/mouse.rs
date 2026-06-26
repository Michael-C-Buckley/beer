//! Mouse-event encoding: frame a button/motion report in the wire form the
//! application selected (legacy byte form, UTF-8, or SGR).

use std::io::Write as _;

use smithay_client_toolkit::seat::keyboard::Modifiers;

use crate::style::MouseEncoding;

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

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: false,
        caps_lock: false,
        logo: false,
        num_lock: false,
    };

    #[test]
    fn mouse_sgr_press_and_release() {
        // Left press at column 3, row 5 (0-based) -> SGR with 1-based coords.
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
}
