//! Golden escape-sequence tests: feed bytes, assert the resulting grid state.
//!
//! These cover the ground `vttest` and `esctest` exercise interactively
//! (cursor movement, erase/edit, scroll regions, origin mode, tab stops, SGR,
//! autowrap, alt screen, line drawing) at the model level, since the project
//! tests the grid rather than pixels. A live `vttest`/`esctest` run against a
//! real window is still the acceptance check for rendering; the procedure and
//! the known gaps are documented in `doc/conformance.md`.

use super::*;

/// Feed `bytes` through a fresh parser at an 8x16 cell size.
fn feed(term: &mut Term, bytes: &[u8]) {
  let mut parser = vte::Parser::new();
  term.feed(&mut parser, bytes, (8, 16));
}

/// A term of the given size with `bytes` already applied.
fn term(cols: usize, rows: usize, bytes: &[u8]) -> Term {
  let mut t = Term::new(cols, rows);
  feed(&mut t, bytes);
  t
}

// --- Cursor movement (vttest menu 1) --------------------------------------

#[test]
fn cup_positions_one_based() {
  // CSI 2;3 H puts the cursor at row 2, col 3 (1-based) => (2, 1) 0-based.
  let t = term(10, 5, b"\x1b[2;3H");
  assert_eq!(t.grid().cursor(), (2, 1));
}

#[test]
fn cuu_cud_cuf_cub_move_and_clamp() {
  let mut t = Term::new(10, 5);
  feed(&mut t, b"\x1b[5;5H"); // (4, 4)
  feed(&mut t, b"\x1b[2A"); // up 2 -> row 2
  feed(&mut t, b"\x1b[1C"); // right 1 -> col 5
  assert_eq!(t.grid().cursor(), (5, 2));
  feed(&mut t, b"\x1b[100A"); // clamp to top row
  assert_eq!(t.grid().cursor().1, 0);
  feed(&mut t, b"\x1b[100D"); // clamp to first col
  assert_eq!(t.grid().cursor().0, 0);
}

#[test]
fn cha_and_vpa_set_single_axis() {
  let mut t = Term::new(20, 5);
  feed(&mut t, b"\x1b[10G"); // CHA: column 10 (1-based)
  assert_eq!(t.grid().cursor().0, 9);
  feed(&mut t, b"\x1b[3d"); // VPA: row 3 (1-based)
  assert_eq!(t.grid().cursor().1, 2);
}

#[test]
fn decsc_decrc_round_trip() {
  let mut t = Term::new(10, 5);
  feed(&mut t, b"\x1b[3;4H\x1b7"); // save at (3, 2)
  feed(&mut t, b"\x1b[1;1H"); // move to origin
  feed(&mut t, b"\x1b8"); // restore
  assert_eq!(t.grid().cursor(), (3, 2));
}

// --- Erase and edit (vttest menu 2) ---------------------------------------

#[test]
fn el_modes_clear_the_right_span() {
  // EL 0: cursor to end.
  let t = term(10, 2, b"abcdef\x1b[1;4H\x1b[0K");
  assert_eq!(t.grid().row_text(0), "abc");

  // EL 1: start to cursor (inclusive) becomes blanks.
  let t = term(10, 2, b"abcdef\x1b[1;4H\x1b[1K");
  assert_eq!(t.grid().row_text(0), "    ef");

  // EL 2: whole line.
  let t = term(10, 2, b"abcdef\x1b[1;4H\x1b[2K");
  assert_eq!(t.grid().row_text(0), "");
}

#[test]
fn ed_modes_clear_screen_regions() {
  // ED 2 clears everything.
  let t = term(10, 3, b"aaa\r\nbbb\r\nccc\x1b[2J");
  assert_eq!(t.grid().row_text(0), "");
  assert_eq!(t.grid().row_text(2), "");

  // ED 0 clears from the cursor to the end of the screen.
  let t = term(10, 3, b"aaa\r\nbbb\r\nccc\x1b[2;2H\x1b[0J");
  assert_eq!(t.grid().row_text(0), "aaa");
  assert_eq!(t.grid().row_text(1), "b");
  assert_eq!(t.grid().row_text(2), "");

  // ED 1 clears from the start of the screen to the cursor.
  let t = term(10, 3, b"aaa\r\nbbb\r\nccc\x1b[2;2H\x1b[1J");
  assert_eq!(t.grid().row_text(0), "");
  assert_eq!(t.grid().row_text(1), "  b");
  assert_eq!(t.grid().row_text(2), "ccc");
}

#[test]
fn ich_dch_ech_edit_in_line() {
  // ICH inserts blanks, shifting the tail right.
  let t = term(10, 1, b"abcdef\x1b[1;2H\x1b[2@");
  assert_eq!(t.grid().row_text(0), "a  bcdef");

  // DCH deletes, pulling the tail left.
  let t = term(10, 1, b"abcdef\x1b[1;2H\x1b[2P");
  assert_eq!(t.grid().row_text(0), "adef");

  // ECH blanks in place without shifting.
  let t = term(10, 1, b"abcdef\x1b[1;2H\x1b[2X");
  assert_eq!(t.grid().row_text(0), "a  def");
}

#[test]
fn il_dl_insert_and_delete_lines() {
  let t = term(10, 4, b"one\r\ntwo\r\nthree\x1b[1;1H\x1b[1L");
  assert_eq!(t.grid().row_text(0), "");
  assert_eq!(t.grid().row_text(1), "one");

  let t = term(10, 4, b"one\r\ntwo\r\nthree\x1b[1;1H\x1b[1M");
  assert_eq!(t.grid().row_text(0), "two");
  assert_eq!(t.grid().row_text(1), "three");
}

// --- Scroll regions and origin mode (vttest menu 1) -----------------------

#[test]
fn decstbm_confines_scrolling() {
  // Region rows 2-3. Fill it, then a line feed at the bottom scrolls only
  // within the region, leaving rows 1 and 4 untouched.
  let mut t = Term::new(10, 4);
  feed(&mut t, b"top\r\n"); // row 0
  feed(&mut t, b"\x1b[2;3r"); // DECSTBM rows 2..=3
  feed(&mut t, b"\x1b[2;1Hin2"); // row 1 (0-based)
  feed(&mut t, b"\x1b[3;1Hin3\n"); // row 2, then LF scrolls region
  feed(&mut t, b"\rin3b"); // CR back to col 0 on the new bottom row
  assert_eq!(t.grid().row_text(0), "top");
  assert_eq!(t.grid().row_text(1), "in3");
  assert_eq!(t.grid().row_text(2), "in3b");
}

#[test]
fn decom_origin_mode_is_region_relative() {
  let mut t = Term::new(10, 6);
  feed(&mut t, b"\x1b[2;4r"); // region rows 2..=4
  feed(&mut t, b"\x1b[?6h"); // origin mode on
  feed(&mut t, b"\x1b[1;1H"); // "row 1" is now region top (grid row 1)
  feed(&mut t, b"X");
  assert_eq!(t.grid().row_text(1), "X");
  // A CUP past the region bottom clamps inside it.
  feed(&mut t, b"\x1b[9;1HY");
  assert_eq!(t.grid().row_text(3), "Y");
}

// --- Tab stops (vttest menu 3) --------------------------------------------

#[test]
fn default_tab_stops_every_eight() {
  let t = term(30, 1, b"a\tb\tc");
  assert_eq!(t.grid().row_text(0), "a       b       c");
}

#[test]
fn hts_and_tbc_edit_tab_stops() {
  // Set a stop at column 4 (1-based), clear the default grid, then tab.
  let mut t = Term::new(30, 1);
  feed(&mut t, b"\x1b[3g"); // TBC 3: clear all tab stops
  feed(&mut t, b"\x1b[4G\x1bH"); // move to col 4, HTS sets a stop there
  feed(&mut t, b"\r\tX"); // CR, tab jumps to col 4, print X
  assert_eq!(t.grid().cell(3, 0).c, 'X');
}

// --- SGR attributes (vttest menu 4) ---------------------------------------

#[test]
fn sgr_sets_and_resets_flags() {
  let t = term(10, 1, b"\x1b[1;3;4mX\x1b[0mY");
  let x = t.grid().cell(0, 0);
  assert!(x.flags.contains(Flags::BOLD));
  assert!(x.flags.contains(Flags::ITALIC));
  assert_eq!(x.underline, Underline::Single);
  let y = t.grid().cell(1, 0);
  assert!(!y.flags.contains(Flags::BOLD));
  assert_eq!(y.underline, Underline::None);
}

#[test]
fn sgr_reverse_strike_overline() {
  let t = term(10, 1, b"\x1b[7;9;53mZ");
  let z = t.grid().cell(0, 0);
  assert!(z.flags.contains(Flags::REVERSE));
  assert!(z.flags.contains(Flags::STRIKE));
  assert!(z.flags.contains(Flags::OVERLINE));
}

#[test]
fn sgr_indexed_and_truecolor() {
  // 256-colour foreground (SGR 38;5;n) and truecolor background (48;2;r;g;b).
  let t = term(10, 1, b"\x1b[38;5;196;48;2;10;20;30mC");
  let c = t.grid().cell(0, 0);
  assert_eq!(c.fg, Color::Indexed(196));
  assert_eq!(c.bg, Color::Rgb(10, 20, 30));
}

#[test]
fn sgr_styled_underline_and_colour() {
  // 4:3 curly underline, 58;5;n underline colour.
  let t = term(10, 1, b"\x1b[4:3;58;5;42mU");
  let u = t.grid().cell(0, 0);
  assert_eq!(u.underline, Underline::Curly);
  assert_eq!(u.underline_color, Color::Indexed(42));
}

// --- Autowrap, line drawing, alt screen -----------------------------------

#[test]
fn autowrap_wraps_at_the_margin() {
  // Six columns, seven characters: the seventh wraps to the next row.
  let t = term(6, 3, b"abcdefg");
  assert_eq!(t.grid().row_text(0), "abcdef");
  assert_eq!(t.grid().row_text(1), "g");
}

#[test]
fn autowrap_off_overprints_last_column() {
  let t = term(6, 3, b"\x1b[?7labcdefg");
  // With autowrap off the tail overprints the final column in place.
  assert_eq!(t.grid().row_text(0), "abcdeg");
  assert_eq!(t.grid().row_text(1), "");
}

#[test]
fn dec_special_graphics_maps_line_drawing() {
  // Designate DEC special graphics into G0, print 'q' (horizontal line),
  // then return to ASCII.
  let t = term(10, 1, b"\x1b(0q\x1b(B");
  assert_eq!(t.grid().cell(0, 0).c, '\u{2500}');
}

#[test]
fn alt_screen_swaps_and_restores() {
  let mut t = Term::new(10, 2);
  feed(&mut t, b"main");
  feed(&mut t, b"\x1b[?1049h\x1b[H"); // enter alt screen, home the cursor
  assert_eq!(t.grid().row_text(0), "");
  feed(&mut t, b"alt");
  assert_eq!(t.grid().row_text(0), "alt");
  feed(&mut t, b"\x1b[?1049l"); // leave alt screen
  assert_eq!(t.grid().row_text(0), "main");
}

#[test]
fn ind_ri_nel_move_between_lines() {
  let mut t = Term::new(10, 3);
  feed(&mut t, b"\x1b[2;1HX"); // row 1, col 0 -> print X, cursor at col 1
  feed(&mut t, b"\x1bM"); // RI: up one line
  assert_eq!(t.grid().cursor().1, 0);
  feed(&mut t, b"\x1bD"); // IND: down one line
  assert_eq!(t.grid().cursor().1, 1);
  feed(&mut t, b"\x1bE"); // NEL: down and to column 0
  assert_eq!(t.grid().cursor(), (0, 2));
}
