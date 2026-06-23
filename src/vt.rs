//! VT emulation: feed bytes through `vte` and drive the [`Grid`].

use std::io::Write as _;

use vte::{Params, Perform};

use crate::grid::{Color, Flags, Grid};

/// G0/G1 character set designation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Charset {
    Ascii,
    DecSpecial,
}

/// The terminal model: a grid plus the escape-sequence state around it.
#[derive(Debug)]
pub struct Term {
    grid: Grid,
    title: Option<String>,
    response: Vec<u8>,
    g0: Charset,
    g1: Charset,
    shift_out: bool,
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            grid: Grid::new(cols, rows),
            title: None,
            response: Vec::new(),
            g0: Charset::Ascii,
            g1: Charset::Ascii,
            shift_out: false,
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Bytes the terminal needs to send back to the application (DA, CPR, ...).
    pub fn take_response(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.response)
    }

    fn active_charset(&self) -> Charset {
        if self.shift_out { self.g1 } else { self.g0 }
    }

    fn set_mode(&mut self, params: &Params, private: bool, on: bool) {
        for p in params.iter() {
            let Some(&code) = p.first() else { continue };
            match (private, code) {
                (true, 6) => self.grid.set_origin(on),
                (true, 7) => self.grid.set_autowrap(on),
                (true, 1049) => {
                    if on {
                        self.grid.save_cursor();
                        self.grid.enter_alt_screen();
                        self.grid.erase_display(2);
                    } else {
                        self.grid.leave_alt_screen();
                        self.grid.restore_cursor();
                    }
                }
                (true, 47 | 1047) => {
                    if on {
                        self.grid.enter_alt_screen();
                    } else {
                        self.grid.leave_alt_screen();
                    }
                }
                (false, 4) => self.grid.set_insert(on),
                // App-cursor/bracketed-paste/mouse/sync modes affect input and
                // rendering, which arrive with the keyboard and renderer.
                _ => tracing::trace!("unhandled mode {code} private={private} on={on}"),
            }
        }
    }

    fn sgr(&mut self, params: &Params) {
        let items: Vec<&[u16]> = params.iter().collect();
        if items.is_empty() {
            self.grid.reset_pen();
            return;
        }
        let mut i = 0;
        while i < items.len() {
            let p = items[i];
            let code = p.first().copied().unwrap_or(0);
            let pen = self.grid.pen_mut();
            let mut step = 1;
            match code {
                0 => *pen = Default::default(),
                1 => pen.flags.insert(Flags::BOLD),
                2 => pen.flags.insert(Flags::DIM),
                3 => pen.flags.insert(Flags::ITALIC),
                4 => pen.flags.insert(Flags::UNDERLINE),
                5 | 6 => pen.flags.insert(Flags::BLINK),
                7 => pen.flags.insert(Flags::REVERSE),
                8 => pen.flags.insert(Flags::HIDDEN),
                9 => pen.flags.insert(Flags::STRIKE),
                21 | 22 => pen.flags.remove(Flags::BOLD.union(Flags::DIM)),
                23 => pen.flags.remove(Flags::ITALIC),
                24 => pen.flags.remove(Flags::UNDERLINE),
                25 => pen.flags.remove(Flags::BLINK),
                27 => pen.flags.remove(Flags::REVERSE),
                28 => pen.flags.remove(Flags::HIDDEN),
                29 => pen.flags.remove(Flags::STRIKE),
                30..=37 => pen.fg = Color::Indexed((code - 30) as u8),
                39 => pen.fg = Color::Default,
                40..=47 => pen.bg = Color::Indexed((code - 40) as u8),
                49 => pen.bg = Color::Default,
                90..=97 => pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => pen.bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 => {
                    let (color, consumed) = ext_color(&items, i);
                    if let Some(color) = color {
                        if code == 38 {
                            pen.fg = color;
                        } else {
                            pen.bg = color;
                        }
                    }
                    step = consumed;
                }
                // 58/59 (underline colour) and others: no storage yet.
                _ => {}
            }
            i += step;
        }
    }

    fn device_attrs(&mut self, secondary: bool) {
        // Claim a VT220 with ANSI colour (62;22) for DA1, and a generic
        // firmware level for DA2.
        if secondary {
            self.response.extend_from_slice(b"\x1b[>0;276;0c");
        } else {
            self.response.extend_from_slice(b"\x1b[?62;22c");
        }
    }

    fn device_status(&mut self, params: &Params) {
        match params.iter().next().and_then(|p| p.first().copied()) {
            Some(5) => self.response.extend_from_slice(b"\x1b[0n"),
            Some(6) => {
                let (x, y) = self.grid.cursor();
                let _ = write!(self.response, "\x1b[{};{}R", y + 1, x + 1);
            }
            _ => {}
        }
    }
}

/// First param value, with 0/absent folded to `default` (xterm convention for
/// cursor movement and counts).
fn n(params: &Params, idx: usize, default: usize) -> usize {
    match params.iter().nth(idx).and_then(|p| p.first().copied()) {
        Some(0) | None => default,
        Some(v) => v as usize,
    }
}

/// Raw first param value (0 is meaningful), defaulting to 0 when absent.
fn raw(params: &Params, idx: usize) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|p| p.first().copied())
        .unwrap_or(0)
}

/// Parse an SGR 38/48 extended colour, returning the colour and how many
/// top-level params it consumed (1 for colon form, more for semicolon form).
fn ext_color(items: &[&[u16]], i: usize) -> (Option<Color>, usize) {
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
        }
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
        }
        _ => (None, 1),
    }
}

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
        }
        _ => None,
    }
}

fn charset(byte: u8) -> Charset {
    match byte {
        b'0' => Charset::DecSpecial,
        _ => Charset::Ascii,
    }
}

/// Translate a byte under the DEC special graphics set (line drawing).
fn dec_special(c: char) -> char {
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

impl Perform for Term {
    fn print(&mut self, c: char) {
        let c = if self.active_charset() == Charset::DecSpecial {
            dec_special(c)
        } else {
            c
        };
        self.grid.print(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => self.grid.backspace(),
            0x09 => self.grid.tab(),
            0x0A..=0x0C => self.grid.line_feed(),
            0x0D => self.grid.carriage_return(),
            0x0E => self.shift_out = true,
            0x0F => self.shift_out = false,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first() == Some(&b'?');
        let secondary = intermediates.first() == Some(&b'>');
        match action {
            'A' => self.grid.cursor_up(n(params, 0, 1)),
            'B' | 'e' => self.grid.cursor_down(n(params, 0, 1)),
            'C' | 'a' => self.grid.cursor_fwd(n(params, 0, 1)),
            'D' => self.grid.cursor_back(n(params, 0, 1)),
            'E' => {
                self.grid.cursor_down(n(params, 0, 1));
                self.grid.carriage_return();
            }
            'F' => {
                self.grid.cursor_up(n(params, 0, 1));
                self.grid.carriage_return();
            }
            'G' | '`' => self.grid.move_to_col(n(params, 0, 1) - 1),
            'd' => self.grid.move_to_row(n(params, 0, 1) - 1),
            'H' | 'f' => self.grid.move_to(n(params, 1, 1) - 1, n(params, 0, 1) - 1),
            'J' => self.grid.erase_display(raw(params, 0)),
            'K' => self.grid.erase_line(raw(params, 0)),
            '@' => self.grid.insert_chars(n(params, 0, 1)),
            'P' => self.grid.delete_chars(n(params, 0, 1)),
            'L' => self.grid.insert_lines(n(params, 0, 1)),
            'M' => self.grid.delete_lines(n(params, 0, 1)),
            'X' => self.grid.erase_chars(n(params, 0, 1)),
            'S' => self.grid.scroll_up(n(params, 0, 1)),
            'T' => self.grid.scroll_down(n(params, 0, 1)),
            'm' => self.sgr(params),
            'r' => {
                let top = n(params, 0, 1) - 1;
                let bottom = match params.iter().nth(1).and_then(|p| p.first().copied()) {
                    Some(0) | None => self.grid.rows() - 1,
                    Some(v) => (v as usize).saturating_sub(1),
                };
                self.grid.set_scroll_region(top, bottom);
            }
            'h' => self.set_mode(params, private, true),
            'l' => self.set_mode(params, private, false),
            'c' => self.device_attrs(secondary),
            'n' => self.device_status(params),
            's' => self.grid.save_cursor(),
            'u' => self.grid.restore_cursor(),
            'g' => match raw(params, 0) {
                3 => self.grid.clear_all_tabs(),
                _ => self.grid.clear_tab(),
            },
            _ => tracing::trace!("unhandled CSI {action:?} {intermediates:?}"),
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates.first().copied(), byte) {
            (None, b'D') => self.grid.line_feed(),
            (None, b'M') => self.grid.reverse_index(),
            (None, b'E') => self.grid.next_line(),
            (None, b'7') => self.grid.save_cursor(),
            (None, b'8') => self.grid.restore_cursor(),
            (None, b'H') => self.grid.set_tab(),
            (None, b'c') => {
                self.grid.reset_pen();
                self.grid.set_scroll_region(0, self.grid.rows() - 1);
                self.grid.set_autowrap(true);
                self.grid.set_origin(false);
                self.grid.erase_display(2);
                self.grid.move_to(0, 0);
                self.g0 = Charset::Ascii;
                self.g1 = Charset::Ascii;
                self.shift_out = false;
            }
            (Some(b'('), c) => self.g0 = charset(c),
            (Some(b')'), c) => self.g1 = charset(c),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        match params.first() {
            Some(&n) if n == b"0" || n == b"2" => {
                if let Some(text) = params.get(1) {
                    self.title = Some(String::from_utf8_lossy(text).into_owned());
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(term: &mut Term, bytes: &[u8]) {
        let mut parser = vte::Parser::new();
        parser.advance(term, bytes);
    }

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut t = Term::new(20, 4);
        feed(&mut t, b"hello");
        assert_eq!(t.grid().row_text(0), "hello");
    }

    #[test]
    fn cursor_position_and_erase() {
        let mut t = Term::new(20, 4);
        feed(&mut t, b"abcde\x1b[Hxyz");
        assert_eq!(t.grid().row_text(0), "xyzde");
    }

    #[test]
    fn newline_sequence() {
        let mut t = Term::new(20, 4);
        feed(&mut t, b"one\r\ntwo");
        assert_eq!(t.grid().row_text(0), "one");
        assert_eq!(t.grid().row_text(1), "two");
    }

    #[test]
    fn sgr_sets_pen_colours() {
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1b[31;1mX");
        let cell = t.grid().cell(0, 0);
        assert_eq!(cell.fg, Color::Indexed(1));
        assert!(cell.flags.contains(Flags::BOLD));
    }

    #[test]
    fn truecolor_semicolon_and_colon() {
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1b[38;2;10;20;30mA");
        assert_eq!(t.grid().cell(0, 0).fg, Color::Rgb(10, 20, 30));
        feed(&mut t, b"\x1b[38:2:40:50:60mB");
        assert_eq!(t.grid().cell(1, 0).fg, Color::Rgb(40, 50, 60));
    }

    #[test]
    fn device_status_reports_cursor() {
        let mut t = Term::new(20, 4);
        feed(&mut t, b"\x1b[3;5H\x1b[6n");
        assert_eq!(t.take_response(), b"\x1b[3;5R");
    }

    #[test]
    fn line_drawing_charset() {
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1b(0qx\x1b(B");
        assert_eq!(t.grid().row_text(0), "─│");
    }

    #[test]
    fn title_via_osc() {
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1b]0;hello\x07");
        assert_eq!(t.title(), Some("hello"));
    }

    #[test]
    fn alt_screen_preserves_primary() {
        let mut t = Term::new(20, 2);
        feed(&mut t, b"main");
        feed(&mut t, b"\x1b[?1049h");
        assert_eq!(t.grid().row_text(0), "");
        feed(&mut t, b"\x1b[?1049l");
        assert_eq!(t.grid().row_text(0), "main");
    }
}
