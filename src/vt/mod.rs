//! VT emulation: feed bytes through `vte` and drive the [`Grid`].

mod perform;

use std::io::Write as _;

use vte::Params;

use crate::grid::{
    Color, CursorShape, Flags, Grid, MouseEncoding, MouseProtocol, PromptKind, Underline,
};
use crate::theme::{Rgb, Theme};

/// G0/G1 character set designation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Charset {
    Ascii,
    DecSpecial,
}

/// Which device-attributes query is being answered.
#[derive(Clone, Copy, Debug)]
enum DaLevel {
    Primary,
    Secondary,
    Tertiary,
}

/// A clipboard request from the application (OSC 52), for the front-end to act
/// on since it owns the Wayland selections.
#[derive(Clone, Debug)]
pub enum ClipboardOp {
    /// Set the clipboard (or primary) to `text`.
    Set { primary: bool, text: String },
    /// Report the current clipboard (or primary) contents back to the app.
    Query { primary: bool },
}

/// A desktop notification an application requested (OSC 9 / 777 / 99), for the
/// front-end to deliver via the configured notifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub title: Option<String>,
    pub body: String,
}

/// Which dynamic colour an OSC 10/11/17/19 escape targets.
#[derive(Clone, Copy, Debug)]
enum Dynamic {
    Fg,
    Bg,
    SelBg,
    SelFg,
}

/// DECRQM mode-state code: 1 = set, 2 = reset.
fn set_reset(on: bool) -> u8 {
    if on { 1 } else { 2 }
}

/// Parse an OSC colour spec into an [`Rgb`].
fn parse_spec(spec: &[u8]) -> Option<Rgb> {
    std::str::from_utf8(spec)
        .ok()
        .and_then(crate::theme::parse_color)
}

/// Parse a decimal palette index (0-255).
fn parse_index(b: &[u8]) -> Option<u8> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

fn rgb_tuple(rgb: Rgb) -> (u8, u8, u8) {
    (rgb.0, rgb.1, rgb.2)
}

/// Select `protocol` when a mouse mode is set, else turn reporting off.
fn proto(on: bool, protocol: MouseProtocol) -> MouseProtocol {
    if on { protocol } else { MouseProtocol::Off }
}

/// Select `encoding` when its mode is set, else fall back to the default form.
fn enc(on: bool, encoding: MouseEncoding) -> MouseEncoding {
    if on { encoding } else { MouseEncoding::X10 }
}

/// Map an SGR 4 param (`4` or `4:x`) to an underline style.
fn underline_from(param: &[u16]) -> Underline {
    match param.get(1).copied().unwrap_or(1) {
        0 => Underline::None,
        2 => Underline::Double,
        3 => Underline::Curly,
        4 => Underline::Dotted,
        5 => Underline::Dashed,
        _ => Underline::Single,
    }
}

/// The terminal model: a grid plus the escape-sequence state around it.
#[derive(Debug)]
pub struct Term {
    grid: Grid,
    title: Option<String>,
    title_stack: Vec<Option<String>>,
    response: Vec<u8>,
    g0: Charset,
    g1: Charset,
    shift_out: bool,
    /// Accumulated payload of an in-progress `DCS + q` (XTGETTCAP) query.
    xtgettcap: Option<Vec<u8>>,
    /// Pending OSC 52 clipboard requests, drained by the front-end.
    clipboard_ops: Vec<ClipboardOp>,
    /// The active colour scheme (seeded from config, mutated by OSC escapes).
    theme: Theme,
    /// Set when the child rings the bell (`BEL`); cleared by the front-end.
    bell: bool,
    /// Working directory reported by the shell via OSC 7, for new windows.
    cwd: Option<String>,
    /// Desktop notifications requested via OSC 9/777/99, drained by the front-end.
    notifications: Vec<Notification>,
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            grid: Grid::new(cols, rows),
            title: None,
            title_stack: Vec::new(),
            response: Vec::new(),
            g0: Charset::Ascii,
            g1: Charset::Ascii,
            shift_out: false,
            xtgettcap: None,
            clipboard_ops: Vec::new(),
            theme: Theme::default(),
            bell: false,
            cwd: None,
            notifications: Vec::new(),
        }
    }

    /// The working directory last reported by the shell (OSC 7), if any.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Drain the desktop notifications requested since the last call.
    pub fn take_notifications(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.notifications)
    }

    /// Take and clear the pending bell flag.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Drain the OSC 52 clipboard requests accumulated since the last call.
    pub fn take_clipboard_ops(&mut self) -> Vec<ClipboardOp> {
        std::mem::take(&mut self.clipboard_ops)
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Replace the colour scheme (config load / reload).
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Answer an XTGETTCAP query: for each hex-encoded capability name, reply
    /// with `DCS 1 + r name=value ST` if known, else `DCS 0 + r name ST`.
    fn answer_xtgettcap(&mut self, payload: &[u8]) {
        for name_hex in payload.split(|&b| b == b';') {
            let value = decode_hex(name_hex).and_then(|name| cap_value(&name));
            match value {
                Some(value) => {
                    self.response.extend_from_slice(b"\x1bP1+r");
                    self.response.extend_from_slice(name_hex);
                    self.response.push(b'=');
                    for byte in value.bytes() {
                        let _ = write!(self.response, "{byte:02x}");
                    }
                    self.response.extend_from_slice(b"\x1b\\");
                }
                None => {
                    self.response.extend_from_slice(b"\x1bP0+r");
                    self.response.extend_from_slice(name_hex);
                    self.response.extend_from_slice(b"\x1b\\");
                }
            }
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.grid.resize(cols, rows);
    }

    pub fn scroll_view(&mut self, delta: isize) {
        self.grid.scroll_view(delta);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.grid.scroll_to_bottom();
    }

    /// Lines per page, for page-scroll bindings.
    pub fn page(&self) -> usize {
        self.grid.page()
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
                (true, 1) => self.grid.set_app_cursor(on),
                (true, 25) => self.grid.set_cursor_visible(on),
                (true, 9) => self.grid.set_mouse_protocol(proto(on, MouseProtocol::X10)),
                (true, 1000) => self
                    .grid
                    .set_mouse_protocol(proto(on, MouseProtocol::Normal)),
                (true, 1002) => self
                    .grid
                    .set_mouse_protocol(proto(on, MouseProtocol::Button)),
                (true, 1003) => self.grid.set_mouse_protocol(proto(on, MouseProtocol::Any)),
                (true, 1004) => self.grid.set_focus_events(on),
                (true, 1005) => self.grid.set_mouse_encoding(enc(on, MouseEncoding::Utf8)),
                (true, 1006) => self.grid.set_mouse_encoding(enc(on, MouseEncoding::Sgr)),
                (true, 2004) => self.grid.set_bracketed_paste(on),
                (true, 2026) => self.grid.set_sync(on),
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
                4 => pen.underline = underline_from(p),
                5 | 6 => pen.flags.insert(Flags::BLINK),
                7 => pen.flags.insert(Flags::REVERSE),
                8 => pen.flags.insert(Flags::HIDDEN),
                9 => pen.flags.insert(Flags::STRIKE),
                21 => pen.underline = Underline::Double,
                22 => pen.flags.remove(Flags::BOLD.union(Flags::DIM)),
                23 => pen.flags.remove(Flags::ITALIC),
                24 => pen.underline = Underline::None,
                25 => pen.flags.remove(Flags::BLINK),
                27 => pen.flags.remove(Flags::REVERSE),
                28 => pen.flags.remove(Flags::HIDDEN),
                29 => pen.flags.remove(Flags::STRIKE),
                30..=37 => pen.fg = Color::Indexed((code - 30) as u8),
                39 => pen.fg = Color::Default,
                40..=47 => pen.bg = Color::Indexed((code - 40) as u8),
                49 => pen.bg = Color::Default,
                53 => pen.flags.insert(Flags::OVERLINE),
                55 => pen.flags.remove(Flags::OVERLINE),
                90..=97 => pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => pen.bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let (color, consumed) = ext_color(&items, i);
                    if let Some(color) = color {
                        match code {
                            38 => pen.fg = color,
                            48 => pen.bg = color,
                            _ => pen.underline_color = color,
                        }
                    }
                    step = consumed;
                }
                59 => pen.underline_color = Color::Default,
                _ => {}
            }
            i += step;
        }
    }

    /// Device attributes. DA1 claims a VT220 with ANSI colour; DA2 a generic
    /// firmware level; DA3 a (zero) unit ID.
    fn device_attrs(&mut self, level: DaLevel) {
        match level {
            DaLevel::Primary => self.response.extend_from_slice(b"\x1b[?62;22c"),
            DaLevel::Secondary => self.response.extend_from_slice(b"\x1b[>0;276;0c"),
            DaLevel::Tertiary => self.response.extend_from_slice(b"\x1bP!|00000000\x1b\\"),
        }
    }

    /// Reply to a kitty-keyboard flags query (`CSI ? u`) with `CSI ? flags u`.
    fn report_kitty_flags(&mut self) {
        let _ = write!(self.response, "\x1b[?{}u", self.grid.kitty_flags());
    }

    /// XTVERSION (`CSI > q`): report the terminal name and version.
    fn report_version(&mut self) {
        let _ = write!(
            self.response,
            "\x1bP>|beer({})\x1b\\",
            env!("CARGO_PKG_VERSION")
        );
    }

    /// Emit an OSC colour reply (`OSC code ; rgb:rrrr/gggg/bbbb` + terminator).
    fn reply_color(&mut self, code: &str, rgb: Rgb, bell: bool) {
        let Rgb(r, g, b) = rgb;
        let _ = write!(
            self.response,
            "\x1b]{code};rgb:{:04x}/{:04x}/{:04x}",
            u16::from(r) * 0x101,
            u16::from(g) * 0x101,
            u16::from(b) * 0x101,
        );
        self.response
            .extend_from_slice(if bell { b"\x07" } else { b"\x1b\\" });
    }

    /// OSC 4: set or query palette entries, given as `index;spec` pairs.
    fn osc_palette(&mut self, params: &[&[u8]], bell: bool) {
        let mut rest = params[1..].iter();
        while let (Some(idx_raw), Some(spec)) = (rest.next(), rest.next()) {
            let Some(idx) = parse_index(idx_raw) else {
                continue;
            };
            if *spec == b"?" {
                let rgb = self.theme.palette[idx as usize];
                self.reply_color(&format!("4;{idx}"), rgb, bell);
            } else if let Some(rgb) = parse_spec(spec) {
                self.theme.set_palette(idx, rgb);
            }
        }
    }

    /// OSC 10/11/17/19: set or query a dynamic colour.
    fn osc_dynamic_color(&mut self, kind: Dynamic, spec: Option<&&[u8]>, bell: bool) {
        let Some(spec) = spec else { return };
        if **spec == b"?"[..] {
            let rgb = match kind {
                Dynamic::Fg => self.theme.fg,
                Dynamic::Bg => self.theme.bg,
                Dynamic::SelBg => self.theme.selection_bg,
                Dynamic::SelFg => self.theme.selection_fg.unwrap_or(self.theme.fg),
            };
            let code = match kind {
                Dynamic::Fg => "10",
                Dynamic::Bg => "11",
                Dynamic::SelBg => "17",
                Dynamic::SelFg => "19",
            };
            self.reply_color(code, rgb, bell);
        } else if let Some(rgb) = parse_spec(spec) {
            match kind {
                Dynamic::Fg => self.theme.fg = rgb,
                Dynamic::Bg => self.theme.bg = rgb,
                Dynamic::SelBg => self.theme.selection_bg = rgb,
                Dynamic::SelFg => self.theme.selection_fg = Some(rgb),
            }
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

    /// DECRQM (`CSI [?] Ps $ p`): report whether a mode is set (1), reset (2),
    /// or unrecognized (0). Only the modes we actually track are reported.
    fn report_mode(&mut self, params: &Params, private: bool) {
        let code = raw(params, 0);
        let state = match (private, code) {
            (true, 6) => set_reset(self.grid.origin()),
            (true, 7) => set_reset(self.grid.autowrap()),
            (true, 47 | 1047 | 1049) => set_reset(self.grid.alt_active()),
            (true, 9) => set_reset(self.grid.mouse_protocol() == MouseProtocol::X10),
            (true, 1000) => set_reset(self.grid.mouse_protocol() == MouseProtocol::Normal),
            (true, 1002) => set_reset(self.grid.mouse_protocol() == MouseProtocol::Button),
            (true, 1003) => set_reset(self.grid.mouse_protocol() == MouseProtocol::Any),
            (true, 1004) => set_reset(self.grid.focus_events()),
            (true, 1005) => set_reset(self.grid.mouse_encoding() == MouseEncoding::Utf8),
            (true, 1006) => set_reset(self.grid.mouse_encoding() == MouseEncoding::Sgr),
            (true, 2004) => set_reset(self.grid.bracketed_paste()),
            (true, 2026) => set_reset(self.grid.sync_active()),
            (false, 4) => set_reset(self.grid.insert()),
            _ => 0,
        };
        let prefix = if private { "?" } else { "" };
        let _ = write!(self.response, "\x1b[{prefix}{code};{state}$y");
    }

    /// Title stack (`CSI 22/23 ; Ps t`): push or pop the window title.
    fn title_stack_op(&mut self, params: &Params) {
        match raw(params, 0) {
            22 => self.title_stack.push(self.title.clone()),
            23 => {
                if let Some(title) = self.title_stack.pop() {
                    self.title = title;
                }
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

/// Look up a terminfo capability beer reports via XTGETTCAP.
fn cap_value(name: &[u8]) -> Option<&'static str> {
    match name {
        b"TN" => Some("beer"),
        b"Co" | b"colors" => Some("256"),
        b"RGB" => Some("8/8/8"),
        _ => None,
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 encode (used for OSC 52 query replies).
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Standard base64 decode, ignoring padding and whitespace; `None` on a bad
/// character.
fn base64_decode(data: &[u8]) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let filtered: Vec<u8> = data
        .iter()
        .copied()
        .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(filtered.len() / 4 * 3);
    for chunk in filtered.chunks(4) {
        if chunk.len() == 1 {
            return None; // a lone sextet cannot form a byte
        }
        let mut n = 0u32;
        for &c in chunk {
            n = (n << 6) | val(c)?;
        }
        n <<= 6 * (4 - chunk.len() as u32);
        out.push((n >> 16) as u8);
        if chunk.len() >= 3 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() >= 4 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Decode an OSC string field to UTF-8 (lossy), or `None` if absent.
fn osc_text(field: Option<&&[u8]>) -> Option<String> {
    field.map(|b| String::from_utf8_lossy(b).into_owned())
}

/// Map an OSC 133 mark letter to a [`PromptKind`].
fn prompt_kind(b: u8) -> Option<PromptKind> {
    match b {
        b'A' => Some(PromptKind::PromptStart),
        b'B' => Some(PromptKind::CmdStart),
        b'C' => Some(PromptKind::OutputStart),
        b'D' => Some(PromptKind::CmdEnd),
        _ => None,
    }
}

/// Extract the local path from an OSC 7 `file://host/path` URI, percent-decoding
/// `%XX` escapes. The host part is ignored (we only spawn locally). Returns
/// `None` if it is not a usable absolute path.
fn file_uri_path(uri: &[u8]) -> Option<String> {
    let rest = uri.strip_prefix(b"file://").unwrap_or(uri);
    // Skip the authority (host) up to the first '/', which begins the path.
    let slash = rest.iter().position(|&b| b == b'/')?;
    let path_bytes = percent_decode(&rest[slash..]);
    let path = String::from_utf8(path_bytes).ok()?;
    path.starts_with('/').then_some(path)
}

/// Percent-decode `%XX` byte escapes in a URI path, passing other bytes through.
fn percent_decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' && i + 2 < s.len() {
            let hi = (s[i + 1] as char).to_digit(16);
            let lo = (s[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// Decode an even-length lowercase/uppercase hex string into bytes.
fn decode_hex(s: &[u8]) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    s.chunks_exact(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{MouseEncoding, MouseProtocol};

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
    fn device_attributes_levels() {
        let mut t = Term::new(20, 4);
        feed(&mut t, b"\x1b[c");
        assert_eq!(t.take_response(), b"\x1b[?62;22c");
        feed(&mut t, b"\x1b[>c");
        assert_eq!(t.take_response(), b"\x1b[>0;276;0c");
        feed(&mut t, b"\x1b[=c");
        assert_eq!(t.take_response(), b"\x1bP!|00000000\x1b\\");
    }

    #[test]
    fn xtversion_reports_name() {
        let mut t = Term::new(20, 4);
        feed(&mut t, b"\x1b[>q");
        let resp = t.take_response();
        assert!(resp.starts_with(b"\x1bP>|beer("));
        assert!(resp.ends_with(b")\x1b\\"));
    }

    #[test]
    fn decrqm_reports_known_modes() {
        let mut t = Term::new(20, 4);
        feed(&mut t, b"\x1b[?7$p"); // autowrap, on by default
        assert_eq!(t.take_response(), b"\x1b[?7;1$y");
        feed(&mut t, b"\x1b[?7l\x1b[?7$p"); // turn it off, re-query
        assert_eq!(t.take_response(), b"\x1b[?7;2$y");
        feed(&mut t, b"\x1b[?9999$p"); // unknown mode
        assert_eq!(t.take_response(), b"\x1b[?9999;0$y");
    }

    #[test]
    fn sgr_underline_styles_and_lines() {
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1b[4:3;58;5;1;53mX");
        let cell = t.grid().cell(0, 0);
        assert_eq!(cell.underline, Underline::Curly);
        assert_eq!(cell.underline_color, Color::Indexed(1));
        assert!(cell.flags.contains(Flags::OVERLINE));
        // 4:0 turns the underline back off.
        feed(&mut t, b"\x1b[4:0mY");
        assert_eq!(t.grid().cell(1, 0).underline, Underline::None);
    }

    #[test]
    fn decscusr_and_cursor_visibility() {
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1b[4 q");
        assert_eq!(t.grid().cursor_shape(), CursorShape::Underline);
        feed(&mut t, b"\x1b[6 q");
        assert_eq!(t.grid().cursor_shape(), CursorShape::Beam);
        feed(&mut t, b"\x1b[0 q");
        assert_eq!(t.grid().cursor_shape(), CursorShape::Block);

        feed(&mut t, b"\x1b[?25l");
        assert!(!t.grid().cursor_visible());
        feed(&mut t, b"\x1b[?25h");
        assert!(t.grid().cursor_visible());
    }

    #[test]
    fn osc12_sets_and_resets_cursor_color() {
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1b]12;#ff0000\x07");
        assert_eq!(t.grid().cursor_color(), Some((255, 0, 0)));
        feed(&mut t, b"\x1b]12;rgb:00/80/ff\x07");
        assert_eq!(t.grid().cursor_color(), Some((0, 0x80, 0xff)));
        feed(&mut t, b"\x1b]112\x07");
        assert_eq!(t.grid().cursor_color(), None);
    }

    #[test]
    fn decscusr_and_cursor_color() {
        use crate::grid::CursorShape;
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1b[5 q"); // blinking bar
        assert_eq!(t.grid().cursor_shape(), CursorShape::Beam);
        feed(&mut t, b"\x1b[4 q"); // steady underline
        assert_eq!(t.grid().cursor_shape(), CursorShape::Underline);
        feed(&mut t, b"\x1b]12;#ff3030\x07");
        assert_eq!(t.grid().cursor_color(), Some((0xff, 0x30, 0x30)));
        feed(&mut t, b"\x1b]112\x07");
        assert_eq!(t.grid().cursor_color(), None);
        feed(&mut t, b"\x1b[?25l"); // hide cursor
        assert!(!t.grid().cursor_visible());
    }

    #[test]
    fn osc_palette_and_dynamic_colors() {
        use crate::theme::Rgb;
        let mut t = Term::new(20, 2);
        // Set palette index 1 and foreground via OSC, then query them back.
        feed(&mut t, b"\x1b]4;1;#ff0000\x1b\\");
        assert_eq!(t.theme().palette[1], Rgb(0xff, 0, 0));
        feed(&mut t, b"\x1b]10;rgb:00/80/ff\x1b\\");
        assert_eq!(t.theme().fg, Rgb(0, 0x80, 0xff));
        feed(&mut t, b"\x1b]11;?\x07");
        let resp = t.take_response();
        assert!(resp.starts_with(b"\x1b]11;rgb:"));
        // Reset returns the palette entry to its default.
        feed(&mut t, b"\x1b]104;1\x1b\\");
        assert_ne!(t.theme().palette[1], Rgb(0xff, 0, 0));
    }

    #[test]
    fn xtgettcap_known_and_unknown() {
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1bP+q544e\x1b\\"); // "TN"
        assert_eq!(t.take_response(), b"\x1bP1+r544e=62656572\x1b\\"); // = "beer"
        feed(&mut t, b"\x1bP+q6162\x1b\\"); // "ab", unknown
        assert_eq!(t.take_response(), b"\x1bP0+r6162\x1b\\");
    }

    #[test]
    fn bracketed_paste_and_sync_modes() {
        let mut t = Term::new(20, 2);
        feed(&mut t, b"\x1b[?2004h");
        assert!(t.grid().bracketed_paste());
        feed(&mut t, b"\x1b[?2004$p");
        assert_eq!(t.take_response(), b"\x1b[?2004;1$y");
        feed(&mut t, b"\x1b[?2026h");
        assert!(t.grid().sync_active());
        feed(&mut t, b"\x1b[?2026l\x1b[?2026$p");
        assert!(!t.grid().sync_active());
        assert_eq!(t.take_response(), b"\x1b[?2026;2$y");
    }

    #[test]
    fn base64_round_trips() {
        for s in [
            "",
            "f",
            "fo",
            "foo",
            "foob",
            "fooba",
            "foobar",
            "hi there\n",
        ] {
            let enc = base64_encode(s.as_bytes());
            assert_eq!(base64_decode(enc.as_bytes()).as_deref(), Some(s.as_bytes()));
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_decode(b"Zm9vYmFy").as_deref(), Some(&b"foobar"[..]));
    }

    #[test]
    fn osc52_set_and_query() {
        let mut t = Term::new(20, 2);
        // Set clipboard to "hi" (base64 "aGk=").
        feed(&mut t, b"\x1b]52;c;aGk=\x07");
        let ops = t.take_clipboard_ops();
        match ops.as_slice() {
            [
                ClipboardOp::Set {
                    primary: false,
                    text,
                },
            ] => assert_eq!(text, "hi"),
            other => panic!("unexpected ops: {other:?}"),
        }
        // Query the primary selection.
        feed(&mut t, b"\x1b]52;p;?\x07");
        let ops = t.take_clipboard_ops();
        assert!(matches!(
            ops.as_slice(),
            [ClipboardOp::Query { primary: true }]
        ));
    }

    #[test]
    fn mouse_modes_track_protocol_and_encoding() {
        let mut t = Term::new(20, 4);
        feed(&mut t, b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(t.grid().mouse_protocol(), MouseProtocol::Button);
        assert_eq!(t.grid().mouse_encoding(), MouseEncoding::Sgr);
        feed(&mut t, b"\x1b[?1002$p");
        assert_eq!(t.take_response(), b"\x1b[?1002;1$y");
        feed(&mut t, b"\x1b[?1003h"); // any-event supersedes button-event
        assert_eq!(t.grid().mouse_protocol(), MouseProtocol::Any);
        feed(&mut t, b"\x1b[?1000l"); // turning a mouse mode off clears reporting
        assert_eq!(t.grid().mouse_protocol(), MouseProtocol::Off);
        feed(&mut t, b"\x1b[?1004h");
        assert!(t.grid().focus_events());
    }

    #[test]
    fn title_stack_push_pop() {
        let mut t = Term::new(20, 4);
        feed(&mut t, b"\x1b]0;first\x07");
        feed(&mut t, b"\x1b[22t"); // push "first"
        feed(&mut t, b"\x1b]0;second\x07");
        assert_eq!(t.title(), Some("second"));
        feed(&mut t, b"\x1b[23t"); // pop -> "first"
        assert_eq!(t.title(), Some("first"));
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
    fn osc133_marks_capture_last_command_output() {
        let mut t = Term::new(12, 6);
        feed(&mut t, b"\x1b]133;A\x07$ echo hi\r\n"); // prompt + typed command
        feed(&mut t, b"\x1b]133;C\x07hi\r\n"); // output start, then output
        feed(&mut t, b"\x1b]133;D\x07"); // command finished
        assert_eq!(t.grid().last_command_output().as_deref(), Some("hi\n"));
    }

    #[test]
    fn osc_notifications_collected() {
        let mut t = Term::new(20, 2);
        feed(&mut t, b"\x1b]9;hello\x07");
        feed(&mut t, b"\x1b]777;notify;Title;Body\x07");
        let n = t.take_notifications();
        assert_eq!(
            n,
            vec![
                Notification {
                    title: None,
                    body: "hello".into()
                },
                Notification {
                    title: Some("Title".into()),
                    body: "Body".into()
                },
            ]
        );
        assert!(t.take_notifications().is_empty());
    }

    #[test]
    fn osc7_tracks_cwd_and_decodes_percent() {
        let mut t = Term::new(20, 1);
        feed(&mut t, b"\x1b]7;file://hermes/home/user/my%20dir\x07");
        assert_eq!(t.cwd(), Some("/home/user/my dir"));
        // A non-file or relative URI leaves the previous value untouched? It
        // simply does not match a path, so cwd stays None here.
        let mut t2 = Term::new(20, 1);
        feed(&mut t2, b"\x1b]7;file://host\x07");
        assert_eq!(t2.cwd(), None);
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
