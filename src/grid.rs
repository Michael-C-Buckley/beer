//! The terminal screen: a grid of styled cells, a cursor, and the editing
//! operations the VT parser drives.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

/// Maximum scrollback lines retained for the main screen.
const SCROLLBACK_CAP: usize = 10_000;

/// A cell colour: terminal default, a palette index, or direct RGB.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

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

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

/// Underline style (SGR 4 / 4:x / 21).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Cursor shape (DECSCUSR).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
}

/// One grid cell: a character plus its rendering style.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
    pub underline: Underline,
    /// Underline colour; `Default` means "follow the foreground".
    pub underline_color: Color,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: Flags::empty(),
            underline: Underline::None,
            underline_color: Color::Default,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
    x: usize,
    y: usize,
}

/// A point in the combined scrollback+live coordinate space: `row` indexes
/// scrollback lines first (oldest at 0), then the live screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Point {
    pub row: usize,
    pub col: usize,
}

/// The active screen plus cursor, scroll region, and current pen.
#[derive(Debug)]
pub struct Grid {
    cols: usize,
    rows: usize,
    lines: Vec<Vec<Cell>>,
    cursor: Cursor,
    saved: Cursor,
    /// Inclusive top/bottom rows of the scroll region.
    top: usize,
    bottom: usize,
    /// Template cell carrying the current SGR colours/flags.
    pen: Cell,
    autowrap: bool,
    origin: bool,
    insert: bool,
    /// Cursor parked past the last column, awaiting the next print to wrap.
    wrap_pending: bool,
    tabs: Vec<bool>,
    /// Saved primary screen while the alternate screen is active.
    alt_saved: Option<Vec<Vec<Cell>>>,
    /// Lines that have scrolled off the top of the main screen, newest last.
    scrollback: VecDeque<Vec<Cell>>,
    /// How many lines the viewport is scrolled back from the live bottom.
    view_offset: usize,
    cursor_shape: CursorShape,
    /// Whether the cursor shape is a blinking variant (DECSCUSR odd codes).
    cursor_blink: bool,
    cursor_visible: bool,
    /// Application cursor-keys mode (DECCKM): arrows send SS3 instead of CSI.
    app_cursor: bool,
    /// Cursor colour from OSC 12; `None` follows the cell under the cursor.
    cursor_color: Option<(u8, u8, u8)>,
    /// Active mouse selection as (anchor, head) in absolute coordinates.
    selection: Option<(Point, Point)>,
    /// Bracketed paste mode (DECSET 2004): wrap pasted text in `ESC[200~`/`201~`.
    bracketed_paste: bool,
    /// Synchronized output (DECSET 2026): hold presentation while a frame is
    /// being assembled, so the screen never shows a half-drawn update.
    sync: bool,
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0 && i != 0).collect()
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            lines: vec![vec![Cell::default(); cols]; rows],
            cursor: Cursor::default(),
            saved: Cursor::default(),
            top: 0,
            bottom: rows - 1,
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
            selection: None,
            bracketed_paste: false,
            sync: false,
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Resize the screen, clipping content (reflow comes later).
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        for line in &mut self.lines {
            line.resize(cols, Cell::default());
        }
        self.lines.resize(rows, vec![Cell::default(); cols]);
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.tabs = default_tabs(cols);
        self.cursor.x = self.cursor.x.min(cols - 1);
        self.cursor.y = self.cursor.y.min(rows - 1);
        self.wrap_pending = false;
        // Scrollback lines keep their old width (no reflow); snap to the live
        // screen so the viewport never indexes a stale-width line.
        self.view_offset = 0;
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor.x, self.cursor.y)
    }

    // --- pen / attributes ---

    pub fn pen_mut(&mut self) -> &mut Cell {
        &mut self.pen
    }

    pub fn reset_pen(&mut self) {
        self.pen = Cell::default();
    }

    pub fn set_autowrap(&mut self, on: bool) {
        self.autowrap = on;
    }

    pub fn set_origin(&mut self, on: bool) {
        self.origin = on;
        self.move_to(0, 0);
    }

    pub fn set_insert(&mut self, on: bool) {
        self.insert = on;
    }

    pub fn autowrap(&self) -> bool {
        self.autowrap
    }

    pub fn origin(&self) -> bool {
        self.origin
    }

    pub fn insert(&self) -> bool {
        self.insert
    }

    pub fn alt_active(&self) -> bool {
        self.alt_saved.is_some()
    }

    pub fn set_cursor_shape(&mut self, shape: CursorShape) {
        self.cursor_shape = shape;
    }

    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    pub fn set_cursor_blink(&mut self, blink: bool) {
        self.cursor_blink = blink;
    }

    pub fn cursor_blink(&self) -> bool {
        self.cursor_blink
    }

    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn set_cursor_color(&mut self, color: Option<(u8, u8, u8)>) {
        self.cursor_color = color;
    }

    pub fn cursor_color(&self) -> Option<(u8, u8, u8)> {
        self.cursor_color
    }

    pub fn set_app_cursor(&mut self, on: bool) {
        self.app_cursor = on;
    }

    pub fn app_cursor(&self) -> bool {
        self.app_cursor
    }

    // --- printing ---

    /// Place a printable character at the cursor, honouring width and autowrap.
    pub fn print(&mut self, c: char) {
        let width = c.width().unwrap_or(0);
        if width == 0 {
            // Combining/zero-width: no standalone storage yet.
            return;
        }
        if self.wrap_pending {
            self.cursor.x = 0;
            self.line_feed();
            self.wrap_pending = false;
        }
        if width == 2 && self.cursor.x + 1 >= self.cols {
            // A double-width glyph cannot straddle the right edge: wrap first.
            if self.autowrap {
                self.cursor.x = 0;
                self.line_feed();
            } else {
                return;
            }
        }
        if self.insert {
            self.shift_right(width);
        }

        let (x, y) = (self.cursor.x, self.cursor.y);
        let mut cell = self.pen.clone();
        cell.c = c;
        cell.flags.remove(Flags::WIDE_CONT);
        self.lines[y][x] = cell;
        if width == 2 && x + 1 < self.cols {
            let mut cont = self.pen.clone();
            cont.c = ' ';
            cont.flags.insert(Flags::WIDE_CONT);
            self.lines[y][x + 1] = cont;
        }

        let advance = width;
        if self.cursor.x + advance >= self.cols {
            if self.autowrap {
                self.cursor.x = self.cols - 1;
                self.wrap_pending = true;
            } else {
                self.cursor.x = self.cols - 1;
            }
        } else {
            self.cursor.x += advance;
        }
    }

    fn shift_right(&mut self, n: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let end = self.cols;
        let blank = self.pen_blank();
        let row = &mut self.lines[y];
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

    // --- cursor movement ---

    fn region(&self) -> (usize, usize) {
        if self.origin {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        }
    }

    pub fn move_to(&mut self, x: usize, y: usize) {
        let (rt, rb) = self.region();
        self.cursor.x = x.min(self.cols - 1);
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

    pub fn cursor_back(&mut self, n: usize) {
        self.cursor.x = self.cursor.x.saturating_sub(n);
        self.wrap_pending = false;
    }

    pub fn save_cursor(&mut self) {
        self.saved = self.cursor;
    }

    pub fn restore_cursor(&mut self) {
        self.cursor = self.saved;
        self.cursor.x = self.cursor.x.min(self.cols - 1);
        self.cursor.y = self.cursor.y.min(self.rows - 1);
        self.wrap_pending = false;
    }

    // --- line discipline ---

    pub fn carriage_return(&mut self) {
        self.cursor.x = 0;
        self.wrap_pending = false;
    }

    pub fn backspace(&mut self) {
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

    // --- tab stops ---

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

    // --- scrolling within the region ---

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

    pub fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top + 1);
        // Lines leaving the top of the *whole* main screen become scrollback;
        // a DECSTBM region scroll (top > 0) or the alt screen does not.
        if self.top == 0 && self.alt_saved.is_none() {
            for y in 0..n {
                let line = std::mem::replace(&mut self.lines[y], vec![Cell::default(); self.cols]);
                self.scrollback.push_back(line);
            }
            let mut evicted = 0;
            while self.scrollback.len() > SCROLLBACK_CAP {
                self.scrollback.pop_front();
                evicted += 1;
            }
            if evicted > 0 {
                self.shift_selection(evicted);
            }
            // Keep a scrolled-back viewport anchored to the same content.
            if self.view_offset > 0 {
                self.view_offset = (self.view_offset + n).min(self.scrollback.len());
            }
        }
        for y in self.top..=self.bottom {
            if y + n <= self.bottom {
                self.lines.swap(y, y + n);
            }
        }
        for y in (self.bottom + 1 - n)..=self.bottom {
            self.blank_row(y);
        }
    }

    pub fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top + 1);
        for y in (self.top..=self.bottom).rev() {
            if y >= self.top + n {
                self.lines.swap(y, y - n);
            }
        }
        for y in self.top..(self.top + n) {
            self.blank_row(y);
        }
    }

    fn blank_row(&mut self, y: usize) {
        let blank = self.pen_blank();
        for cell in &mut self.lines[y] {
            *cell = blank.clone();
        }
    }

    // --- erase ---

    /// ED: 0=below, 1=above, 2/3=all.
    pub fn erase_display(&mut self, mode: u16) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        match mode {
            0 => {
                self.erase_in_row(y, x, self.cols);
                for r in (y + 1)..self.rows {
                    self.blank_row(r);
                }
            }
            1 => {
                for r in 0..y {
                    self.blank_row(r);
                }
                self.erase_in_row(y, 0, x + 1);
            }
            _ => {
                for r in 0..self.rows {
                    self.blank_row(r);
                }
            }
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
        let blank = self.pen_blank();
        for cell in &mut self.lines[y][from..to.min(self.cols)] {
            *cell = blank.clone();
        }
    }

    // --- intra-line editing ---

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
        let n = n.min(self.cols - x);
        let blank = self.pen_blank();
        let row = &mut self.lines[y];
        for i in x..self.cols {
            row[i] = if i + n < self.cols {
                row[i + n].clone()
            } else {
                blank.clone()
            };
        }
    }

    /// IL: insert n blank lines at the cursor row, within the scroll region.
    pub fn insert_lines(&mut self, n: usize) {
        if self.cursor.y < self.top || self.cursor.y > self.bottom {
            return;
        }
        let n = n.min(self.bottom - self.cursor.y + 1);
        for y in (self.cursor.y..=self.bottom).rev() {
            if y >= self.cursor.y + n {
                self.lines.swap(y, y - n);
            }
        }
        for y in self.cursor.y..(self.cursor.y + n) {
            self.blank_row(y);
        }
    }

    /// DL: delete n lines at the cursor row, within the scroll region.
    pub fn delete_lines(&mut self, n: usize) {
        if self.cursor.y < self.top || self.cursor.y > self.bottom {
            return;
        }
        let n = n.min(self.bottom - self.cursor.y + 1);
        for y in self.cursor.y..=self.bottom {
            if y + n <= self.bottom {
                self.lines.swap(y, y + n);
            }
        }
        for y in (self.bottom + 1 - n)..=self.bottom {
            self.blank_row(y);
        }
    }

    // --- alternate screen ---

    pub fn enter_alt_screen(&mut self) {
        if self.alt_saved.is_some() {
            return;
        }
        self.view_offset = 0;
        let blank = vec![vec![Cell::default(); self.cols]; self.rows];
        self.alt_saved = Some(std::mem::replace(&mut self.lines, blank));
    }

    pub fn leave_alt_screen(&mut self) {
        if let Some(main) = self.alt_saved.take() {
            self.lines = main;
        }
    }

    // --- scrollback viewport ---

    /// Scroll the viewport by `delta` lines: positive = back into history,
    /// negative = toward the live screen. No-op on the alternate screen.
    pub fn scroll_view(&mut self, delta: isize) {
        if self.alt_saved.is_some() {
            return;
        }
        let max = self.scrollback.len() as isize;
        self.view_offset = (self.view_offset as isize + delta).clamp(0, max) as usize;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.view_offset = 0;
    }

    /// Whether the viewport is showing the live screen (not scrolled back).
    pub fn view_at_bottom(&self) -> bool {
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
            &self.scrollback[idx]
        } else {
            &self.lines[idx - self.scrollback.len()]
        }
    }

    // --- selection ---

    /// The absolute row currently shown at viewport row `y`.
    pub fn view_to_abs(&self, y: usize) -> usize {
        self.scrollback.len() - self.view_offset + y
    }

    /// Cells of an absolute row (scrollback first, then the live screen).
    fn abs_row(&self, row: usize) -> &[Cell] {
        if row < self.scrollback.len() {
            &self.scrollback[row]
        } else {
            &self.lines[row - self.scrollback.len()]
        }
    }

    /// Slide an active selection up by `n` rows after scrollback eviction,
    /// dropping it if either endpoint scrolled off the top.
    fn shift_selection(&mut self, n: usize) {
        if let Some((a, b)) = self.selection {
            if a.row < n || b.row < n {
                self.selection = None;
            } else {
                self.selection = Some((
                    Point {
                        row: a.row - n,
                        ..a
                    },
                    Point {
                        row: b.row - n,
                        ..b
                    },
                ));
            }
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Begin a selection at an absolute point (drag anchor).
    pub fn start_selection(&mut self, row: usize, col: usize) {
        let p = Point { row, col };
        self.selection = Some((p, p));
    }

    /// Move the selection head (drag), keeping the anchor fixed.
    pub fn extend_selection(&mut self, row: usize, col: usize) {
        if let Some((_, head)) = self.selection.as_mut() {
            *head = Point { row, col };
        }
    }

    /// Select the whitespace-delimited word at an absolute point.
    pub fn select_word(&mut self, row: usize, col: usize) {
        let line = self.abs_row(row);
        let word = |c: char| !c.is_whitespace();
        if col >= line.len() || !word(line[col].c) {
            self.start_selection(row, col);
            return;
        }
        let mut lo = col;
        while lo > 0 && word(line[lo - 1].c) {
            lo -= 1;
        }
        let mut hi = col;
        while hi + 1 < line.len() && word(line[hi + 1].c) {
            hi += 1;
        }
        self.selection = Some((Point { row, col: lo }, Point { row, col: hi }));
    }

    /// Select the whole line at an absolute row.
    pub fn select_line(&mut self, row: usize) {
        let last = self.abs_row(row).len().saturating_sub(1);
        self.selection = Some((Point { row, col: 0 }, Point { row, col: last }));
    }

    /// Normalized selection (start <= end in reading order), if any.
    fn ordered_selection(&self) -> Option<(Point, Point)> {
        self.selection.map(|(a, b)| {
            if (a.row, a.col) <= (b.row, b.col) {
                (a, b)
            } else {
                (b, a)
            }
        })
    }

    /// Whether the cell at an absolute `(row, col)` falls inside the selection.
    pub fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some((start, end)) = self.ordered_selection() else {
            return false;
        };
        if row < start.row || row > end.row {
            return false;
        }
        let lo = if row == start.row { start.col } else { 0 };
        let hi = if row == end.row { end.col } else { usize::MAX };
        col >= lo && col <= hi
    }

    /// The inclusive `(lo, hi)` column span selected on absolute row `row`, if
    /// any part of that row is selected.
    pub fn selection_span_on(&self, row: usize) -> Option<(usize, usize)> {
        let (start, end) = self.ordered_selection()?;
        if row < start.row || row > end.row {
            return None;
        }
        let lo = if row == start.row { start.col } else { 0 };
        let hi = if row == end.row {
            end.col
        } else {
            self.abs_row(row).len().saturating_sub(1)
        };
        Some((lo, hi))
    }

    /// The selected text, with trailing blanks trimmed per line and rows joined
    /// by newlines. `None` if there is no selection.
    pub fn selection_text(&self) -> Option<String> {
        let (start, end) = self.ordered_selection()?;
        let mut out = String::new();
        for row in start.row..=end.row {
            let line = self.abs_row(row);
            let lo = if row == start.row { start.col } else { 0 };
            let hi = if row == end.row {
                (end.col + 1).min(line.len())
            } else {
                line.len()
            };
            let text: String = line
                .get(lo..hi)
                .unwrap_or(&[])
                .iter()
                .filter(|c| !c.flags.contains(Flags::WIDE_CONT))
                .map(|c| c.c)
                .collect();
            out.push_str(text.trim_end());
            if row != end.row {
                out.push('\n');
            }
        }
        Some(out)
    }

    pub fn set_bracketed_paste(&mut self, on: bool) {
        self.bracketed_paste = on;
    }

    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    pub fn set_sync(&mut self, on: bool) {
        self.sync = on;
    }

    pub fn sync_active(&self) -> bool {
        self.sync
    }

    // --- inspection (logging + tests) ---

    /// The visible text of one row, trailing blanks trimmed.
    #[cfg(test)]
    pub fn row_text(&self, y: usize) -> String {
        self.lines[y]
            .iter()
            .filter(|c| !c.flags.contains(Flags::WIDE_CONT))
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    pub fn cell(&self, x: usize, y: usize) -> &Cell {
        &self.lines[y][x]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
