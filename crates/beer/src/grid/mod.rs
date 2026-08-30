//! The terminal screen: a grid of styled cells, a cursor, and the editing
//! operations the VT parser drives.

mod links;
mod search;
mod selection;

use std::{collections::VecDeque, num::NonZeroU16};

pub use links::UrlHit;
use search::SearchState;
use unicode_width::UnicodeWidthChar;

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
  /// Interpret search queries as regular expressions (config `[search]
  /// regex`).
  search_regex:    bool,
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

/// Format a row of cells as text, skipping continuation cells and trimming
/// trailing blanks.
fn cells_text(cells: &[Cell]) -> String {
  cells
    .iter()
    .filter(|c| {
      !c.flags.contains(Flags::WIDE_CONT)
        && !c.flags.contains(Flags::SIZED_CONT)
    })
    .map(|c| c.c)
    .collect::<String>()
    .trim_end()
    .to_string()
}

/// Join lines with newlines after dropping trailing empty lines.
fn join_trimmed(mut lines: Vec<String>) -> String {
  while lines.last().is_some_and(String::is_empty) {
    lines.pop();
  }
  lines.join("\n")
}

fn default_tabs(cols: usize) -> Vec<bool> {
  (0..cols).map(|i| i % 8 == 0 && i != 0).collect()
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
  let mut out: Vec<(char, String, usize)> = Vec::new();
  for c in text.chars() {
    match c.width().unwrap_or(0) {
      0 => {
        if let Some(last) = out.last_mut() {
          last.1.push(c);
        }
      },
      w => out.push((c, String::new(), w)),
    }
  }
  out
}

#[expect(
  clippy::absolute_paths,
  reason = "grid internals use fully-qualified standard-library ownership \
            operations"
)]
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
      search_regex: false,
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

  /// Choose whether search queries are regular expressions or literal text.
  pub const fn set_search_regex(&mut self, on: bool) {
    self.search_regex = on;
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

  /// Resize the screen. On the main screen this reflows: soft-wrapped runs are
  /// rejoined into logical lines and rewrapped to the new width, across both
  /// scrollback and the live screen, keeping the cursor on its content. The
  /// alternate screen just clips, since its apps repaint on resize.
  pub fn resize(&mut self, cols: usize, rows: usize) {
    let cols = cols.max(1);
    let rows = rows.max(1);
    self.clear_sized_runs();
    if self.alt_saved.is_some() {
      self.clip_resize(cols, rows);
    } else {
      self.reflow_resize(cols, rows);
    }
    self.cols = cols;
    self.rows = rows;
    self.top = 0;
    self.bottom = rows - 1;
    self.left = 0;
    self.right = cols - 1;
    self.tabs = default_tabs(cols);
    self.cursor.x = self.cursor.x.min(cols - 1);
    self.cursor.y = self.cursor.y.min(rows - 1);
    self.wrap_pending = false;
    self.view_offset = 0;
  }

  /// Clip-resize the live screen and the saved primary (alternate screen).
  fn clip_resize(&mut self, cols: usize, rows: usize) {
    for line in &mut self.lines {
      line.cells.resize(cols, Cell::default());
    }
    self.lines.resize(rows, Line::blank(cols));
    if let Some(saved) = self.alt_saved.as_mut() {
      for line in saved.iter_mut() {
        line.cells.resize(cols, Cell::default());
      }
      saved.resize(rows, Line::blank(cols));
    }
    self.clear_selection();
    self.clear_search();
  }

  /// Reflow scrollback + live content to a new width, rewrapping soft-wrapped
  /// paragraphs and repositioning the cursor onto its character.
  fn reflow_resize(&mut self, cols: usize, rows: usize) {
    let cursor_abs = self.scrollback.len() + self.cursor.y;
    let total = self.scrollback.len() + self.lines.len();

    // 1. Rejoin soft-wrapped rows into logical lines. Track which logical line
    //    the cursor falls in and its offset within that line.
    let mut logicals: Vec<Vec<Cell>> = Vec::new();
    let mut logical_marks: Vec<Option<PromptKind>> = Vec::new();
    let mut acc: Vec<Cell> = Vec::new();
    let mut acc_mark: Option<PromptKind> = None;
    let mut cur_logical = 0usize;
    let mut cur_off = 0usize;
    for abs in 0..total {
      if abs == cursor_abs {
        cur_logical = logicals.len();
        cur_off = acc.len() + self.cursor.x;
      }
      let line = if abs < self.scrollback.len() {
        &self.scrollback[abs]
      } else {
        &self.lines[abs - self.scrollback.len()]
      };
      // The mark on the first physical row of a logical line carries over.
      if acc.is_empty() {
        acc_mark = line.prompt;
      }
      acc.extend_from_slice(&line.cells);
      if !line.wrapped {
        logicals.push(std::mem::take(&mut acc));
        logical_marks.push(acc_mark.take());
      }
    }
    if !acc.is_empty() {
      logicals.push(acc);
      logical_marks.push(acc_mark);
    }
    // Drop trailing all-blank lines (empty screen below the content), but
    // never above the cursor's line, so the cursor keeps its row.
    let last_content = logicals
      .iter()
      .rposition(|l| l.iter().any(|c| *c != Cell::default()))
      .unwrap_or(0);
    logicals.truncate(last_content.max(cur_logical) + 1);
    logical_marks.truncate(last_content.max(cur_logical) + 1);

    // 2. Rewrap each logical line to the new width, recording where the cursor
    //    lands. Trailing blanks are dropped so a hard line does not rewrap its
    //    padding onto extra rows.
    let mut new_lines: Vec<Line> = Vec::new();
    let mut new_cursor_abs = 0usize;
    let mut new_cursor_col = 0usize;
    for (li, mut logical) in logicals.into_iter().enumerate() {
      let trim = logical
        .iter()
        .rposition(|c| *c != Cell::default())
        .map_or(0, |p| p + 1);
      logical.truncate(trim);
      let first = new_lines.len();
      let chunks = logical.len().div_ceil(cols).max(1);
      for ci in 0..chunks {
        let start = ci * cols;
        let end = (start + cols).min(logical.len());
        let mut cells = logical.get(start..end).unwrap_or(&[]).to_vec();
        cells.resize(cols, Cell::default());
        new_lines.push(Line {
          cells,
          wrapped: ci + 1 < chunks,
          // The mark belongs to the first physical row of the line.
          prompt: if ci == 0 { logical_marks[li] } else { None },
        });
      }
      if li == cur_logical {
        let off = cur_off.min(logical.len());
        let chunk = (off / cols).min(chunks - 1);
        new_cursor_abs = first + chunk;
        new_cursor_col = (off - chunk * cols).min(cols - 1);
      }
    }

    // 3. The last `rows` lines are the live screen; the rest is scrollback.
    let live_start = new_lines.len().saturating_sub(rows);
    let mut scrollback: VecDeque<Line> =
      new_lines.drain(0..live_start).collect();
    let mut live = new_lines;
    while live.len() < rows {
      live.push(Line::blank(cols));
    }
    while scrollback.len() > self.scrollback_cap {
      scrollback.pop_front();
    }

    self.cursor.y = new_cursor_abs.saturating_sub(live_start).min(rows - 1);
    self.cursor.x = new_cursor_col;
    self.lines = live;
    self.scrollback = scrollback;
    self.clear_selection();
    self.clear_search();
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
    const MAX_MARKS: usize = 8;
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

  /// Stamp a graphics-protocol placement as a `cols` by `rows` cell rectangle
  /// with its top-left at the cursor, then move the cursor unless `keep_cursor`
  /// (the `C=1` policy) is set. Each cell records its `(dx, dy)` in the
  /// placement so the renderer composites the right image slice; the pixels and
  /// geometry live in the graphics engine. Cells beyond the screen are clipped.
  pub fn place_image(
    &mut self,
    image: u32,
    placement: u32,
    cols: usize,
    rows: usize,
    keep_cursor: bool,
  ) {
    let x0 = self.cursor.x;
    // Scroll the screen to make vertical room for the image. Only scroll
    // when the full screen is the active scroll region; a DECSTBM region
    // or the alternate screen keeps the image clipped as before.
    let y0 = if self.top == 0
      && self.bottom == self.rows - 1
      && self.alt_saved.is_none()
    {
      let y0_initial = self.cursor.y;
      let scroll_n = (y0_initial + rows)
        .saturating_sub(self.rows)
        .min(y0_initial);
      if scroll_n > 0 {
        self.scroll_up(scroll_n);
      }
      y0_initial - scroll_n
    } else {
      self.cursor.y
    };
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
        let cell = &mut self.lines[cy].cells[cx];
        cell.image = Some(ImageRef {
          image,
          placement,
          dx: u16::try_from(dx).unwrap_or(u16::MAX),
          dy: u16::try_from(dy).unwrap_or(u16::MAX),
        });
      }
    }
    if keep_cursor {
      return;
    }
    // Land the cursor just past the image on its bottom row, the way kitty
    // leaves it, clamped to the screen.
    self.cursor.y = (y0 + rows.saturating_sub(1)).min(self.rows - 1);
    self.cursor.x = (x0 + cols).min(self.cols - 1);
    self.wrap_pending = false;
  }

  /// Remove image placements: every cell whose reference matches `pred` is
  /// cleared back to a blank. With `pred` always true this erases all images.
  pub fn clear_images(&mut self, pred: impl Fn(&ImageRef) -> bool) {
    let blank = Cell::default();
    let touch = |line: &mut Line| {
      for cell in &mut line.cells {
        if cell.image.is_some_and(|r| pred(&r)) {
          *cell = blank.clone();
        }
      }
    };
    self.lines.iter_mut().for_each(touch);
    self.scrollback.iter_mut().for_each(touch);
    if let Some(alt) = self.alt_saved.as_mut() {
      alt.iter_mut().for_each(touch);
    }
  }

  /// The image placements intersecting the current cursor cell, for `d=c`
  /// deletes: returns each `(image, placement)` found there.
  pub fn images_at_cursor(&self) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    if let Some(r) = self.lines[self.cursor.y].cells[self.cursor.x].image {
      out.push((r.image, r.placement));
    }
    out
  }

  /// If `(x, y)` belongs to a text-sizing block, blank every cell of that
  /// block so a write into it cannot orphan continuation cells.
  fn clear_sized_at(&mut self, x: usize, y: usize) {
    let Some(s) = self
      .lines
      .get(y)
      .and_then(|l| l.cells.get(x))
      .and_then(|c| c.sized.as_deref())
    else {
      return;
    };
    let (dx, dy, cols, rows) = (
      s.dx as usize,
      s.dy as usize,
      s.cols as usize,
      s.rows as usize,
    );
    let x0 = x.saturating_sub(dx);
    let y0 = y.saturating_sub(dy);
    let blank = self.pen_blank();
    let x_end = (x0 + cols).min(self.cols);
    for by in y0..(y0 + rows).min(self.rows) {
      for cell in &mut self.lines[by].cells[x0..x_end] {
        // Only blank cells that are themselves part of a sized block, so
        // a stale `(dx, dy)` (e.g. after a scroll split the block) can
        // never wipe unrelated plain text - at worst it leaves a fragment.
        if cell.sized.is_some() {
          *cell = blank.clone();
        }
      }
    }
  }

  /// Dissolve every text-sizing block that intersects cells `[from, to)` of row
  /// `y`, so any write into a block (erase, delete, shift) removes the whole
  /// block rather than leaving the renderer to draw an orphaned fragment.
  fn dissolve_sized(&mut self, y: usize, from: usize, to: usize) {
    let to = to.min(self.cols);
    for x in from..to {
      if self
        .lines
        .get(y)
        .and_then(|l| l.cells.get(x))
        .is_some_and(|c| c.sized.is_some())
      {
        // Clearing the block blanks the rest of the range's members too,
        // so subsequent iterations find nothing and skip.
        self.clear_sized_at(x, y);
      }
    }
  }

  /// Drop all text-sizing blocks: their cells become plain characters. Used
  /// before a resize, since a scaled block must not be split across a rewrap;
  /// applications using `OSC 66` repaint on resize regardless.
  fn clear_sized_runs(&mut self) {
    let strip = |line: &mut Line| {
      for cell in &mut line.cells {
        if cell.sized.take().is_some() {
          cell.flags.remove(Flags::SIZED_CONT);
        }
      }
    };
    self.lines.iter_mut().for_each(strip);
    self.scrollback.iter_mut().for_each(strip);
    if let Some(alt) = self.alt_saved.as_mut() {
      alt.iter_mut().for_each(strip);
    }
  }

  fn shift_right(&mut self, n: usize) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    let end = self.right_edge();
    // Shifting cells sideways would scramble a scaled block's back-references.
    self.dissolve_sized(y, x, end);
    let blank = self.pen_blank();
    let row = &mut self.lines[y].cells;
    for i in (x + n..end).rev() {
      row[i] = row[i - n].clone();
    }
    for cell in &mut row[x..(x + n).min(end)] {
      *cell = blank.clone();
    }
  }

  fn pen_blank(&self) -> Cell {
    // A space carrying only the current background (back-colour erase).
    Cell {
      bg: self.pen.bg,
      ..Cell::default()
    }
  }

  const fn region(&self) -> (usize, usize) {
    if self.origin {
      (self.top, self.bottom)
    } else {
      (0, self.rows - 1)
    }
  }

  /// The column region the cursor addresses within: the left/right margins in
  /// origin mode when they are enabled, else the full width.
  const fn hregion(&self) -> (usize, usize) {
    if self.origin && self.lr_margins {
      (self.left, self.right)
    } else {
      (0, self.cols - 1)
    }
  }

  /// The column a wrap returns to: the left margin when the cursor is inside an
  /// enabled left/right region, else column 0.
  const fn left_edge(&self) -> usize {
    if self.lr_margins
      && self.cursor.x >= self.left
      && self.cursor.x <= self.right
    {
      self.left
    } else {
      0
    }
  }

  /// The exclusive column bound for wrapping and in-line edits: one past the
  /// right margin when the cursor is inside an enabled region, else the width.
  const fn right_edge(&self) -> usize {
    if self.lr_margins
      && self.cursor.x >= self.left
      && self.cursor.x <= self.right
    {
      self.right + 1
    } else {
      self.cols
    }
  }

  /// Copy the `[left..=right]` span of row `src` into row `dst`, for a scroll
  /// confined by the left/right margins.
  fn copy_span(&mut self, dst: usize, src: usize) {
    let (l, r) = (self.left, self.right);
    self.dissolve_sized(dst, l, r + 1);
    self.dissolve_sized(src, l, r + 1);
    let span: Vec<Cell> = self.lines[src].cells[l..=r].to_vec();
    self.lines[dst].cells[l..=r].clone_from_slice(&span);
  }

  /// Blank the `[left..=right]` span of row `y` with the pen background.
  fn blank_span(&mut self, y: usize) {
    let (l, r) = (self.left, self.right);
    self.dissolve_sized(y, l, r + 1);
    let blank = self.pen_blank();
    for cell in &mut self.lines[y].cells[l..=r] {
      *cell = blank.clone();
    }
  }

  pub fn move_to(&mut self, x: usize, y: usize) {
    let (rt, rb) = self.region();
    let (cl, cr) = self.hregion();
    self.cursor.x = (x + cl).min(cr).max(cl);
    self.cursor.y = (y + rt).min(rb).max(rt);
    self.wrap_pending = false;
  }

  pub fn move_to_col(&mut self, x: usize) {
    self.cursor.x = x.min(self.cols - 1);
    self.wrap_pending = false;
  }

  pub fn move_to_row(&mut self, y: usize) {
    let (rt, rb) = self.region();
    self.cursor.y = (y + rt).min(rb).max(rt);
    self.wrap_pending = false;
  }

  pub fn cursor_up(&mut self, n: usize) {
    let (rt, _) = self.region();
    self.cursor.y = self.cursor.y.saturating_sub(n).max(rt);
    self.wrap_pending = false;
  }

  pub fn cursor_down(&mut self, n: usize) {
    let (_, rb) = self.region();
    self.cursor.y = (self.cursor.y + n).min(rb);
    self.wrap_pending = false;
  }

  pub fn cursor_fwd(&mut self, n: usize) {
    self.cursor.x = (self.cursor.x + n).min(self.cols - 1);
    self.wrap_pending = false;
  }

  pub const fn cursor_back(&mut self, n: usize) {
    self.cursor.x = self.cursor.x.saturating_sub(n);
    self.wrap_pending = false;
  }

  pub const fn save_cursor(&mut self) {
    self.saved = self.cursor;
  }

  pub fn restore_cursor(&mut self) {
    self.cursor = self.saved;
    self.cursor.x = self.cursor.x.min(self.cols - 1);
    self.cursor.y = self.cursor.y.min(self.rows - 1);
    self.wrap_pending = false;
  }

  pub const fn carriage_return(&mut self) {
    self.cursor.x = 0;
    self.wrap_pending = false;
  }

  pub const fn backspace(&mut self) {
    self.cursor.x = self.cursor.x.saturating_sub(1);
    self.wrap_pending = false;
  }

  /// LF/VT/FF: move down one row, scrolling at the region bottom.
  pub fn line_feed(&mut self) {
    if self.cursor.y == self.bottom {
      self.scroll_up(1);
    } else if self.cursor.y < self.rows - 1 {
      self.cursor.y += 1;
    }
    self.wrap_pending = false;
  }

  /// RI: move up one row, scrolling down at the region top.
  pub fn reverse_index(&mut self) {
    if self.cursor.y == self.top {
      self.scroll_down(1);
    } else if self.cursor.y > 0 {
      self.cursor.y -= 1;
    }
    self.wrap_pending = false;
  }

  /// NEL: carriage return plus line feed.
  pub fn next_line(&mut self) {
    self.carriage_return();
    self.line_feed();
  }

  pub fn tab(&mut self) {
    let mut x = self.cursor.x + 1;
    while x < self.cols && !self.tabs[x] {
      x += 1;
    }
    self.cursor.x = x.min(self.cols - 1);
    self.wrap_pending = false;
  }

  pub fn set_tab(&mut self) {
    if self.cursor.x < self.cols {
      self.tabs[self.cursor.x] = true;
    }
  }

  pub fn clear_tab(&mut self) {
    if self.cursor.x < self.cols {
      self.tabs[self.cursor.x] = false;
    }
  }

  pub fn clear_all_tabs(&mut self) {
    self.tabs.iter_mut().for_each(|t| *t = false);
  }

  pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
    if top < bottom && bottom < self.rows {
      self.top = top;
      self.bottom = bottom;
    } else {
      self.top = 0;
      self.bottom = self.rows - 1;
    }
    self.move_to(0, 0);
  }

  /// Whether left/right margins are enabled (DECLRMM, DECSET `?69`).
  pub const fn lr_margins_enabled(&self) -> bool {
    self.lr_margins
  }

  /// DECLRMM (DECSET `?69`): enable or disable left/right margins. Disabling
  /// resets the margins to the full width.
  pub const fn set_lr_margins_mode(&mut self, on: bool) {
    self.lr_margins = on;
    if !on {
      self.left = 0;
      self.right = self.cols - 1;
    }
  }

  /// DECSLRM (`CSI Pl ; Pr s`): set the left/right margins when DECLRMM is on,
  /// then home the cursor. Out-of-order or out-of-range values reset to full.
  pub fn set_lr_margins(&mut self, left: usize, right: usize) {
    if !self.lr_margins {
      return;
    }
    if left < right && right < self.cols {
      self.left = left;
      self.right = right;
    } else {
      self.left = 0;
      self.right = self.cols - 1;
    }
    self.move_to(0, 0);
  }

  pub fn scroll_up(&mut self, n: usize) {
    let n = n.min(self.bottom - self.top + 1);
    let full_width = self.left == 0 && self.right == self.cols - 1;
    // Lines leaving the top of the *whole* main screen become scrollback; a
    // DECSTBM region scroll (top > 0), a margin-confined scroll, or the alt
    // screen does not.
    if self.top == 0 && full_width && self.alt_saved.is_none() {
      for y in 0..n {
        let line =
          std::mem::replace(&mut self.lines[y], Line::blank(self.cols));
        self.scrollback.push_back(line);
      }
      let mut evicted = 0;
      while self.scrollback.len() > self.scrollback_cap {
        self.scrollback.pop_front();
        evicted += 1;
      }
      if evicted > 0 {
        self.shift_selection(evicted);
        self.shift_search(evicted);
      }
      // Keep a scrolled-back viewport anchored to the same content.
      if self.view_offset > 0 {
        self.view_offset = (self.view_offset + n).min(self.scrollback.len());
      }
    }
    if full_width {
      for y in self.top..=self.bottom {
        if y + n <= self.bottom {
          self.lines.swap(y, y + n);
        }
      }
      for y in (self.bottom + 1 - n)..=self.bottom {
        self.blank_row(y);
      }
    } else {
      for y in self.top..=self.bottom {
        if y + n <= self.bottom {
          self.copy_span(y, y + n);
        } else {
          self.blank_span(y);
        }
      }
    }
  }

  pub fn scroll_down(&mut self, n: usize) {
    let n = n.min(self.bottom - self.top + 1);
    let full_width = self.left == 0 && self.right == self.cols - 1;
    for y in (self.top..=self.bottom).rev() {
      if y >= self.top + n {
        if full_width {
          self.lines.swap(y, y - n);
        } else {
          self.copy_span(y, y - n);
        }
      } else if full_width {
        self.blank_row(y);
      } else {
        self.blank_span(y);
      }
    }
  }

  fn blank_row(&mut self, y: usize) {
    let cols = self.cols;
    self.dissolve_sized(y, 0, cols);
    let blank = self.pen_blank();
    let line = &mut self.lines[y];
    for cell in &mut line.cells {
      *cell = blank.clone();
    }
    line.wrapped = false;
  }

  /// DECALN (`ESC # 8`): fill the screen with `E` in default attributes for
  /// alignment testing, reset the scroll region, and home the cursor.
  pub fn decaln(&mut self) {
    self.top = 0;
    self.bottom = self.rows - 1;
    for y in 0..self.rows {
      self.dissolve_sized(y, 0, self.cols);
      let line = &mut self.lines[y];
      for cell in &mut line.cells {
        *cell = Cell {
          c: 'E',
          ..Cell::default()
        };
      }
      line.wrapped = false;
    }
    self.cursor.x = 0;
    self.cursor.y = 0;
    self.wrap_pending = false;
  }

  /// ED: 0=below, 1=above, 2/3=all.
  pub fn erase_display(&mut self, mode: u16) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    match mode {
      0 => {
        self.erase_in_row(y, x, self.cols);
        for r in (y + 1)..self.rows {
          self.blank_row(r);
        }
      },
      1 => {
        for r in 0..y {
          self.blank_row(r);
        }
        self.erase_in_row(y, 0, x + 1);
      },
      _ => {
        for r in 0..self.rows {
          self.blank_row(r);
        }
      },
    }
    self.wrap_pending = false;
  }

  /// EL: 0=right, 1=left, 2=line.
  pub fn erase_line(&mut self, mode: u16) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    match mode {
      0 => self.erase_in_row(y, x, self.cols),
      1 => self.erase_in_row(y, 0, x + 1),
      _ => self.erase_in_row(y, 0, self.cols),
    }
    self.wrap_pending = false;
  }

  /// ECH: erase n characters from the cursor without moving it.
  pub fn erase_chars(&mut self, n: usize) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    self.erase_in_row(y, x, (x + n).min(self.cols));
  }

  fn erase_in_row(&mut self, y: usize, from: usize, to: usize) {
    let to = to.min(self.cols);
    self.dissolve_sized(y, from, to);
    let blank = self.pen_blank();
    for cell in &mut self.lines[y].cells[from..to] {
      *cell = blank.clone();
    }
  }

  /// ICH: insert n blanks at the cursor, shifting the rest right.
  pub fn insert_chars(&mut self, n: usize) {
    let saved = self.insert;
    self.insert = true;
    self.shift_right(n.min(self.cols));
    self.insert = saved;
  }

  /// DCH: delete n characters at the cursor, shifting the rest left.
  pub fn delete_chars(&mut self, n: usize) {
    let (x, y) = (self.cursor.x, self.cursor.y);
    let end = self.right_edge();
    let n = n.min(end - x);
    // Shifting cells sideways would scramble a scaled block's back-references.
    self.dissolve_sized(y, x, end);
    let blank = self.pen_blank();
    let row = &mut self.lines[y].cells;
    for i in x..end {
      row[i] = if i + n < end {
        row[i + n].clone()
      } else {
        blank.clone()
      };
    }
  }

  /// IL: insert n blank lines at the cursor row, within the scroll region and
  /// (when set) the left/right margins.
  pub fn insert_lines(&mut self, n: usize) {
    if self.cursor.y < self.top || self.cursor.y > self.bottom {
      return;
    }
    if self.lr_margins
      && (self.cursor.x < self.left || self.cursor.x > self.right)
    {
      return;
    }
    let n = n.min(self.bottom - self.cursor.y + 1);
    if self.left == 0 && self.right == self.cols - 1 {
      for y in (self.cursor.y..=self.bottom).rev() {
        if y >= self.cursor.y + n {
          self.lines.swap(y, y - n);
        }
      }
      for y in self.cursor.y..(self.cursor.y + n) {
        self.blank_row(y);
      }
    } else {
      for y in (self.cursor.y..=self.bottom).rev() {
        if y >= self.cursor.y + n {
          self.copy_span(y, y - n);
        } else {
          self.blank_span(y);
        }
      }
    }
  }

  /// DL: delete n lines at the cursor row, within the scroll region and (when
  /// set) the left/right margins.
  pub fn delete_lines(&mut self, n: usize) {
    if self.cursor.y < self.top || self.cursor.y > self.bottom {
      return;
    }
    if self.lr_margins
      && (self.cursor.x < self.left || self.cursor.x > self.right)
    {
      return;
    }
    let n = n.min(self.bottom - self.cursor.y + 1);
    if self.left == 0 && self.right == self.cols - 1 {
      for y in self.cursor.y..=self.bottom {
        if y + n <= self.bottom {
          self.lines.swap(y, y + n);
        }
      }
      for y in (self.bottom + 1 - n)..=self.bottom {
        self.blank_row(y);
      }
    } else {
      for y in self.cursor.y..=self.bottom {
        if y + n <= self.bottom {
          self.copy_span(y, y + n);
        } else {
          self.blank_span(y);
        }
      }
    }
  }

  /// Resolve `rect` (origin-relative, 0-based, inclusive) to absolute grid
  /// coordinates clamped to the addressable region.
  fn clamp_rect(&self, rect: Rect) -> Rect {
    let (rt, rb) = self.region();
    let (cl, cr) = self.hregion();
    let top = (rect.top + rt).min(rb);
    let left = (rect.left + cl).min(cr);
    Rect {
      top,
      left,
      bottom: (rect.bottom + rt).min(rb).max(top),
      right: (rect.right + cl).min(cr).max(left),
    }
  }

  /// DECFRA: fill an inclusive rectangle with `c` in the current pen.
  pub fn fill_rect(&mut self, c: char, rect: Rect) {
    let a = self.clamp_rect(rect);
    let mut cell = self.pen.clone();
    cell.c = c;
    cell.combining = None;
    cell.sized = None;
    cell.flags.remove(Flags::WIDE_CONT);
    for y in a.top..=a.bottom {
      self.dissolve_sized(y, a.left, a.right + 1);
      for cell_ref in &mut self.lines[y].cells[a.left..=a.right] {
        *cell_ref = cell.clone();
      }
    }
  }

  /// DECERA: erase an inclusive rectangle to the pen background.
  pub fn erase_rect(&mut self, rect: Rect) {
    let a = self.clamp_rect(rect);
    for y in a.top..=a.bottom {
      self.erase_in_row(y, a.left, a.right + 1);
    }
  }

  /// DECCARA: set/clear flag attributes (and optionally the underline style)
  /// over an inclusive rectangle, leaving the characters in place.
  pub fn change_attrs_rect(
    &mut self,
    rect: Rect,
    set: Flags,
    clear: Flags,
    underline: Option<Underline>,
  ) {
    let a = self.clamp_rect(rect);
    for y in a.top..=a.bottom {
      for cell in &mut self.lines[y].cells[a.left..=a.right] {
        cell.flags.insert(set);
        cell.flags.remove(clear);
        if let Some(u) = underline {
          cell.underline = u;
        }
      }
    }
  }

  /// DECCRA: copy an inclusive source rectangle to a destination top-left,
  /// snapshotting first so overlapping copies are well defined.
  pub fn copy_rect(&mut self, src: Rect, dst_top: usize, dst_left: usize) {
    let s = self.clamp_rect(src);
    let (rt, _) = self.region();
    let (cl, _) = self.hregion();
    let dt = (dst_top + rt).min(self.rows - 1);
    let dl = (dst_left + cl).min(self.cols - 1);
    let mut buf: Vec<Vec<Cell>> = Vec::with_capacity(s.bottom - s.top + 1);
    for y in s.top..=s.bottom {
      buf.push(self.lines[y].cells[s.left..=s.right].to_vec());
    }
    for (dy, row) in buf.into_iter().enumerate() {
      let ty = dt + dy;
      if ty >= self.rows {
        break;
      }
      self.dissolve_sized(ty, dl, (dl + row.len()).min(self.cols));
      for (dx, cell) in row.into_iter().enumerate() {
        let tx = dl + dx;
        if tx < self.cols {
          self.lines[ty].cells[tx] = cell;
        }
      }
    }
  }

  pub fn enter_alt_screen(&mut self) {
    if self.alt_saved.is_some() {
      return;
    }
    self.view_offset = 0;
    let blank = vec![Line::blank(self.cols); self.rows];
    self.alt_saved = Some(std::mem::replace(&mut self.lines, blank));
  }

  pub fn leave_alt_screen(&mut self) {
    if let Some(main) = self.alt_saved.take() {
      self.lines = main;
    }
  }

  /// RIS (`ESC c`): return to the main screen and reset the grid-side modes to
  /// their power-on defaults, then clear the screen. Scrollback is kept.
  pub fn hard_reset(&mut self) {
    self.leave_alt_screen();
    self.reset_pen();
    self.top = 0;
    self.bottom = self.rows - 1;
    self.left = 0;
    self.right = self.cols - 1;
    self.lr_margins = false;
    self.autowrap = true;
    self.origin = false;
    self.insert = false;
    self.wrap_pending = false;
    self.tabs = default_tabs(self.cols);
    self.cursor = Cursor::default();
    self.saved = Cursor::default();
    self.cursor_visible = true;
    self.cursor_color = None;
    self.app_cursor = false;
    self.app_keypad = false;
    self.bracketed_paste = false;
    self.sync = false;
    self.focus_events = false;
    self.mouse_protocol = MouseProtocol::Off;
    self.mouse_encoding = MouseEncoding::X10;
    self.kitty_current = 0;
    self.kitty_stack.clear();
    self.erase_display(2);
  }

  /// Scroll the viewport by `delta` lines: positive = back into history,
  /// negative = toward the live screen. No-op on the alternate screen.
  #[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "scrollback positions are bounded by the grid and adjusted in \
              signed coordinates"
  )]
  pub fn scroll_view(&mut self, delta: isize) {
    if self.alt_saved.is_some() {
      return;
    }
    let max = self.scrollback.len() as isize;
    self.view_offset =
      (self.view_offset as isize + delta).clamp(0, max) as usize;
  }

  pub const fn scroll_to_bottom(&mut self) {
    self.view_offset = 0;
  }

  /// Whether the viewport is showing the live screen (not scrolled back).
  pub const fn view_at_bottom(&self) -> bool {
    self.view_offset == 0
  }

  /// One page (a screenful) of lines, for page-scroll bindings.
  pub fn page(&self) -> usize {
    self.rows.max(1)
  }

  /// The cells shown at viewport row `y` (0 = top of the window), accounting
  /// for the scrollback offset. May differ from `cols` in width if the line
  /// predates a resize (no reflow yet), so callers must not assume length.
  pub fn view_row(&self, y: usize) -> &[Cell] {
    let start = self.scrollback.len() - self.view_offset;
    let idx = start + y;
    if idx < self.scrollback.len() {
      &self.scrollback[idx].cells
    } else {
      &self.lines[idx - self.scrollback.len()].cells
    }
  }

  /// For a viewport cell `(y, x)` that is the left edge of a text-sizing block
  /// (its `dx == 0`), return the block's leading cell - which holds the text
  /// and the full descriptor - together with this row's `dy` within the block.
  /// `None` if `(y, x)` is not a left-edge sized cell, or the leading row is
  /// scrolled above the viewport top (the block is then clipped, not drawn).
  pub fn sized_lead(&self, y: usize, x: usize) -> Option<(&Cell, usize)> {
    let dy = {
      let s = self.view_row(y).get(x)?.sized.as_ref()?;
      if s.dx != 0 {
        return None;
      }
      s.dy as usize
    };
    if dy > y {
      return None;
    }
    let lead = self.view_row(y - dy).get(x)?;
    lead
      .sized
      .as_ref()
      .filter(|ls| ls.dx == 0 && ls.dy == 0)
      .map(|_| (lead, dy))
  }

  /// The absolute row currently shown at viewport row `y`.
  pub fn view_to_abs(&self, y: usize) -> usize {
    self.scrollback.len() - self.view_offset + y
  }

  /// The line at an absolute row (scrollback first, then the live screen).
  fn line_at_abs(&self, abs: usize) -> &Line {
    if abs < self.scrollback.len() {
      &self.scrollback[abs]
    } else {
      &self.lines[abs - self.scrollback.len()]
    }
  }

  /// Cells of an absolute row (scrollback first, then the live screen).
  fn abs_row(&self, row: usize) -> &[Cell] {
    if row < self.scrollback.len() {
      &self.scrollback[row].cells
    } else {
      &self.lines[row - self.scrollback.len()].cells
    }
  }

  /// Total rows across scrollback and the live screen.
  fn total_lines(&self) -> usize {
    self.scrollback.len() + self.lines.len()
  }

  /// The OSC 133 mark on an absolute row, if any.
  fn abs_prompt(&self, row: usize) -> Option<PromptKind> {
    if row < self.scrollback.len() {
      self.scrollback[row].prompt
    } else {
      self
        .lines
        .get(row - self.scrollback.len())
        .and_then(|l| l.prompt)
    }
  }

  /// Attach an OSC 133 prompt mark to the live line under the cursor.
  pub fn set_prompt_mark(&mut self, kind: PromptKind) {
    let y = self.cursor.y;
    if let Some(line) = self.lines.get_mut(y) {
      line.prompt = Some(kind);
    }
  }

  /// Scroll the viewport to the previous (`up`) or next prompt, placing that
  /// prompt line at the top of the window. No-op on the alternate screen or
  /// when there is no prompt in that direction.
  #[expect(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "prompt positions are bounded by the grid and adjusted in signed \
              coordinates"
  )]
  pub fn jump_prompt(&mut self, up: bool) {
    if self.alt_saved.is_some() {
      return;
    }
    let top = self.scrollback.len().saturating_sub(self.view_offset);
    let total = self.total_lines();
    let is_prompt = |k: Option<PromptKind>| k == Some(PromptKind::PromptStart);
    let target = if up {
      (0..top).rev().find(|&r| is_prompt(self.abs_prompt(r)))
    } else {
      ((top + 1)..total).find(|&r| is_prompt(self.abs_prompt(r)))
    };
    if let Some(t) = target {
      let offset = self.scrollback.len() as isize - t as isize;
      self.view_offset =
        offset.clamp(0, self.scrollback.len() as isize) as usize;
    }
  }

  /// Text of the most recent command's output: the rows from the last
  /// output-start (OSC 133 C) up to the command-end (D) or next prompt.
  pub fn last_command_output(&self) -> Option<String> {
    let total = self.total_lines();
    let start = (0..total)
      .rev()
      .find(|&r| self.abs_prompt(r) == Some(PromptKind::OutputStart))?;
    let mut lines: Vec<String> = Vec::new();
    for r in start..total {
      if r > start
        && matches!(
          self.abs_prompt(r),
          Some(PromptKind::CmdEnd | PromptKind::PromptStart)
        )
      {
        break;
      }
      lines.push(self.row_slice_text(r, 0, usize::MAX).trim_end().to_string());
    }
    // Drop trailing blank rows (e.g. the empty live screen below the output).
    while lines.last().is_some_and(std::string::String::is_empty) {
      lines.pop();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Some(out)
  }

  pub const fn set_bracketed_paste(&mut self, on: bool) {
    self.bracketed_paste = on;
  }

  pub const fn bracketed_paste(&self) -> bool {
    self.bracketed_paste
  }

  pub const fn set_sync(&mut self, on: bool) {
    self.sync = on;
  }

  pub const fn sync_active(&self) -> bool {
    self.sync
  }

  pub const fn set_mouse_protocol(&mut self, protocol: MouseProtocol) {
    self.mouse_protocol = protocol;
  }

  pub const fn mouse_protocol(&self) -> MouseProtocol {
    self.mouse_protocol
  }

  pub const fn set_mouse_encoding(&mut self, encoding: MouseEncoding) {
    self.mouse_encoding = encoding;
  }

  pub const fn mouse_encoding(&self) -> MouseEncoding {
    self.mouse_encoding
  }

  pub const fn set_focus_events(&mut self, on: bool) {
    self.focus_events = on;
  }

  pub const fn focus_events(&self) -> bool {
    self.focus_events
  }

  /// The visible text of one row, trailing blanks trimmed.
  #[cfg(test)]
  pub fn row_text(&self, y: usize) -> String {
    cells_text(&self.lines[y].cells)
  }

  /// The visible viewport as text, one line per row with trailing blank rows
  /// and blank cells trimmed. Used to pipe the on-screen contents.
  pub fn visible_text(&self) -> String {
    join_trimmed(
      (0..self.rows)
        .map(|y| cells_text(self.view_row(y)))
        .collect(),
    )
  }

  /// The full scrollback plus the live screen as text. Used to pipe or dump
  /// the whole history.
  pub fn scrollback_text(&self) -> String {
    let total = self.scrollback.len() + self.rows;
    join_trimmed((0..total).map(|r| cells_text(self.abs_row(r))).collect())
  }

  pub fn cell(&self, x: usize, y: usize) -> &Cell {
    &self.lines[y].cells[x]
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn kitty_flag_stack() {
    let mut g = Grid::new(4, 2);
    assert_eq!(g.kitty_flags(), 0);
    g.kitty_set(0b1, 1); // replace
    assert_eq!(g.kitty_flags(), 0b1);
    g.kitty_set(0b100, 2); // set bits
    assert_eq!(g.kitty_flags(), 0b101);
    g.kitty_set(0b1, 3); // clear bits
    assert_eq!(g.kitty_flags(), 0b100);
    g.kitty_push(0b11); // push current, switch
    assert_eq!(g.kitty_flags(), 0b11);
    g.kitty_pop(1); // restore
    assert_eq!(g.kitty_flags(), 0b100);
    g.kitty_pop(5); // underflow is harmless
    assert_eq!(g.kitty_flags(), 0);
  }

  #[test]
  fn prints_and_wraps() {
    let mut g = Grid::new(4, 2);
    for c in "abcde".chars() {
      g.print(c);
    }
    assert_eq!(g.row_text(0), "abcd");
    assert_eq!(g.row_text(1), "e");
    assert_eq!(g.cursor(), (1, 1));
  }

  #[test]
  fn combining_marks_attach_to_base_cell() {
    let mut g = Grid::new(8, 1);
    // "e" + COMBINING ACUTE ACCENT + "x": the mark joins the 'e' cell, the
    // 'x' lands in the next cell (the mark advanced nothing).
    for c in "e\u{0301}x".chars() {
      g.print(c);
    }
    assert_eq!(g.cell(0, 0).c, 'e');
    assert_eq!(g.cell(0, 0).combining.as_deref(), Some("\u{0301}"));
    assert_eq!(g.cell(1, 0).c, 'x');
    assert_eq!(g.cursor(), (2, 0));
    // Copied text round-trips the full grapheme cluster.
    g.start_selection(0, 0);
    g.extend_selection(0, 1);
    assert_eq!(g.selection_text().as_deref(), Some("e\u{0301}x"));
  }

  #[test]
  fn detects_urls_trimming_trailing_punctuation() {
    let mut g = Grid::new(60, 2);
    for c in "see https://example.com/p?q=1, ok".chars() {
      g.print(c);
    }
    let hits = g.visible_urls();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://example.com/p?q=1");
    assert_eq!((hits[0].row, hits[0].col), (0, 4));
  }

  #[test]
  fn detects_url_across_a_soft_wrap() {
    // 20 cols: the URL wraps, but autowrap sets the wrapped flag so it rejoins.
    let mut g = Grid::new(20, 3);
    for c in "x https://example.com/averylongpath".chars() {
      g.print(c);
    }
    let hits = g.visible_urls();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://example.com/averylongpath");
  }

  #[test]
  fn prompt_mark_survives_reflow() {
    let mut g = Grid::new(10, 4);
    g.set_prompt_mark(PromptKind::OutputStart); // marks line 0
    for c in "hello".chars() {
      g.print(c);
    }
    g.resize(6, 4); // rewrap to a narrower width
    // The mark followed its logical line, so the output is still found.
    assert_eq!(g.last_command_output().as_deref(), Some("hello\n"));
  }

  #[test]
  fn carriage_return_and_line_feed() {
    let mut g = Grid::new(8, 4);
    for c in "hi".chars() {
      g.print(c);
    }
    g.carriage_return();
    g.line_feed();
    for c in "yo".chars() {
      g.print(c);
    }
    assert_eq!(g.row_text(0), "hi");
    assert_eq!(g.row_text(1), "yo");
  }

  #[test]
  fn line_feed_scrolls_at_bottom() {
    let mut g = Grid::new(4, 2);
    g.print('a');
    g.next_line();
    g.print('b');
    g.next_line(); // scrolls: row0 <- "b", row1 blank
    assert_eq!(g.row_text(0), "b");
    assert_eq!(g.row_text(1), "");
  }

  #[test]
  fn erase_line_to_right() {
    let mut g = Grid::new(6, 1);
    for c in "abcdef".chars() {
      g.print(c);
    }
    g.move_to(2, 0);
    g.erase_line(0);
    assert_eq!(g.row_text(0), "ab");
  }

  #[test]
  fn delete_and_insert_chars() {
    let mut g = Grid::new(6, 1);
    for c in "abcdef".chars() {
      g.print(c);
    }
    g.move_to(1, 0);
    g.delete_chars(2);
    assert_eq!(g.row_text(0), "adef");
    g.move_to(1, 0);
    g.insert_chars(2);
    assert_eq!(g.row_text(0), "a  def");
  }

  #[test]
  fn scrollback_captures_scrolled_lines() {
    let mut g = Grid::new(8, 2);
    for c in ['1', '2', '3'] {
      g.print(c);
      g.next_line();
    }
    // Live screen: newest line on top, cleared line below.
    assert_eq!(g.view_row(0)[0].c, '3');
    assert!(g.view_at_bottom());
    // Scroll back to reveal the two captured lines.
    g.scroll_view(2);
    assert!(!g.view_at_bottom());
    assert_eq!(g.view_row(0)[0].c, '1');
    assert_eq!(g.view_row(1)[0].c, '2');
    g.scroll_to_bottom();
    assert_eq!(g.view_row(0)[0].c, '3');
  }

  #[test]
  fn sized_scale_lays_out_a_block_and_advances() {
    // `s=2` with width 0: 'X' fills a 2x2 block, cursor advances 2 cells.
    let mut g = Grid::new(8, 4);
    g.print_sized("X", TextSize::parse_str("s=2"));
    let lead = g.cell(0, 0);
    assert_eq!(lead.c, 'X');
    assert!(!lead.flags.contains(Flags::SIZED_CONT));
    let s = lead.sized.as_ref().expect("leading cell is sized");
    assert_eq!((s.cols, s.rows, s.dx, s.dy), (2, 2, 0, 0));
    // The other three cells of the block are continuations.
    for (x, y) in [(1, 0), (0, 1), (1, 1)] {
      assert!(g.cell(x, y).flags.contains(Flags::SIZED_CONT));
    }
    assert_eq!(g.cursor(), (2, 0));
  }

  #[test]
  fn sized_packed_run_round_trips_as_text() {
    // `w=1` packs the whole run into one cell; selection yields it whole.
    let mut g = Grid::new(8, 2);
    g.print_sized("ab", TextSize::parse_str("n=1:d=2:w=1"));
    assert!(g.cell(0, 0).sized.as_ref().unwrap().run.is_some());
    g.start_selection(0, 0);
    g.extend_selection(0, 0);
    assert_eq!(g.selection_text().as_deref(), Some("ab"));
    assert_eq!(g.cursor(), (1, 0));
  }

  #[test]
  fn overwriting_a_sized_block_dissolves_it() {
    let mut g = Grid::new(8, 4);
    g.print_sized("X", TextSize::parse_str("s=2"));
    // Print a normal char into a continuation cell of the block.
    g.move_to(1, 1);
    g.print('z');
    // The whole block is gone: no cell still claims to be sized.
    for y in 0..2 {
      for x in 0..2 {
        assert!(g.cell(x, y).sized.is_none(), "({x},{y}) still sized");
        assert!(!g.cell(x, y).flags.contains(Flags::SIZED_CONT));
      }
    }
    assert_eq!(g.cell(1, 1).c, 'z');
  }

  #[test]
  fn erasing_over_a_sized_block_dissolves_it() {
    let mut g = Grid::new(8, 4);
    g.print_sized("X", TextSize::parse_str("s=2"));
    // Erase row 0; the whole 2x2 block (rows 0-1) must dissolve, not just row
    // 0.
    g.move_to(0, 0);
    g.erase_line(2);
    for y in 0..2 {
      for x in 0..2 {
        assert!(g.cell(x, y).sized.is_none(), "({x},{y}) still sized");
      }
    }
  }

  #[test]
  fn deleting_chars_dissolves_a_sized_block() {
    let mut g = Grid::new(8, 2);
    g.print_sized("X", TextSize::parse_str("s=2"));
    g.move_to(0, 0);
    g.delete_chars(1);
    for y in 0..2 {
      for x in 0..2 {
        assert!(g.cell(x, y).sized.is_none(), "({x},{y}) still sized");
      }
    }
  }

  #[test]
  fn resize_dissolves_sized_runs() {
    let mut g = Grid::new(8, 4);
    g.print_sized("X", TextSize::parse_str("s=2"));
    g.resize(6, 4);
    assert!(g.cell(0, 0).sized.is_none());
    assert!(!g.cell(1, 0).flags.contains(Flags::SIZED_CONT));
  }

  #[test]
  fn wide_char_occupies_two_columns() {
    let mut g = Grid::new(6, 1);
    g.print('世');
    g.print('x');
    assert_eq!(g.cell(0, 0).c, '世');
    assert!(g.cell(1, 0).flags.contains(Flags::WIDE_CONT));
    assert_eq!(g.cell(2, 0).c, 'x');
  }

  #[test]
  fn selection_extracts_text_across_rows() {
    let mut g = Grid::new(8, 2);
    for c in "abcd".chars() {
      g.print(c);
    }
    g.carriage_return();
    g.line_feed();
    for c in "efgh".chars() {
      g.print(c);
    }
    // Select "cd" on row 0 through "ef" on row 1 (rows are live: abs 0,1).
    g.start_selection(0, 2);
    g.extend_selection(1, 1);
    assert!(g.is_selected(0, 3));
    assert!(g.is_selected(1, 0));
    assert!(!g.is_selected(1, 2));
    assert_eq!(g.selection_text().as_deref(), Some("cd\nef"));
  }

  #[test]
  fn select_word_spans_one_word() {
    let mut g = Grid::new(16, 1);
    for c in "foo bar baz".chars() {
      g.print(c);
    }
    g.select_word(0, 5); // inside "bar"
    assert_eq!(g.selection_text().as_deref(), Some("bar"));
  }

  #[test]
  fn select_word_breaks_on_delimiters_but_keeps_paths() {
    let mut g = Grid::new(32, 1);
    for c in "run /usr/bin:next".chars() {
      g.print(c);
    }
    // '/' and ':' are not delimiters, so the path selects whole.
    g.select_word(0, 7); // inside "/usr/bin:next"
    assert_eq!(g.selection_text().as_deref(), Some("/usr/bin:next"));
    // '(' is a delimiter.
    let mut g = Grid::new(16, 1);
    for c in "f(arg)".chars() {
      g.print(c);
    }
    g.select_word(0, 2); // inside "arg"
    assert_eq!(g.selection_text().as_deref(), Some("arg"));
  }

  #[test]
  fn block_selection_is_rectangular() {
    let mut g = Grid::new(8, 3);
    for line in ["abcd", "efgh", "ijkl"] {
      for c in line.chars() {
        g.print(c);
      }
      g.carriage_return();
      g.line_feed();
    }
    // A block from (row0,col1) to (row2,col2) takes columns 1..=2 each row.
    g.start_block_selection(0, 1);
    g.extend_selection(2, 2);
    assert!(g.is_selected(0, 1));
    assert!(g.is_selected(2, 2));
    assert!(!g.is_selected(1, 3));
    assert!(!g.is_selected(1, 0));
    assert_eq!(g.selection_text().as_deref(), Some("bc\nfg\njk"));
  }

  #[test]
  fn search_finds_matches_across_history() {
    let mut g = Grid::new(16, 2);
    for line in ["alpha", "beta", "ALPHA", "gamma"] {
      for c in line.chars() {
        g.print(c);
      }
      g.carriage_return();
      g.line_feed();
    }
    // Case-insensitive (no uppercase in query) matches both alphas.
    g.set_search("alpha");
    assert_eq!(g.search_count(), (2, 2)); // focus starts on the latest hit
    let spans0 = g.search_spans_on(0);
    assert_eq!(spans0, vec![(0, 4, false)]);
    // Smart case: an uppercase letter restricts to the exact-case hit.
    g.set_search("ALPHA");
    assert_eq!(g.search_count(), (1, 1));
    assert_eq!(g.search_spans_on(2), vec![(0, 4, true)]);
    // Stepping wraps and the no-match query reports zero.
    g.set_search("zzz");
    assert_eq!(g.search_count(), (0, 0));
    assert_eq!(g.search_query(), Some("zzz"));
    g.clear_search();
    assert_eq!(g.search_query(), None);
  }

  #[test]
  fn text_dump_joins_rows() {
    let mut g = Grid::new(8, 2);
    for line in ["one", "two", "three"] {
      for c in line.chars() {
        g.print(c);
      }
      g.carriage_return();
      g.line_feed();
    }
    // Scrollback dump spans history plus the live screen; trailing blank rows
    // are trimmed. The visible dump is just what is on screen.
    assert_eq!(g.scrollback_text(), "one\ntwo\nthree");
    assert_eq!(g.visible_text(), "three");
  }

  #[test]
  fn regex_search_matches_patterns() {
    let mut g = Grid::new(16, 2);
    g.set_search_regex(true);
    for line in ["alpha1", "beta22", "gamma3"] {
      for c in line.chars() {
        g.print(c);
      }
      g.carriage_return();
      g.line_feed();
    }
    // A digit class matches the run of digits on each line.
    g.set_search("[0-9]+");
    assert_eq!(g.search_count(), (3, 3));
    // "22" on row 1 spans two columns; "alpha1" digit is a single column.
    assert_eq!(g.search_spans_on(1), vec![(4, 5, false)]);
    assert_eq!(g.search_spans_on(0), vec![(5, 5, false)]);
    // An invalid pattern yields no matches rather than panicking.
    g.set_search("[unterminated");
    assert_eq!(g.search_count(), (0, 0));
  }

  #[test]
  fn reflow_rewraps_a_wrapped_paragraph() {
    let mut g = Grid::new(4, 3);
    for c in "abcdefgh".chars() {
      g.print(c); // wraps: "abcd" | "efgh" | (cursor parked)
    }
    // Widen: the two wrapped rows rejoin into one logical line.
    g.resize(8, 3);
    assert_eq!(g.row_text(0), "abcdefgh");
    assert_eq!(g.cursor(), (7, 0)); // cursor follows the content end
    // Narrow back: it rewraps to the smaller width without losing text.
    g.resize(4, 3);
    assert_eq!(g.row_text(0), "abcd");
    assert_eq!(g.row_text(1), "efgh");
  }

  #[test]
  fn reflow_preserves_hard_breaks() {
    let mut g = Grid::new(10, 3);
    for c in "one".chars() {
      g.print(c);
    }
    g.carriage_return();
    g.line_feed();
    for c in "two".chars() {
      g.print(c);
    }
    // A hard newline must not be rejoined when the width changes.
    g.resize(6, 3);
    assert_eq!(g.row_text(0), "one");
    assert_eq!(g.row_text(1), "two");
  }

  #[test]
  fn scroll_region_limits_line_feed() {
    let mut g = Grid::new(4, 4);
    g.set_scroll_region(1, 2);
    g.move_to(0, 2); // bottom of region (origin off: absolute row 2)
    g.print('x');
    g.move_to(0, 2);
    g.line_feed(); // at region bottom: scroll [1,2] up, x moves to row 1
    assert_eq!(g.row_text(1), "x");
    assert_eq!(g.row_text(2), "");
  }
}
