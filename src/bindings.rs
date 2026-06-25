//! Configurable key bindings: chord strings (e.g. `Ctrl+Shift+C`) mapped to
//! editor actions, plus text bindings that send a literal string.

use std::collections::HashMap;

use smithay_client_toolkit::seat::keyboard::{KeyEvent, Keysym, Modifiers};

/// An action a key binding can trigger.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Copy,
    Paste,
    PastePrimary,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    SearchStart,
    FontIncrease,
    FontDecrease,
    FontReset,
    Fullscreen,
    NewWindow,
    JumpPromptUp,
    JumpPromptDown,
    PipeCommandOutput,
    UrlMode,
}

impl Action {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "copy" => Self::Copy,
            "paste" => Self::Paste,
            "paste-primary" => Self::PastePrimary,
            "scrollback-up" => Self::ScrollPageUp,
            "scrollback-down" => Self::ScrollPageDown,
            "scrollback-top" => Self::ScrollTop,
            "scrollback-bottom" => Self::ScrollBottom,
            "search" => Self::SearchStart,
            "font-increase" => Self::FontIncrease,
            "font-decrease" => Self::FontDecrease,
            "font-reset" => Self::FontReset,
            "fullscreen" => Self::Fullscreen,
            "new-window" => Self::NewWindow,
            "jump-prompt-up" => Self::JumpPromptUp,
            "jump-prompt-down" => Self::JumpPromptDown,
            "pipe-command-output" => Self::PipeCommandOutput,
            "url-mode" => Self::UrlMode,
            _ => return None,
        })
    }
}

/// A parsed chord: a key plus the modifiers that must be held exactly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Chord {
    key: Keysym,
    ctrl: bool,
    shift: bool,
    alt: bool,
    logo: bool,
}

impl Chord {
    /// Parse a chord like `Ctrl+Shift+C`. Modifier order is irrelevant; the
    /// final token is the key. Returns `None` if no key is recognized.
    fn parse(spec: &str) -> Option<Self> {
        let (mut ctrl, mut shift, mut alt, mut logo) = (false, false, false, false);
        let mut key = None;
        for token in spec.split('+').map(str::trim).filter(|t| !t.is_empty()) {
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" | "mod1" | "meta" => alt = true,
                "super" | "logo" | "cmd" | "mod4" => logo = true,
                _ => key = keysym_from_token(token),
            }
        }
        Some(Self {
            key: key?,
            ctrl,
            shift,
            alt,
            logo,
        })
    }

    /// Whether `event`/`mods` match this chord. Letters compare case-insensitively
    /// (Shift is matched via the modifier, not the keysym case).
    fn matches(&self, event: &KeyEvent, mods: Modifiers) -> bool {
        if mods.ctrl != self.ctrl
            || mods.shift != self.shift
            || mods.alt != self.alt
            || mods.logo != self.logo
        {
            return false;
        }
        event.keysym == self.key
            || matches!(
                (self.key.key_char(), event.keysym.key_char()),
                (Some(a), Some(b)) if a.eq_ignore_ascii_case(&b)
            )
    }
}

/// The resolved binding tables for a session.
#[derive(Clone, Debug, Default)]
pub struct Bindings {
    keys: Vec<(Chord, Action)>,
    text: Vec<(Chord, Vec<u8>)>,
}

impl Bindings {
    /// Build the tables from config, starting from the built-in defaults and
    /// applying user overrides (a value of `none` unbinds a default chord).
    pub fn from_config(
        key_bindings: &HashMap<String, String>,
        text_bindings: &HashMap<String, String>,
    ) -> Self {
        let mut keys: Vec<(Chord, Action)> = Vec::new();
        let mut add = |chord: &str, action: Action| {
            if let Some(c) = Chord::parse(chord) {
                keys.retain(|(existing, _)| *existing != c);
                keys.push((c, action));
            }
        };
        for (chord, action) in DEFAULT_BINDINGS {
            if let Some(a) = Action::parse(action) {
                add(chord, a);
            }
        }
        for (chord, action) in key_bindings {
            if let Some(c) = Chord::parse(chord) {
                keys.retain(|(existing, _)| *existing != c);
                if let Some(a) = Action::parse(action) {
                    keys.push((c, a));
                } else if action != "none" {
                    tracing::warn!("unknown key-binding action {action:?}");
                }
            }
        }

        let mut text = Vec::new();
        for (chord, value) in text_bindings {
            if let Some(c) = Chord::parse(chord) {
                text.push((c, unescape(value)));
            }
        }
        Self { keys, text }
    }

    /// The action bound to this key event, if any.
    pub fn action(&self, event: &KeyEvent, mods: Modifiers) -> Option<Action> {
        self.keys
            .iter()
            .find(|(c, _)| c.matches(event, mods))
            .map(|(_, a)| *a)
    }

    /// The literal bytes bound to this key event, if any.
    pub fn text(&self, event: &KeyEvent, mods: Modifiers) -> Option<&[u8]> {
        self.text
            .iter()
            .find(|(c, _)| c.matches(event, mods))
            .map(|(_, t)| t.as_slice())
    }
}

/// Built-in default key bindings (chord, action name).
const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("Ctrl+Shift+C", "copy"),
    ("Ctrl+Shift+V", "paste"),
    ("Ctrl+Shift+F", "search"),
    ("Shift+Page_Up", "scrollback-up"),
    ("Shift+Page_Down", "scrollback-down"),
    ("Ctrl+Shift+Home", "scrollback-top"),
    ("Ctrl+Shift+End", "scrollback-bottom"),
    ("Ctrl+plus", "font-increase"),
    ("Ctrl+equal", "font-increase"),
    ("Ctrl+minus", "font-decrease"),
    ("Ctrl+0", "font-reset"),
    ("F11", "fullscreen"),
    ("Ctrl+Shift+N", "new-window"),
    ("Ctrl+Shift+Up", "jump-prompt-up"),
    ("Ctrl+Shift+Down", "jump-prompt-down"),
    ("Ctrl+Shift+O", "url-mode"),
];

/// Map a key token to a keysym: a single character, or a named special key.
fn keysym_from_token(token: &str) -> Option<Keysym> {
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        // A single character; bind case-insensitively via the lowercase keysym.
        return Some(Keysym::from_char(c.to_ascii_lowercase()));
    }
    Some(match token {
        "Return" | "Enter" => Keysym::Return,
        "Tab" => Keysym::Tab,
        "Escape" | "Esc" => Keysym::Escape,
        "Space" => Keysym::space,
        "BackSpace" => Keysym::BackSpace,
        "Delete" => Keysym::Delete,
        "Insert" => Keysym::Insert,
        "Home" => Keysym::Home,
        "End" => Keysym::End,
        "Page_Up" | "PageUp" | "Prior" => Keysym::Page_Up,
        "Page_Down" | "PageDown" | "Next" => Keysym::Page_Down,
        "Up" => Keysym::Up,
        "Down" => Keysym::Down,
        "Left" => Keysym::Left,
        "Right" => Keysym::Right,
        "plus" => Keysym::plus,
        "minus" => Keysym::minus,
        "equal" => Keysym::equal,
        "F1" => Keysym::F1,
        "F2" => Keysym::F2,
        "F3" => Keysym::F3,
        "F4" => Keysym::F4,
        "F5" => Keysym::F5,
        "F6" => Keysym::F6,
        "F7" => Keysym::F7,
        "F8" => Keysym::F8,
        "F9" => Keysym::F9,
        "F10" => Keysym::F10,
        "F11" => Keysym::F11,
        "F12" => Keysym::F12,
        other => {
            tracing::warn!("unknown key name {other:?} in binding");
            return None;
        }
    })
}

/// Decode the escapes a text binding may contain: `\e \n \r \t \\` and `\xNN`.
fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('e') => out.push(0x1b),
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = (0..2).filter_map(|_| chars.next()).collect();
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    out.push(byte);
                }
            }
            Some(other) => {
                out.push(b'\\');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => out.push(b'\\'),
        }
    }
    out
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

    fn key(keysym: Keysym) -> KeyEvent {
        KeyEvent {
            time: 0,
            raw_code: 0,
            keysym,
            utf8: None,
        }
    }

    #[test]
    fn default_copy_binding_matches_case_insensitively() {
        let b = Bindings::from_config(&HashMap::new(), &HashMap::new());
        let mods = Modifiers {
            ctrl: true,
            shift: true,
            ..NONE
        };
        // Shift yields the uppercase keysym; the binding still matches.
        assert_eq!(b.action(&key(Keysym::C), mods), Some(Action::Copy));
        // Without the right modifiers, no match.
        assert_eq!(b.action(&key(Keysym::C), NONE), None);
    }

    #[test]
    fn config_overrides_and_unbinds() {
        let mut kb = HashMap::new();
        kb.insert("Ctrl+Shift+C".to_string(), "none".to_string());
        kb.insert("Ctrl+y".to_string(), "copy".to_string());
        let b = Bindings::from_config(&kb, &HashMap::new());
        let cs = Modifiers {
            ctrl: true,
            shift: true,
            ..NONE
        };
        assert_eq!(b.action(&key(Keysym::C), cs), None); // unbound
        let c = Modifiers { ctrl: true, ..NONE };
        assert_eq!(b.action(&key(Keysym::y), c), Some(Action::Copy));
    }

    #[test]
    fn text_binding_unescapes() {
        let mut tb = HashMap::new();
        tb.insert("Ctrl+Shift+Return".to_string(), "\\x1b\\r".to_string());
        let b = Bindings::from_config(&HashMap::new(), &tb);
        let mods = Modifiers {
            ctrl: true,
            shift: true,
            ..NONE
        };
        assert_eq!(b.text(&key(Keysym::Return), mods), Some(&[0x1b, b'\r'][..]));
    }
}
