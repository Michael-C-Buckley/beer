//! User configuration: a TOML file at `$XDG_CONFIG_HOME/beer/beer.toml`
//! deserialized into a typed [`Config`]. A missing file uses defaults; a
//! malformed one warns and falls back to defaults rather than failing to start.

use std::{
  collections::HashMap,
  env,
  fs,
  path::{Path, PathBuf},
};

use serde::Deserialize;

/// Top-level configuration. Unknown keys are ignored so a config written for a
/// newer beer still loads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
  pub main:              Main,
  pub colors:            Colors,
  pub cursor:            Cursor,
  pub scrollback:        Scrollback,
  pub bell:              Bell,
  pub mouse:             Mouse,
  pub shell_integration: ShellIntegration,
  pub url:               Url,
  pub notify:            Notify,
  /// Chord → action, e.g. `"Ctrl+Shift+C" = "copy"`. Merged over the defaults;
  /// a value of `"none"` unbinds.
  pub key_bindings:      HashMap<String, String>,
  /// Chord → literal text to send (supports `\e \n \r \t \\ \xNN`).
  pub text_bindings:     HashMap<String, String>,
  /// Mouse chord (e.g. `"Middle"`, `"Shift+Right"`) → action. Merged over the
  /// defaults; `"none"` unbinds. Left-button select/drag stays built in.
  pub mouse_bindings:    HashMap<String, String>,
}

/// `[cursor]`: the default cursor presentation (DECSCUSR may override at
/// runtime).
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
  pub visual:  bool,
  /// Command (argv) to run on the bell, e.g. `["paplay",
  /// "/usr/share/.../bell.oga"]`.
  pub command: Vec<String>,
  /// Request the compositor's attention (xdg-activation) when the bell rings
  /// while the window is unfocused.
  pub urgent:  bool,
}

/// `[notify]`: how desktop notifications (OSC 9/777/99) are delivered.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Notify {
  /// Notifier argv; the title and body are appended as the last two arguments.
  pub command: Vec<String>,
}

impl Default for Notify {
  fn default() -> Self {
    Self {
      command: vec!["notify-send".to_string()],
    }
  }
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
  pub alternate_scroll:  bool,
}

impl Default for Mouse {
  fn default() -> Self {
    Self {
      scroll_multiplier: 1.0,
      alternate_scroll:  true,
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

/// How glyph coverage is composited over the background.
///
/// `Native` blends in the stored sRGB space (fast, matches Foot's default).
/// `Linear` blends in linear light (gamma-correct, but thin text looks
/// lighter). `LinearCorrected` blends in linear light but remaps the coverage
/// so the result keeps the perceived weight of `Native` while avoiding the
/// dark colour fringing of `Native` on coloured text - this is what Kitty and
/// Ghostty default to. Subpixel (LCD) glyphs always composite in `Native`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AlphaBlending {
  Native,
  Linear,
  #[default]
  LinearCorrected,
}

/// `[colors]`: foreground/background, the 16 base palette entries, and accents.
/// Each value is an X11 colour spec (`#rrggbb` or `rgb:rr/gg/bb`); unset
/// entries keep the built-in default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Colors {
  pub foreground:               Option<String>,
  pub background:               Option<String>,
  pub cursor:                   Option<String>,
  pub selection_foreground:     Option<String>,
  pub selection_background:     Option<String>,
  /// The eight regular palette entries (indices 0-7).
  pub regular:                  Option<Vec<String>>,
  /// The eight bright palette entries (indices 8-15).
  pub bright:                   Option<Vec<String>>,
  pub match_background:         Option<String>,
  pub match_current_background: Option<String>,
  /// Background opacity, 0.0 (transparent) - 1.0 (opaque).
  pub alpha:                    Option<f32>,
  /// Render bold text with the bright palette variant.
  pub bold_as_bright:           Option<bool>,
  /// How glyph coverage is composited over the background.
  #[serde(default)]
  pub alpha_blending:           AlphaBlending,
}

/// Subpixel (LCD) antialiasing order. `None` renders grayscale coverage, which
/// is the safe default: the physical subpixel order is a property of the panel,
/// so the wrong choice produces colour fringing. `Rgb`/`Bgr` select horizontal
/// LCD rendering for displays with that subpixel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Subpixel {
  #[default]
  None,
  Rgb,
  Bgr,
}

/// Outline grid-fitting strength passed to `FreeType`. Grid-fitting snaps
/// stems to the pixel grid for crisper text at small sizes at the cost of some
/// fidelity to the outline; the levels trade the two off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Hinting {
  /// No grid-fitting; outlines render at their natural sub-pixel positions.
  None,
  /// Light autohinter: vertical grid-fitting only, leaving horizontal metrics
  /// untouched so glyph shapes and spacing stay closer to the design.
  Slight,
  /// Full grid-fitting, the `FreeType` default.
  #[default]
  Normal,
}

/// `[main]`: fonts, window geometry, padding, and the terminal name.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
#[expect(
  clippy::struct_excessive_bools,
  reason = "each field maps to one TOML key"
)]
pub struct Main {
  /// Primary font family, resolved via fontconfig.
  pub font:               String,
  /// Font family for bold text. Unset resolves the primary family's bold
  /// style.
  pub font_bold:          Option<String>,
  /// Font family for italic text. Unset resolves the primary family's italic
  /// style.
  pub font_italic:        Option<String>,
  /// Font family for bold-italic text. Unset resolves the primary family's
  /// bold-italic style.
  pub font_bold_italic:   Option<String>,
  /// Fallback families, tried in order before fontconfig coverage matching for
  /// code points the primary family lacks.
  pub font_fallback:      Vec<String>,
  /// OpenType feature settings applied while shaping. Each entry is a tag with
  /// an optional value: `"ss01"` or `"+ss01"` enables a feature, `"-liga"`
  /// disables it, and `"cv01=2"` sets an explicit value.
  pub font_features:      Vec<String>,
  /// Variation-axis settings for variable fonts, each `"tag=value"`, e.g.
  /// `"wght=550"`. Applied to the primary family and its style variants.
  pub font_variations:    Vec<String>,
  /// Contextual shaping and ligatures. When off, cells render in isolation.
  pub ligatures:          bool,
  /// Font size in pixels.
  pub font_size:          u32,
  /// Outline grid-fitting strength.
  pub hinting:            Hinting,
  /// Pixels added to each cell's advance width; negative tightens the grid.
  pub adjust_cell_width:  i32,
  /// Pixels added to each cell's height; negative tightens line spacing.
  pub adjust_cell_height: i32,
  /// Pixels the text baseline is shifted down within the cell.
  pub adjust_baseline:    i32,
  /// Thicken every glyph by one coverage pixel, a light synthetic weight.
  pub thicken:            bool,
  /// Subpixel (LCD) antialiasing order; `None` keeps grayscale coverage.
  pub subpixel:           Subpixel,
  /// `TERM` value exported to the child shell.
  pub term:               String,
  /// Initial size in character cells.
  pub initial_cols:       u16,
  pub initial_rows:       u16,
  /// Inner padding in pixels between the window edge and the cell grid.
  pub pad_x:              u32,
  pub pad_y:              u32,
  /// Characters that break a word for double-click selection. Empty/unset
  /// keeps the built-in default.
  pub word_delimiters:    Option<String>,
  /// Hold an idle inhibitor while the window is focused, so the compositor
  /// does not blank the screen or start the screensaver. Default off.
  pub idle_inhibit:       bool,
  /// Keep a `--server` process alive after its last window closes. Off makes
  /// the server exit with its last window, like a standalone process.
  pub server_resident:    bool,
}

impl Default for Main {
  fn default() -> Self {
    Self {
      font:               "monospace".to_string(),
      font_bold:          None,
      font_italic:        None,
      font_bold_italic:   None,
      font_fallback:      Vec::new(),
      font_features:      Vec::new(),
      font_variations:    Vec::new(),
      ligatures:          true,
      font_size:          16,
      hinting:            Hinting::Normal,
      adjust_cell_width:  0,
      adjust_cell_height: 0,
      adjust_baseline:    0,
      thicken:            false,
      subpixel:           Subpixel::None,
      term:               "beer".to_string(),
      initial_cols:       80,
      initial_rows:       24,
      pad_x:              2,
      pad_y:              2,
      word_delimiters:    None,
      idle_inhibit:       false,
      server_resident:    true,
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
  /// Load configuration from `paths`, merging later files over earlier ones.
  /// When `paths` is empty, falls back to the default config path.
  /// Any read/parse failure in a file logs a warning and skips that file.
  pub fn load(paths: &[PathBuf]) -> Self {
    let resolved = if paths.is_empty() {
      match default_path().filter(|p| p.exists()) {
        Some(p) => vec![p],
        None => return Self::default(),
      }
    } else {
      paths.to_vec()
    };

    // Read + parse + report-unknown-keys in a single pass per file so each
    // file hits disk only once. Parse errors skip the file; unknown-key
    // warnings are best-effort (deserialization failures in serde_ignored
    // mean only the unknown-key check is lost, not the actual config load).
    let mut merged = toml::Value::Table(toml::Table::new());
    for path in &resolved {
      let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => {
          tracing::warn!("read config {}: {err}; skipping", path.display());
          continue;
        },
      };
      report_unknown_keys(&text, path);
      let value = match toml::from_str::<toml::Value>(&text) {
        Ok(v) => v,
        Err(err) => {
          tracing::warn!("config {}: {err}; skipping", path.display());
          continue;
        },
      };
      deep_merge(&mut merged, &value);
    }

    let serialized = toml::to_string(&merged).unwrap_or_default();
    let config = match toml::from_str(&serialized) {
      Ok(c) => c,
      Err(err) => {
        tracing::warn!("config deserialize: {err}; using defaults");
        return Self::default();
      },
    };
    tracing::info!("loaded config from {} file(s)", resolved.len());
    config
  }
}

/// Deep-merge `overlay` into `base`. Tables are merged recursively; every
/// other value type is replaced outright (last write wins).
fn deep_merge(base: &mut toml::Value, overlay: &toml::Value) {
  match (base, overlay) {
    (toml::Value::Table(base), toml::Value::Table(overlay)) => {
      for (k, v) in overlay {
        match base.get_mut(k) {
          Some(existing) => deep_merge(existing, v),
          None => {
            base.insert(k.clone(), v.clone());
          },
        }
      }
    },
    (base, overlay) => *base = overlay.clone(),
  }
}

/// Parse `text` through `serde_ignored` with `Config` as the target so a typo'd
/// key (`font-sze`) is reported instead of silently dropped, while still
/// loading so unknown keys remain tolerated for forward-compatibility.
fn report_unknown_keys(text: &str, path: &Path) {
  let Ok(de) = toml::Deserializer::parse(text) else {
    return;
  };
  match serde_ignored::deserialize(de, |key| {
    tracing::warn!("config {}: unknown key `{key}` ignored", path.display());
  })
  .map(|_: Config| ())
  {
    Ok(()) => {},
    Err(err) => {
      tracing::warn!(
        "config {}: unknown-key check failed ({err}); typo warnings may be \
         missing",
        path.display(),
      );
    },
  }
}

/// `$XDG_CONFIG_HOME/beer/beer.toml`, or `~/.config/beer/beer.toml`.
fn default_path() -> Option<PathBuf> {
  if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
    return Some(PathBuf::from(dir).join("beer/beer.toml"));
  }
  let home = env::var_os("HOME")?;
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

  #[test]
  fn unknown_keys_are_reported_but_still_load() {
    let toml = r#"
            [main]
            font-sze = 14
            font = "JetBrains Mono"

            [made-up]
            x = 1
        "#;
    let mut unknown = Vec::new();
    let de = toml::Deserializer::parse(toml).unwrap();
    let c: Config =
      serde_ignored::deserialize(de, |key| unknown.push(key.to_string()))
        .unwrap();
    // The typo and the bogus table are both surfaced...
    assert!(unknown.iter().any(|k| k == "main.font-sze"), "{unknown:?}");
    assert!(unknown.iter().any(|k| k == "made-up"), "{unknown:?}");
    // ...yet the valid key still loaded.
    assert_eq!(c.main.font, "JetBrains Mono");
  }

  #[test]
  #[expect(
    clippy::absolute_paths,
    reason = "the test intentionally exercises the host temporary-directory \
              API"
  )]
  fn load_reads_a_file_with_table_headers() {
    // `Config::load` must parse the whole document, not a single TOML value.
    // `str::parse::<toml::Value>()` reads only one value and rejects a leading
    // `[table]` header ("unexpected content").
    let path = std::env::temp_dir()
      .join(format!("beer-config-{}.toml", std::process::id()));
    std::fs::write(
      &path,
      "[main]\nfont = \"JetBrains Mono\"\nfont-size = \
       20\n\n[colors]\nbackground = \"#171717\"\nalpha = 0.8\n",
    )
    .unwrap();
    let c = Config::load(std::slice::from_ref(&path));
    let _ = std::fs::remove_file(&path);
    assert_eq!(c.main.font, "JetBrains Mono");
    assert_eq!(c.main.font_size, 20);
  }
}
