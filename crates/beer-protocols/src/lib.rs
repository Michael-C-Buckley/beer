//! Terminal protocol building blocks for [beer](https://github.com/NotAShelf/beer).
//!
//! This crate gathers the self-contained pieces of beer's terminal-protocol
//! support: the byte codecs an escape stream needs, the terminfo capability
//! table, character-set translation, SGR colour/underline parsing, and the
//! keyboard/mouse wire encoders. It holds no terminal state (no
//! grid, no parser loop): every item here is a pure function or a plain data
//! type, so each protocol detail can be read and tested on its own. beer wires
//! these into its `vte`-driven dispatcher and grid model.

//! The modules map onto the protocols a terminal user cares about:
//!
//! - [`codec`] - base64 (OSC 52 clipboard), hex (XTGETTCAP), and `file://` URI
//!   percent-decoding (OSC 7).
//! - [`graphics`] - the kitty graphics protocol APC control-data parse.
//! - [`caps`] - the terminfo capabilities answered over XTGETTCAP.
//! - [`charset`] - G0/G1 designation and DEC special-graphics line drawing.
//! - [`sgr`] - the multi-parameter SGR colour and underline forms.
//! - [`key`] - legacy xterm/VT and kitty keyboard-protocol key encoding.
//! - [`mouse`] - X10/UTF-8/SGR mouse-report encoding.
//! - [`text_size`] - the kitty text-sizing protocol (`OSC 66`) metadata.
//! - [`style`] - the enums an SGR/DECSET stream selects (colour, underline,
//!   cursor shape, mouse protocol/encoding, shell-integration prompt marks).
//!
//! See `README.md` for the full inventory of escape sequences, OSC commands,
//! and POSIX behaviour beer implements.

pub mod caps;
pub mod charset;
pub mod codec;
pub mod graphics;
pub mod key;
pub mod mouse;
pub mod sgr;
pub mod style;
pub mod text_size;

pub use style::{
  Color,
  CursorShape,
  MouseEncoding,
  MouseProtocol,
  PromptKind,
  Underline,
  prompt_kind,
};
