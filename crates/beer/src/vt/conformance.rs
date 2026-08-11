//! Golden escape-sequence tests: feed bytes, assert the resulting grid state.
//!
//! These cover the ground `vttest` and `esctest` exercise interactively
//! (cursor movement, erase/edit, scroll regions, left/right margins,
//! rectangular ops, origin mode, tab stops, SGR, character sets, autowrap, alt
//! screen, line drawing) at the model level, since the project tests the grid
//! rather than pixels. A live `vttest`/`esctest` run against a real window is
//! still the acceptance check for rendering.

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
fn ris_resets_screen_and_modes() {
  let mut t = Term::new(10, 3);
  feed(&mut t, b"\x1b[?25l"); // hide cursor
  feed(&mut t, b"\x1b[4h"); // insert mode
  feed(&mut t, b"\x1b[3g"); // clear all tab stops
  feed(&mut t, b"\x1b[?1049h"); // alternate screen
  feed(&mut t, b"hello");
  feed(&mut t, b"\x1bc"); // RIS
  assert!(t.grid().cursor_visible());
  assert!(!t.grid().insert());
  assert!(!t.grid().alt_active());
  assert_eq!(t.grid().cursor(), (0, 0));
  assert_eq!(t.grid().row_text(0), "");
  // Default tab stops are restored: HT from home lands on column 8.
  feed(&mut t, b"\t");
  assert_eq!(t.grid().cursor().0, 8);
}

#[test]
fn decaln_fills_screen_with_e() {
  let t = term(4, 2, b"\x1b[2;3H\x1b#8");
  assert_eq!(t.grid().row_text(0), "EEEE");
  assert_eq!(t.grid().row_text(1), "EEEE");
  // The cursor homes.
  assert_eq!(t.grid().cursor(), (0, 0));
}

#[test]
fn rep_repeats_last_character() {
  // REP repeats the preceding graphic character Ps times.
  let t = term(10, 1, b"X\x1b[3b");
  assert_eq!(t.grid().row_text(0), "XXXX");

  // With no preceding graphic character REP is a no-op.
  let t = term(10, 1, b"\x1b[3b");
  assert_eq!(t.grid().row_text(0), "");
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
fn uk_charset_maps_pound_sign() {
  // Designate the UK national set into G0; `#` prints as the pound sign.
  let t = term(10, 1, b"\x1b(A#");
  assert_eq!(t.grid().cell(0, 0).c, '£');
}

#[test]
fn single_shift_selects_g3_for_one_char() {
  // G3 = DEC special graphics; SS3 shifts only the next character into it.
  let t = term(10, 1, b"\x1b+0\x1bOqq");
  assert_eq!(t.grid().cell(0, 0).c, '\u{2500}'); // shifted
  assert_eq!(t.grid().cell(1, 0).c, 'q'); // reverted
}

#[test]
fn locking_shift_g2_persists() {
  // G2 = DEC special graphics; LS2 locks GL to it until SI returns to G0.
  let t = term(10, 1, b"\x1b*0\x1bnqq\x0fq");
  assert_eq!(t.grid().cell(0, 0).c, '\u{2500}');
  assert_eq!(t.grid().cell(1, 0).c, '\u{2500}');
  assert_eq!(t.grid().cell(2, 0).c, 'q');
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
fn decrqss_reports_settings() {
  // DECSCUSR: set a steady underline cursor (code 4), then query `SP q`.
  let mut t = Term::new(20, 5);
  feed(&mut t, b"\x1b[4 q");
  feed(&mut t, b"\x1bP$q q\x1b\\");
  assert_eq!(t.take_response(), b"\x1bP1$r4 q\x1b\\");

  // DECSTBM: a 2..4 region (1-based) reports as `2;4r`.
  feed(&mut t, b"\x1b[2;4r");
  feed(&mut t, b"\x1bP$qr\x1b\\");
  assert_eq!(t.take_response(), b"\x1bP1$r2;4r\x1b\\");

  // SGR: bold + indexed foreground round-trips through a `m` request.
  feed(&mut t, b"\x1b[1;31m");
  feed(&mut t, b"\x1bP$qm\x1b\\");
  assert_eq!(t.take_response(), b"\x1bP1$r0;1;31m\x1b\\");

  // An unknown request is rejected with an empty response.
  feed(&mut t, b"\x1bP$qZ\x1b\\");
  assert_eq!(t.take_response(), b"\x1bP0$r\x1b\\");
}

#[test]
fn decfra_fills_rectangle() {
  // Pch=42 ('*'), rows 1..2, cols 2..3 (1-based).
  let t = term(5, 3, b"\x1b[42;1;2;2;3$x");
  assert_eq!(t.grid().cell(1, 0).c, '*');
  assert_eq!(t.grid().cell(2, 1).c, '*');
  assert_eq!(t.grid().cell(0, 0).c, ' ');
  assert_eq!(t.grid().cell(3, 0).c, ' ');
}

#[test]
fn decera_erases_rectangle() {
  let mut t = Term::new(5, 2);
  feed(&mut t, b"AAAAA\r\nAAAAA");
  feed(&mut t, b"\x1b[1;2;2;4$z"); // rows 1..2, cols 2..4
  assert_eq!(t.grid().row_text(0), "A   A");
  assert_eq!(t.grid().row_text(1), "A   A");
}

#[test]
fn deccra_copies_rectangle() {
  let mut t = Term::new(6, 3);
  feed(&mut t, b"ABCDEF");
  // Copy source rows 1..1, cols 1..3 ("ABC") to dest row 3, col 4.
  feed(&mut t, b"\x1b[1;1;1;3;1;3;4$v");
  assert_eq!(t.grid().cell(3, 2).c, 'A');
  assert_eq!(t.grid().cell(4, 2).c, 'B');
  assert_eq!(t.grid().cell(5, 2).c, 'C');
}

#[test]
fn deccara_changes_attributes() {
  use crate::grid::Flags;
  let mut t = Term::new(5, 2);
  feed(&mut t, b"abcde");
  feed(&mut t, b"\x1b[1;1;1;3;1$r"); // bold over row 1, cols 1..3
  assert!(t.grid().cell(0, 0).flags.contains(Flags::BOLD));
  assert!(t.grid().cell(2, 0).flags.contains(Flags::BOLD));
  assert!(!t.grid().cell(3, 0).flags.contains(Flags::BOLD));
  assert_eq!(t.grid().cell(0, 0).c, 'a'); // characters unchanged
}

#[test]
fn csi_s_saves_cursor_without_margins() {
  // With DECLRMM off, `CSI s` is DECSC (save) and `CSI u` restores.
  let mut t = Term::new(10, 5);
  feed(&mut t, b"\x1b[3;4H\x1b[s\x1b[1;1H\x1b[u");
  assert_eq!(t.grid().cursor(), (3, 2));
}

#[test]
fn lr_margins_confine_scroll() {
  let mut t = Term::new(6, 3);
  feed(&mut t, b"ABCDEF\r\nGHIJKL\r\nMNOPQR");
  // Enable left/right margins and set columns 2..5 (1-based): left=1, right=4.
  feed(&mut t, b"\x1b[?69h\x1b[2;5s");
  // Scroll the region up one line; only the [1,4] span moves.
  feed(&mut t, b"\x1b[S");
  assert_eq!(t.grid().row_text(0), "AHIJKF");
  assert_eq!(t.grid().row_text(1), "GNOPQL");
  assert_eq!(t.grid().row_text(2), "M    R");
}

#[test]
fn lr_margin_autowraps_at_right() {
  let mut t = Term::new(6, 3);
  // Margins left=1, right=3, in origin mode so the cursor homes inside them.
  feed(&mut t, b"\x1b[?69h\x1b[?6h\x1b[2;4s");
  feed(&mut t, b"XYZW");
  assert_eq!(t.grid().cell(1, 0).c, 'X');
  assert_eq!(t.grid().cell(3, 0).c, 'Z');
  // The next glyph wraps to the left margin on the following row.
  assert_eq!(t.grid().cell(1, 1).c, 'W');
}

#[test]
fn deckpam_sets_application_keypad() {
  let mut t = Term::new(10, 2);
  assert!(!t.grid().app_keypad());
  feed(&mut t, b"\x1b="); // DECKPAM
  assert!(t.grid().app_keypad());
  feed(&mut t, b"\x1b>"); // DECKPNM
  assert!(!t.grid().app_keypad());
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
