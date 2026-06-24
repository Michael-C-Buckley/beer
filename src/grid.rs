//! The terminal screen: a grid of styled cells, a cursor, and the editing
//! operations the VT parser drives.

use unicode_width::UnicodeWidthChar;

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
    cursor_shape: CursorShape,
    cursor_visible: bool,
    /// Cursor colour from OSC 12; `None` follows the cell under the cursor.
    cursor_color: Option<(u8, u8, u8)>,
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
            cursor_shape: CursorShape::default(),
            cursor_visible: true,
            cursor_color: None,
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
        let blank = vec![vec![Cell::default(); self.cols]; self.rows];
        self.alt_saved = Some(std::mem::replace(&mut self.lines, blank));
    }

    pub fn leave_alt_screen(&mut self) {
        if let Some(main) = self.alt_saved.take() {
            self.lines = main;
        }
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
    fn wide_char_occupies_two_columns() {
        let mut g = Grid::new(6, 1);
        g.print('世');
        g.print('x');
        assert_eq!(g.cell(0, 0).c, '世');
        assert!(g.cell(1, 0).flags.contains(Flags::WIDE_CONT));
        assert_eq!(g.cell(2, 0).c, 'x');
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
