//! User configuration: a TOML file at `$XDG_CONFIG_HOME/beer/beer.toml`
//! deserialized into a typed [`Config`]. A missing file uses defaults; a
//! malformed one warns and falls back to defaults rather than failing to start.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level configuration. Unknown keys are ignored so a config written for a
/// newer beer still loads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    pub main: Main,
    pub colors: Colors,
    pub cursor: Cursor,
    pub scrollback: Scrollback,
    pub bell: Bell,
    pub mouse: Mouse,
    pub shell_integration: ShellIntegration,
    pub url: Url,
    /// Chord → action, e.g. `"Ctrl+Shift+C" = "copy"`. Merged over the defaults;
    /// a value of `"none"` unbinds.
    pub key_bindings: std::collections::HashMap<String, String>,
    /// Chord → literal text to send (supports `\e \n \r \t \\ \xNN`).
    pub text_bindings: std::collections::HashMap<String, String>,
}

/// `[cursor]`: the default cursor presentation (DECSCUSR may override at runtime).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Cursor {
    /// `block`, `beam`/`bar`, or `underline`.
    pub style: Option<String>,
    /// Whether the cursor blinks by default.
    pub blink: bool,
}

/// `[bell]`: what happens on `BEL` (0x07).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Bell {
    /// Briefly flash the screen.
    pub visual: bool,
}

/// `[mouse]`: pointer and wheel behaviour.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Mouse {
    /// Multiplier applied to the lines scrolled per wheel notch.
    pub scroll_multiplier: f64,
    /// On the alternate screen, translate the wheel into arrow-key presses so
    /// full-screen apps that did not request mouse reporting (less, man, …)
    /// still scroll.
    pub alternate_scroll: bool,
}

impl Default for Mouse {
    fn default() -> Self {
        Self {
            scroll_multiplier: 1.0,
            alternate_scroll: true,
        }
    }
}

/// `[shell-integration]`: behaviour driven by OSC 7 / OSC 133 marks.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ShellIntegration {
    /// Command the `pipe-command-output` binding feeds the last command's
    /// output to on stdin (argv form, e.g. `["less"]`). Empty disables it.
    pub pipe_command: Vec<String>,
}

/// `[url]`: opening OSC 8 hyperlinks and detected URLs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Url {
    /// Launcher argv the URL is appended to (e.g. `["xdg-open"]`).
    pub launch: Vec<String>,
}

impl Default for Url {
    fn default() -> Self {
        Self {
            launch: vec!["xdg-open".to_string()],
        }
    }
}

/// `[colors]`: foreground/background, the 16 base palette entries, and accents.
/// Each value is an X11 colour spec (`#rrggbb` or `rgb:rr/gg/bb`); unset entries
/// keep the built-in default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Colors {
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub cursor: Option<String>,
    pub selection_foreground: Option<String>,
    pub selection_background: Option<String>,
    /// The eight regular palette entries (indices 0-7).
    pub regular: Option<Vec<String>>,
    /// The eight bright palette entries (indices 8-15).
    pub bright: Option<Vec<String>>,
    pub match_background: Option<String>,
    pub match_current_background: Option<String>,
    /// Background opacity, 0.0 (transparent) - 1.0 (opaque).
    pub alpha: Option<f32>,
    /// Render bold text with the bright palette variant.
    pub bold_as_bright: Option<bool>,
}

/// `[main]`: fonts, window geometry, padding, and the terminal name.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Main {
    /// Primary font family, resolved via fontconfig.
    pub font: String,
    /// Font size in pixels.
    pub font_size: u32,
    /// `TERM` value exported to the child shell.
    pub term: String,
    /// Initial size in character cells.
    pub initial_cols: u16,
    pub initial_rows: u16,
    /// Inner padding in pixels between the window edge and the cell grid.
    pub pad_x: u32,
    pub pad_y: u32,
    /// Characters that break a word for double-click selection. Empty/unset
    /// keeps the built-in default.
    pub word_delimiters: Option<String>,
}

impl Default for Main {
    fn default() -> Self {
        Self {
            font: "monospace".to_string(),
            font_size: 16,
            term: "beer".to_string(),
            initial_cols: 80,
            initial_rows: 24,
            pad_x: 2,
            pad_y: 2,
            word_delimiters: None,
        }
    }
}

/// `[scrollback]`: history retention.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Scrollback {
    /// Lines of history retained for the main screen.
    pub lines: usize,
}

impl Default for Scrollback {
    fn default() -> Self {
        Self { lines: 10_000 }
    }
}

impl Config {
    /// Load configuration from `explicit` if given, else the default path.
    /// Any read/parse failure logs a warning and returns defaults.
    pub fn load(explicit: Option<&Path>) -> Self {
        let Some(path) = explicit.map(Path::to_path_buf).or_else(default_path) else {
            return Self::default();
        };
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(config) => {
                    tracing::info!("loaded config from {}", path.display());
                    config
                }
                Err(err) => {
                    tracing::warn!("config {}: {err}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(err) => {
                tracing::warn!("read config {}: {err}; using defaults", path.display());
                Self::default()
            }
        }
    }
}

/// `$XDG_CONFIG_HOME/beer/beer.toml`, or `~/.config/beer/beer.toml`.
fn default_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(dir).join("beer/beer.toml"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/beer/beer.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.main.font, "monospace");
        assert_eq!(c.main.font_size, 16);
        assert_eq!(c.scrollback.lines, 10_000);
    }

    #[test]
    fn parses_partial_config_and_ignores_unknown() {
        let toml = r##"
            [main]
            font = "JetBrains Mono"
            font-size = 14
            unknown-key = "tolerated"

            [colors]
            background = "#000000"
        "##;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.main.font, "JetBrains Mono");
        assert_eq!(c.main.font_size, 14);
        // Unset keys keep defaults; unknown tables/keys are ignored.
        assert_eq!(c.main.term, "beer");
        assert_eq!(c.scrollback.lines, 10_000);
    }
}
