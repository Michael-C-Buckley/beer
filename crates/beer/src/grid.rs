//! The terminal screen: a grid of styled cells, a cursor, and the editing
//! operations the VT parser drives.

mod editing;
mod images;
mod layout;
mod links;
mod search;
mod selection;
mod viewport;

use std::{collections::VecDeque, num::NonZeroU16};

use beer_protocols::graphics::{PLACEHOLDER, diacritic_value};
pub use links::UrlHit;
use search::SearchState;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Maximum scrollback lines retained for the main screen.
const SCROLLBACK_CAP: usize = 10_000;

/// The protocol vocabulary an SGR/DECSET stream selects lives in
/// `beer-protocols` and is re-exported here so the grid and renderer keep
/// referring to it as `grid::Color`, `grid::Underline`, and so on.
pub use beer_protocols::text_size::TextSize;
pub use beer_protocols::{
  Color,
  CursorShape,
  MouseEncoding,
  MouseProtocol,
  PromptKind,
  Underline,
};

/// Per-cell style flags, packed into a `u16`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Flags(u16);

impl Flags {
  pub const BOLD: Self = Self(1 << 0);
  pub const DIM: Self = Self(1 << 1);
  pub const ITALIC: Self = Self(1 << 2);
  pub const BLINK: Self = Self(1 << 4);
  pub const REVERSE: Self = Self(1 << 5);
  pub const HIDDEN: Self = Self(1 << 6);
  pub const STRIKE: Self = Self(1 << 7);
  /// Trailing column of a double-width glyph; holds no character of its own.
  pub const WIDE_CONT: Self = Self(1 << 8);
  pub const OVERLINE: Self = Self(1 << 9);
  /// A cell of a text-sizing (`OSC 66`) block that is not the block's leading
  /// cell; it holds no character of its own and is drawn by the leading cell.
  pub const SIZED_CONT: Self = Self(1 << 10);
  /// SGR 6: blink at the rapid cadence rather than the normal SGR 5 cadence.
  pub const RAPID_BLINK: Self = Self(1 << 11);

  pub const fn empty() -> Self {
    Self(0)
  }

  pub const fn union(self, other: Self) -> Self {
    Self(self.0 | other.0)
  }

  pub const fn contains(self, other: Self) -> bool {
    self.0 & other.0 == other.0
  }

  pub const fn insert(&mut self, other: Self) {
    self.0 |= other.0;
  }

  pub const fn remove(&mut self, other: Self) {
    self.0 &= !other.0;
  }
}

/// The text-sizing (`OSC 66`) descriptor a scaled cell carries. A run is drawn
/// as a block `cols` cells wide and `rows` high; this cell sits at `(dx, dy)`
/// within it. The leading cell (`dx == 0, dy == 0`) carries the run text and is
/// what the renderer draws; the rest are flagged [`Flags::SIZED_CONT`].
///
/// Boxed on the cell so the common, unscaled path keeps `Cell` lean: only the
/// rare scaled cell pays an allocation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sized {
  /// Scale, fractional scale, and alignment parsed from the metadata.
  pub size: TextSize,
  /// Block width in cells: `s * w`, or `s * grapheme_width` when `w == 0`.
  pub cols: u8,
  /// Block height in cells: `s`.
  pub rows: u8,
  /// This cell's column within the block.
  pub dx:   u8,
  /// This cell's row within the block.
  pub dy:   u8,
  /// The run text, on the leading cell only. For the per-grapheme (`w == 0`)
  /// form this is `None` and the grapheme lives in the cell's `c`/`combining`;
  /// for the packed (`w > 0`) form it holds the whole run.
  pub run:  Option<Box<str>>,
}

/// A cell's membership in a displayed graphics-protocol image. The image pixels
/// and the placement geometry live in the graphics engine, keyed by
/// `(image, placement)`; the cell records only which placement it belongs to
/// and its `(dx, dy)` position within that placement's cell rectangle, so the
/// renderer can composite the matching slice. Carrying the reference on the
/// cell is what makes images scroll, clear, and erase with the text for free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ImageRef {
  /// Image id in the graphics store.
  pub image:     u32,
  /// Placement id under that image.
  pub placement: u32,
  /// This cell's column within the placement rectangle.
  pub dx:        u16,
  /// This cell's row within the placement rectangle.
  pub dy:        u16,
}

/// An inclusive rectangle of cells for the rectangular-area operations, in
/// origin-relative 0-based coordinates until [`Grid::clamp_rect`] resolves it.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
  pub top:    usize,
  pub left:   usize,
  pub bottom: usize,
  pub right:  usize,
}

/// One grid cell: a character plus its rendering style.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cell {
  pub c:               char,
  pub fg:              Color,
  pub bg:              Color,
  pub flags:           Flags,
  pub underline:       Underline,
  /// Underline colour; `Default` means "follow the foreground".
  pub underline_color: Color,
  /// Zero-width combining marks attached to `c`, in arrival order. `None` for
  /// the common case; the renderer stacks each over the base glyph. This is
  /// the grapheme cluster a future shaper (`HarfBuzz`) would consume.
  pub combining:       Option<Box<str>>,
  /// OSC 8 hyperlink: a 1-based index into the grid's link table, or `None`.
  pub link:            Option<NonZeroU16>,
  /// Text-sizing block membership (`OSC 66`), or `None` for ordinary cells.
  pub sized:           Option<Box<Sized>>,
  /// Graphics-protocol image membership, or `None` for ordinary cells.
  pub image:           Option<ImageRef>,
}

impl Default for Cell {
  fn default() -> Self {
    Self {
      c:               ' ',
      fg:              Color::Default,
      bg:              Color::Default,
      flags:           Flags::empty(),
      underline:       Underline::None,
      underline_color: Color::Default,
      combining:       None,
      link:            None,
      sized:           None,
      image:           None,
    }
  }
}

#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
  x: usize,
  y: usize,
}

/// One screen/scrollback row: its cells plus whether it soft-wrapped into the
/// next row (autowrap continuation, as opposed to a hard line break). The flag
/// is what lets resize rejoin and rewrap paragraphs.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Line {
  cells:   Vec<Cell>,
  wrapped: bool,
  /// OSC 133 mark attached to this (logical) line, if any.
  prompt:  Option<PromptKind>,
}

impl Line {
  fn blank(cols: usize) -> Self {
    Self {
      cells:   vec![Cell::default(); cols],
      wrapped: false,
      prompt:  None,
    }
  }
}

/// A point in the combined scrollback+live coordinate space: `row` indexes
/// scrollback lines first (oldest at 0), then the live screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Point {
  pub row: usize,
  pub col: usize,
}

/// The active screen plus cursor, scroll region, and current pen.
#[expect(
  clippy::struct_excessive_bools,
  reason = "these independent terminal modes are part of the emulated VT state"
)]
#[derive(Debug)]
pub struct Grid {
  cols:            usize,
  rows:            usize,
  lines:           Vec<Line>,
  cursor:          Cursor,
  saved:           Cursor,
  /// Inclusive top/bottom rows of the scroll region.
  top:             usize,
  bottom:          usize,
  /// Left/right scroll margins (DECSLRM). They span the full width unless
  /// `lr_margins` is set; scrolling and line edits are confined to the span.
  left:            usize,
  right:           usize,
  /// Whether left/right margins are enabled (DECLRMM, DECSET `?69`).
  lr_margins:      bool,
  /// Template cell carrying the current SGR colours/flags.
  pen:             Cell,
  autowrap:        bool,
  origin:          bool,
  insert:          bool,
  /// Cursor parked past the last column, awaiting the next print to wrap.
  wrap_pending:    bool,
  tabs:            Vec<bool>,
  /// Saved primary screen while the alternate screen is active.
  alt_saved:       Option<Vec<Line>>,
  /// Lines that have scrolled off the top of the main screen, newest last.
  scrollback:      VecDeque<Line>,
  /// How many lines the viewport is scrolled back from the live bottom.
  view_offset:     usize,
  cursor_shape:    CursorShape,
  /// Whether the cursor shape is a blinking variant (DECSCUSR odd codes).
  cursor_blink:    bool,
  cursor_visible:  bool,
  /// Application cursor-keys mode (DECCKM): arrows send SS3 instead of CSI.
  app_cursor:      bool,
  /// Application keypad mode (DECKPAM): the numeric keypad sends SS3
  /// sequences.
  app_keypad:      bool,
  /// Cursor colour from OSC 12; `None` follows the cell under the cursor.
  cursor_color:    Option<(u8, u8, u8)>,
  /// Active mouse selection as (anchor, head) in absolute coordinates.
  selection:       Option<(Point, Point)>,
  /// Whether the selection is a rectangular block rather than linear flow.
  selection_block: bool,
  /// Bracketed paste mode (DECSET 2004): wrap pasted text in
  /// `ESC[200~`/`201~`.
  bracketed_paste: bool,
  /// Synchronized output (DECSET 2026): hold presentation while a frame is
  /// being assembled, so the screen never shows a half-drawn update.
  sync:            bool,
  /// Which mouse events the application wants reported.
  mouse_protocol:  MouseProtocol,
  /// Wire framing for those reports.
  mouse_encoding:  MouseEncoding,
  /// Focus in/out reporting (DECSET 1004).
  focus_events:    bool,
  /// Active incremental scrollback search, if any.
  search:          Option<SearchState>,
  /// Characters that break a word for double-click selection.
  word_delimiters: String,
  /// History retention cap for the main screen.
  scrollback_cap:  usize,
  /// Position of the last printed base cell, so a following zero-width
  /// combining mark can attach to it.
  last_base:       Option<(usize, usize)>,
  /// Last printed base character, repeated by REP (`CSI b`).
  last_char:       Option<char>,
  /// OSC 8 hyperlink URIs; a cell's `link` is a 1-based index into this.
  links:           Vec<Box<str>>,
  /// Active kitty-keyboard progressive-enhancement flags (0 = legacy mode).
  kitty_current:   u8,
  /// Saved flag values for the kitty push/pop stack.
  kitty_stack:     Vec<u8>,
}

fn default_tabs(cols: usize) -> Vec<bool> {
  (0..cols).map(|i| i % 8 == 0 && i != 0).collect()
}

fn virtual_image_ref(
  cell: &Cell,
  previous: &mut Option<(u32, u32, u32, Color)>,
) -> Option<ImageRef> {
  if cell.c != PLACEHOLDER {
    *previous = None;
    return None;
  }
  let base = match cell.fg {
    Color::Indexed(index) => u32::from(index),
    Color::Rgb(red, green, blue) => {
      (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
    },
    Color::Default => return None,
  };
  let marks: Vec<char> =
    cell.combining.as_deref().unwrap_or("").chars().collect();
  let values =
    [0, 1, 2].map(|index| marks.get(index).copied().and_then(diacritic_value));
  let same = previous.is_some_and(|state| state.3 == cell.fg);
  let (row, col, high) = placeholder_position(values, *previous, same);
  *previous = Some((row, col, high, cell.fg));
  Some(ImageRef {
    image:     base | (high << 24),
    placement: 0,
    dx:        u16::try_from(col).unwrap_or(u16::MAX),
    dy:        u16::try_from(row).unwrap_or(u16::MAX),
  })
}

fn placeholder_position(
  values: [Option<u32>; 3],
  previous: Option<(u32, u32, u32, Color)>,
  same: bool,
) -> (u32, u32, u32) {
  match (values, previous) {
    ([None, None, None], Some(state)) if same => {
      (state.0, state.1 + 1, state.2)
    },
    ([Some(row), None, None], Some(state)) if same && state.0 == row => {
      (row, state.1 + 1, state.2)
    },
    ([Some(row), Some(col), None], Some(state))
      if same && state.0 == row && state.1 + 1 == col =>
    {
      (row, col, state.2)
    },
    ([row, col, high], _) => {
      (row.unwrap_or(0), col.unwrap_or(0), high.unwrap_or(0))
    },
  }
}

fn line_references_image(line: &Line, image: u32) -> bool {
  let mut previous = None;
  line.cells.iter().any(|cell| {
    cell
      .image
      .or_else(|| virtual_image_ref(cell, &mut previous))
      .is_some_and(|reference| reference.image == image)
  })
}

fn line_image_origin(line: &Line, key: (u32, u32)) -> Option<usize> {
  let mut previous = None;
  line.cells.iter().enumerate().find_map(|(x, cell)| {
    let reference = cell
      .image
      .or_else(|| virtual_image_ref(cell, &mut previous))?;
    (reference.image == key.0
      && reference.placement == key.1
      && reference.dx == 0
      && reference.dy == 0)
      .then_some(x)
  })
}

/// Default characters that terminate a word for double-click selection (the
/// `word-delimiters` config key overrides this). `_`, `-`, `.`, `/`, `:`, `~`
/// are deliberately *not* delimiters so paths, URLs, and option flags select as
/// one unit.
const WORD_DELIMITERS: &str = " \t`!@#$%^&*()+=[]{}\\|;'\",<>?";

/// Mask of the kitty-keyboard flags we implement (disambiguate, report events,
/// alternate keys, all-keys-as-escapes, associated text).
const KITTY_ALL: u8 = 0b1_1111;

/// Whether `c` is part of a word (not whitespace, not in `delims`).
fn is_word(c: char, delims: &str) -> bool {
  !c.is_whitespace() && !delims.contains(c)
}

/// Split text into grapheme-ish units for `OSC 66 w=0` layout: each base
/// character with its trailing zero-width combining marks and display width.
/// Leading combining marks with no base are dropped, as in normal printing.
fn graphemes(text: &str) -> Vec<(char, String, usize)> {
  text
    .graphemes(true)
    .filter_map(|cluster| {
      let mut chars = cluster.chars();
      let base = chars.find(|c| c.width().unwrap_or(0) > 0)?;
      let mut rest = cluster.to_string();
      let index = rest.find(base)?;
      rest.replace_range(index..index + base.len_utf8(), "");
      Some((base, rest, UnicodeWidthStr::width(cluster).clamp(1, 2)))
    })
    .collect()
}

impl Grid {
  pub fn new(cols: usize, rows: usize) -> Self {
    let cols = cols.max(1);
    let rows = rows.max(1);
    Self {
      cols,
      rows,
      lines: vec![Line::blank(cols); rows],
      cursor: Cursor::default(),
      saved: Cursor::default(),
      top: 0,
      bottom: rows - 1,
      left: 0,
      right: cols - 1,
      lr_margins: false,
      pen: Cell::default(),
      autowrap: true,
      origin: false,
      insert: false,
      wrap_pending: false,
      tabs: default_tabs(cols),
      alt_saved: None,
      scrollback: VecDeque::new(),
      view_offset: 0,
      cursor_shape: CursorShape::default(),
      cursor_blink: false,
      cursor_visible: true,
      cursor_color: None,
      app_cursor: false,
      app_keypad: false,
      selection: None,
      selection_block: false,
      bracketed_paste: false,
      sync: false,
      mouse_protocol: MouseProtocol::Off,
      mouse_encoding: MouseEncoding::X10,
      focus_events: false,
      search: None,
      word_delimiters: WORD_DELIMITERS.to_string(),
      scrollback_cap: SCROLLBACK_CAP,
      last_base: None,
      last_char: None,
      links: Vec::new(),
      kitty_current: 0,
      kitty_stack: Vec::new(),
    }
  }

  /// The active kitty-keyboard flags (0 means legacy encoding).
  pub const fn kitty_flags(&self) -> u8 {
    self.kitty_current
  }

  /// Apply `CSI = flags ; mode u`: mode 1 replaces, 2 sets bits, 3 clears bits.
  pub const fn kitty_set(&mut self, flags: u8, mode: u8) {
    self.kitty_current = match mode {
      2 => self.kitty_current | flags,
      3 => self.kitty_current & !flags,
      _ => flags,
    } & KITTY_ALL;
  }

  /// Apply `CSI > flags u`: push the current flags and switch to `flags`.
  pub fn kitty_push(&mut self, flags: u8) {
    if self.kitty_stack.len() >= 32 {
      self.kitty_stack.remove(0);
    }
    self.kitty_stack.push(self.kitty_current);
    self.kitty_current = flags & KITTY_ALL;
  }

  /// Apply `CSI < n u`: pop `n` saved flag values, restoring the last one.
  pub fn kitty_pop(&mut self, n: usize) {
    for _ in 0..n.max(1) {
      self.kitty_current = self.kitty_stack.pop().unwrap_or(0);
    }
  }

  /// Override the word-delimiter set; `None` keeps the built-in default.
  pub fn set_word_delimiters(&mut self, delims: Option<String>) {
    if let Some(d) = delims {
      self.word_delimiters = d;
    }
  }

  /// Set the scrollback retention cap, trimming history if it shrank.
  pub fn set_scrollback_cap(&mut self, cap: usize) {
    self.scrollback_cap = cap;
    while self.scrollback.len() > cap {
      self.scrollback.pop_front();
    }
    self.view_offset = self.view_offset.min(self.scrollback.len());
  }

  pub const fn cols(&self) -> usize {
    self.cols
  }

  pub const fn rows(&self) -> usize {
    self.rows
  }

  pub const fn cursor(&self) -> (usize, usize) {
    (self.cursor.x, self.cursor.y)
  }

  pub const fn pen_mut(&mut self) -> &mut Cell {
    &mut self.pen
  }

  /// The current pen, for reporting active SGR attributes (DECRQSS).
  pub const fn pen(&self) -> &Cell {
    &self.pen
  }

  /// The scroll region as inclusive 0-based `(top, bottom)` rows.
  pub const fn scroll_region(&self) -> (usize, usize) {
    (self.top, self.bottom)
  }

  /// The DECSCUSR code (1-6) for the current cursor shape and blink state.
  pub const fn cursor_style_code(&self) -> u16 {
    let base = match self.cursor_shape {
      CursorShape::Block => 1,
      CursorShape::Underline => 3,
      CursorShape::Beam => 5,
    };
    if self.cursor_blink { base } else { base + 1 }
  }

  pub fn reset_pen(&mut self) {
    self.pen = Cell::default();
  }

  pub const fn set_autowrap(&mut self, on: bool) {
    self.autowrap = on;
  }

  pub fn set_origin(&mut self, on: bool) {
    self.origin = on;
    self.move_to(0, 0);
  }

  pub const fn set_insert(&mut self, on: bool) {
    self.insert = on;
  }

  pub const fn autowrap(&self) -> bool {
    self.autowrap
  }

  pub const fn origin(&self) -> bool {
    self.origin
  }

  pub const fn insert(&self) -> bool {
    self.insert
  }

  pub const fn alt_active(&self) -> bool {
    self.alt_saved.is_some()
  }

  pub const fn set_cursor_shape(&mut self, shape: CursorShape) {
    self.cursor_shape = shape;
  }

  pub const fn cursor_shape(&self) -> CursorShape {
    self.cursor_shape
  }

  pub const fn set_cursor_blink(&mut self, blink: bool) {
    self.cursor_blink = blink;
  }

  pub const fn cursor_blink(&self) -> bool {
    self.cursor_blink
  }

  /// Whether a blink timer can change anything currently displayed.
  pub fn needs_blink(&self) -> bool {
    (self.cursor_visible && self.cursor_blink && self.view_at_bottom())
      || (0..self.rows).any(|row| {
        self
          .view_row(row)
          .iter()
          .any(|cell| cell.flags.contains(Flags::BLINK))
      })
  }

  /// Whether a rapid-blink timer can change a currently visible cell.
  pub fn needs_rapid_blink(&self) -> bool {
    (0..self.rows).any(|row| {
      self
        .view_row(row)
        .iter()
        .any(|cell| cell.flags.contains(Flags::RAPID_BLINK))
    })
  }

  pub const fn set_cursor_visible(&mut self, visible: bool) {
    self.cursor_visible = visible;
  }

  pub const fn cursor_visible(&self) -> bool {
    self.cursor_visible
  }

  pub const fn set_cursor_color(&mut self, color: Option<(u8, u8, u8)>) {
    self.cursor_color = color;
  }

  pub const fn cursor_color(&self) -> Option<(u8, u8, u8)> {
    self.cursor_color
  }

  pub const fn set_app_cursor(&mut self, on: bool) {
    self.app_cursor = on;
  }

  pub const fn app_cursor(&self) -> bool {
    self.app_cursor
  }

  pub const fn set_app_keypad(&mut self, on: bool) {
    self.app_keypad = on;
  }

  pub const fn app_keypad(&self) -> bool {
    self.app_keypad
  }

  /// Place a printable character at the cursor, honouring width and autowrap.
  pub fn print(&mut self, c: char) {
    if self.joins_last_grapheme(c) {
      self.add_combining(c);
      return;
    }
    let width = c.width().unwrap_or(0);
    if width == 0 {
      // A zero-width combining mark attaches to the last base cell.
      self.add_combining(c);
      return;
    }
    let (lm, re) = (self.left_edge(), self.right_edge());
    if self.wrap_pending {
      self.cursor.x = lm;
      self.lines[self.cursor.y].wrapped = true;
      self.line_feed();
      self.wrap_pending = false;
    }
    if width == 2 && self.cursor.x + 1 >= re {
      // A double-width glyph cannot straddle the right margin: wrap first.
      if self.autowrap {
        self.cursor.x = lm;
        self.lines[self.cursor.y].wrapped = true;
        self.line_feed();
      } else {
        return;
      }
    }
    if self.insert {
      self.shift_right(width);
    }

    let (x, y) = (self.cursor.x, self.cursor.y);
    // Overwriting any cell of a text-sizing block dissolves the whole block,
    // so no orphaned continuation cells are left for the renderer to draw.
    self.clear_sized_at(x, y);
    if width == 2 && x + 1 < self.cols {
      self.clear_sized_at(x + 1, y);
    }
    let mut cell = self.pen.clone();
    cell.c = c;
    cell.combining = None;
    cell.sized = None;
    cell.flags.remove(Flags::WIDE_CONT);
    self.lines[y].cells[x] = cell;
    self.last_base = Some((x, y));
    self.last_char = Some(c);
    if width == 2 && x + 1 < self.cols {
      let mut cont = self.pen.clone();
      cont.c = ' ';
      cont.combining = None;
      cont.sized = None;
      cont.flags.insert(Flags::WIDE_CONT);
      self.lines[y].cells[x + 1] = cont;
    }

    let advance = width;
    if self.cursor.x + advance >= re {
      self.cursor.x = re - 1;
      self.wrap_pending = self.autowrap;
    } else {
      self.cursor.x += advance;
    }
  }

  /// REP: reprint the last base character `n` times at the cursor.
  pub fn repeat(&mut self, n: usize) {
    if let Some(c) = self.last_char {
      for _ in 0..n {
        self.print(c);
      }
    }
  }

  /// Attach a zero-width combining mark to the most recently printed base
  /// cell. Capped so a malicious stream of marks cannot grow a cell unbounded.
  fn add_combining(&mut self, mark: char) {
    const MAX_MARKS: usize = 16;
    let Some((x, y)) = self.last_base else {
      return;
    };
    let Some(cell) = self.lines.get_mut(y).and_then(|l| l.cells.get_mut(x))
    else {
      return;
    };
    let mut s: String =
      cell.combining.take().map(String::from).unwrap_or_default();
    if s.chars().count() < MAX_MARKS {
      s.push(mark);
    }
    cell.combining = Some(s.into_boxed_str());
  }

  fn joins_last_grapheme(&self, next: char) -> bool {
    let Some((x, y)) = self.last_base else {
      return false;
    };
    let Some(cell) = self.lines.get(y).and_then(|line| line.cells.get(x))
    else {
      return false;
    };
    let mut candidate = String::new();
    candidate.push(cell.c);
    if let Some(rest) = &cell.combining {
      candidate.push_str(rest);
    }
    candidate.push(next);
    candidate.graphemes(true).count() == 1
  }

  /// Lay out a text-sizing run (`OSC 66`) as scaled multicell blocks at the
  /// cursor, advancing it on the same row by the total block width, as the
  /// protocol requires. With `width == 0` each grapheme gets its own `s` by `s`
  /// block (the font scaled by `s`); with `width > 0` the whole run is packed
  /// into one block `s * width` cells wide and `s` high. A plain descriptor
  /// (scale 1, no width, no fraction) falls back to ordinary printing.
  pub fn print_sized(&mut self, text: &str, size: TextSize) {
    if size.is_plain() {
      for c in text.chars() {
        self.print(c);
      }
      return;
    }
    let rows = size.cell_height().clamp(1, self.rows);
    if size.width == 0 {
      for (base, marks, w) in graphemes(text) {
        let cols = (rows * w).clamp(1, self.cols);
        self.place_block(base, marks, None, size, cols, rows);
      }
    } else {
      let cols = (rows * size.width as usize).clamp(1, self.cols);
      self.place_block(' ', String::new(), Some(text), size, cols, rows);
    }
  }

  /// Write one scaled block of `cols` by `rows` cells at the cursor and
  /// advance past it. The leading cell carries the text; the rest are flagged
  /// [`Flags::SIZED_CONT`]. A block that will not fit wraps to the next line.
  fn place_block(
    &mut self,
    lead: char,
    marks: String,
    run: Option<&str>,
    size: TextSize,
    cols: usize,
    rows: usize,
  ) {
    if self.wrap_pending || (self.autowrap && self.cursor.x + cols > self.cols)
    {
      self.cursor.x = 0;
      self.lines[self.cursor.y].wrapped = true;
      self.line_feed();
      self.wrap_pending = false;
    }
    let (x0, y0) = (self.cursor.x, self.cursor.y);
    let marks = (!marks.is_empty()).then(|| marks.into_boxed_str());
    for dy in 0..rows {
      let cy = y0 + dy;
      if cy >= self.rows {
        break;
      }
      for dx in 0..cols {
        let cx = x0 + dx;
        if cx >= self.cols {
          break;
        }
        // Dissolve any block already occupying this cell before reusing it.
        self.clear_sized_at(cx, cy);
        let lead_cell = dx == 0 && dy == 0;
        let mut cell = self.pen.clone();
        cell.c = if lead_cell { lead } else { ' ' };
        cell.combining = if lead_cell { marks.clone() } else { None };
        cell.flags.remove(Flags::WIDE_CONT);
        if !lead_cell {
          cell.flags.insert(Flags::SIZED_CONT);
        }
        cell.sized = Some(Box::new(Sized {
          size,
          cols: u8::try_from(cols).unwrap_or(u8::MAX),
          rows: u8::try_from(rows).unwrap_or(u8::MAX),
          dx: u8::try_from(dx).unwrap_or(u8::MAX),
          dy: u8::try_from(dy).unwrap_or(u8::MAX),
          run: if lead_cell { run.map(Into::into) } else { None },
        }));
        self.lines[cy].cells[cx] = cell;
      }
    }
    self.last_base = None;
    if self.cursor.x + cols >= self.cols {
      self.cursor.x = self.cols - 1;
      self.wrap_pending = self.autowrap;
    } else {
      self.cursor.x += cols;
    }
  }

  pub fn cell(&self, x: usize, y: usize) -> &Cell {
    &self.lines[y].cells[x]
  }
}

#[cfg(test)] mod tests;
