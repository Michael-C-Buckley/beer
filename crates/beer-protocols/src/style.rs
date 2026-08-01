//! The protocol vocabulary: the enums an SGR/DECSET stream selects, shared by
//! the parser, the grid model, and the renderer.

/// A cell colour: terminal default, a palette index, or direct RGB (SGR 30-49,
/// 90-107, and the `38`/`48`/`58` extended forms).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
  #[default]
  /// The terminal's configured default colour.
  Default,
  /// An index into the terminal palette.
  Indexed(u8),
  /// A direct 24-bit red, green, and blue colour.
  Rgb(u8, u8, u8),
}

/// Underline style (SGR 4 / 4:x / 21).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Underline {
  #[default]
  /// No underline.
  None,
  /// A single underline.
  Single,
  /// A double underline.
  Double,
  /// A curly underline.
  Curly,
  /// A dotted underline.
  Dotted,
  /// A dashed underline.
  Dashed,
}

/// Cursor shape (DECSCUSR, `CSI Ps SP q`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorShape {
  #[default]
  /// A block cursor.
  Block,
  /// An underline cursor.
  Underline,
  /// A vertical bar cursor.
  Beam,
}

/// Which mouse events the application has asked to receive (DECSET
/// 9/1000-1003).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseProtocol {
  /// No reporting; the pointer drives local selection/scroll.
  #[default]
  Off,
  /// X10 (9): button presses only.
  X10,
  /// Normal (1000): button press and release.
  Normal,
  /// Button-event (1002): press, release, and motion while a button is held.
  Button,
  /// Any-event (1003): press, release, and all pointer motion.
  Any,
}

/// How mouse events are framed on the wire (default byte form, UTF-8, or SGR).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseEncoding {
  /// Legacy `CSI M Cb Cx Cy`, each value a byte offset by 32 (<= 223).
  #[default]
  X10,
  /// As X10 but coordinates above 95 are UTF-8 encoded (DECSET 1005).
  Utf8,
  /// `CSI < Cb ; Cx ; Cy M/m`, decimal and unbounded (DECSET 1006).
  Sgr,
}

/// Shell-integration prompt mark on a line (OSC 133): the start of a prompt,
/// the start of typed command input, the start of command output, or the line
/// where the command finished.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptKind {
  /// The beginning of a shell prompt.
  PromptStart,
  /// The beginning of typed command input.
  CmdStart,
  /// The beginning of command output.
  OutputStart,
  /// The end of a command.
  CmdEnd,
}

/// Map an OSC 133 mark letter (`A`/`B`/`C`/`D`) to a [`PromptKind`].
#[must_use]
pub const fn prompt_kind(b: u8) -> Option<PromptKind> {
  match b {
    b'A' => Some(PromptKind::PromptStart),
    b'B' => Some(PromptKind::CmdStart),
    b'C' => Some(PromptKind::OutputStart),
    b'D' => Some(PromptKind::CmdEnd),
    _ => None,
  }
}
