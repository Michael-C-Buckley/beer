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
fn detects_web_mail_and_phone_links() {
  let mut g = Grid::new(80, 2);
  for c in "www.example.com mailto:a@example.com tel:+15551212".chars() {
    g.print(c);
  }
  let urls: Vec<String> =
    g.visible_urls().into_iter().map(|hit| hit.url).collect();
  assert_eq!(urls, [
    "https://www.example.com",
    "mailto:a@example.com",
    "tel:+15551212",
  ]);
}

#[test]
fn extended_graphemes_stay_in_one_cell() {
  let mut g = Grid::new(8, 1);
  for c in "👩‍💻X".chars() {
    g.print(c);
  }
  assert_eq!(g.cell(0, 0).c, '👩');
  assert_eq!(g.cell(0, 0).combining.as_deref(), Some("\u{200d}💻"));
  assert_eq!(g.cell(2, 0).c, 'X');
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
fn search_and_text_cross_soft_wraps() {
  let mut g = Grid::new(4, 2);
  for c in "abcdef".chars() {
    g.print(c);
  }
  g.set_search("def");
  assert_eq!(g.search_count(), (1, 1));
  assert_eq!(g.search_spans_on(0), vec![(3, 3, true)]);
  assert_eq!(g.search_spans_on(1), vec![(0, 1, true)]);
  assert_eq!(g.visible_text(), "abcdef");
  g.start_selection(0, 2);
  g.extend_selection(1, 1);
  assert_eq!(g.selection_text().as_deref(), Some("cdef"));
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
