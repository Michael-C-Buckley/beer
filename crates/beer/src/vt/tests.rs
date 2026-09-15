use beer_protocols::codec::base64_encode;

use super::*;
use crate::grid::{Flags, MouseEncoding, MouseProtocol};

fn feed(term: &mut Term, bytes: &[u8]) {
  let mut parser = vte::Parser::new();
  term.feed(&mut parser, bytes, (8, 16));
}

#[test]
fn plain_text_lands_in_the_grid() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"hello");
  assert_eq!(t.grid().row_text(0), "hello");
}

#[test]
fn cursor_position_and_erase() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"abcde\x1b[Hxyz");
  assert_eq!(t.grid().row_text(0), "xyzde");
}

#[test]
fn newline_sequence() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"one\r\ntwo");
  assert_eq!(t.grid().row_text(0), "one");
  assert_eq!(t.grid().row_text(1), "two");
}

#[test]
fn kitty_graphics_apc_transmits_and_displays() {
  // ESC _ G a=T,f=32,s=2,v=2,i=1 ; <base64 RGBA> ESC \: a 2x2 image,
  // transmitted and displayed at the cursor.
  let mut t = Term::new(20, 4);
  let px = vec![0xFFu8; 2 * 2 * 4];
  let b64 = base64_encode(&px);
  let seq = format!("\x1b_Ga=T,f=32,s=2,v=2,i=1;{b64}\x1b\\");
  feed(&mut t, seq.as_bytes());
  // With an 8x16 cell the 2x2 image occupies one cell, stamped at (0,0).
  let cell = t.grid().cell(0, 0);
  assert_eq!(cell.image.map(|r| r.image), Some(1));
  let resp = t.take_response();
  assert!(resp.windows(2).any(|w| w == b"OK"), "expected OK response");
}

#[test]
fn kitty_graphics_scrolls_to_fit_image() {
  // 20x4 terminal, cursor at the last row. An image 3 cells tall needs
  // 3 rows; only 1 is available, so the grid should scroll up 2 rows to
  // make room, then place the image in the bottom 3 rows.
  let mut t = Term::new(20, 4);
  // Move cursor to last row.
  feed(&mut t, b"\x1b[4;1H"); // CSI 4;1 H = row 4, col 1 (1-based)
  assert_eq!(t.grid().cursor(), (0, 3));

  // A 16x48 RGBA image: 2 cells wide, 3 cells tall at 8x16 cell size.
  let px = vec![0xFFu8; 16 * 48 * 4];
  let b64 = base64_encode(&px);
  let seq = format!("\x1b_Ga=T,f=32,s=16,v=48,i=2;{b64}\x1b\\");
  feed(&mut t, seq.as_bytes());

  // The grid should have scrolled 2 rows; cursor is now on row 3 (last),
  // and image rows start at row 1 (dy=0 there, dy=2 at row 3).
  let top_cell = t.grid().cell(0, 1);
  assert_eq!(
    top_cell.image.map(|r| (r.image, r.dy)),
    Some((2, 0)),
    "top row of image should be at grid row 1 after scroll"
  );
  let bot_cell = t.grid().cell(0, 3);
  assert_eq!(
    bot_cell.image.map(|r| (r.image, r.dy)),
    Some((2, 2)),
    "bottom row of image should be at grid row 3"
  );
}

#[test]
fn reports_pixel_geometry_for_graphics_clients() {
  // The test harness feeds with an 8x16 cell. A 20x4 grid is then 160x64
  // pixels. These answers are what an image client needs to size images.
  let mut t = Term::new(20, 4);
  feed(&mut t, b"\x1b[16t"); // cell size in pixels
  assert_eq!(t.take_response(), b"\x1b[6;16;8t");
  feed(&mut t, b"\x1b[14t"); // text area in pixels
  assert_eq!(t.take_response(), b"\x1b[4;64;160t");
  feed(&mut t, b"\x1b[18t"); // text area in cells
  assert_eq!(t.take_response(), b"\x1b[8;4;20t");
}

#[test]
fn kitty_unicode_placeholder_virtual_placement() {
  // Transmit + a virtual placement (U=1): no cells are stamped, but the
  // placement is registered for placeholder cells to reference.
  let mut t = Term::new(20, 4);
  let px = base64_encode(&[0xFF; 4]);
  let seq = format!("\x1b_Ga=T,U=1,i=7,c=1,r=1,f=32,s=1,v=1;{px}\x1b\\");
  feed(&mut t, seq.as_bytes());
  assert!(
    t.grid().cell(0, 0).image.is_none(),
    "virtual placement stamps nothing"
  );
  assert!(t.graphics().placement(7, 0).is_some());
  // The app prints a placeholder carrying image id 7 in its fg colour.
  feed(&mut t, "\x1b[38;5;7m\u{10EEEE}\u{0305}\u{0305}".as_bytes());
  assert_eq!(t.grid().cell(0, 0).c, '\u{10EEEE}');
}

#[test]
fn apc_does_not_disturb_surrounding_text() {
  // Text, then a graphics query APC, then more text: the text is intact and
  // the APC did not leak bytes into the grid.
  let mut t = Term::new(20, 2);
  let px = base64_encode(&[0u8; 4]);
  let seq = format!("ab\x1b_Ga=q,f=32,s=1,v=1,i=2;{px}\x1b\\cd");
  feed(&mut t, seq.as_bytes());
  assert_eq!(t.grid().row_text(0), "abcd");
}

#[test]
fn text_sizing_osc66_lays_out_a_scaled_block() {
  // `OSC 66 ; s=2 ; X BEL`: a 2x2 scaled block, cursor advances two cells.
  let mut t = Term::new(20, 4);
  feed(&mut t, b"\x1b]66;s=2;X\x07");
  let g = t.grid();
  assert_eq!(g.cell(0, 0).c, 'X');
  let s = g.cell(0, 0).sized.as_ref().expect("leading cell is scaled");
  assert_eq!((s.cols, s.rows), (2, 2));
  assert!(g.cell(1, 1).flags.contains(Flags::SIZED_CONT));
  assert_eq!(g.cursor(), (2, 0));
}

#[test]
fn device_attributes_levels() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"\x1b[c");
  assert_eq!(t.take_response(), b"\x1b[?62;22c");
  feed(&mut t, b"\x1b[>c");
  assert_eq!(t.take_response(), b"\x1b[>0;276;0c");
  feed(&mut t, b"\x1b[=c");
  assert_eq!(t.take_response(), b"\x1bP!|00000000\x1b\\");
}

#[test]
fn xtversion_reports_name() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"\x1b[>q");
  let resp = t.take_response();
  assert!(resp.starts_with(b"\x1bP>|beer("));
  assert!(resp.ends_with(b")\x1b\\"));
}

#[test]
fn decrqm_reports_known_modes() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"\x1b[?7$p"); // autowrap, on by default
  assert_eq!(t.take_response(), b"\x1b[?7;1$y");
  feed(&mut t, b"\x1b[?7l\x1b[?7$p"); // turn it off, re-query
  assert_eq!(t.take_response(), b"\x1b[?7;2$y");
  feed(&mut t, b"\x1b[?9999$p"); // unknown mode
  assert_eq!(t.take_response(), b"\x1b[?9999;0$y");
}

#[test]
fn sgr_underline_styles_and_lines() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b[4:3;58;5;1;53mX");
  let cell = t.grid().cell(0, 0);
  assert_eq!(cell.underline, Underline::Curly);
  assert_eq!(cell.underline_color, Color::Indexed(1));
  assert!(cell.flags.contains(Flags::OVERLINE));
  // 4:0 turns the underline back off.
  feed(&mut t, b"\x1b[4:0mY");
  assert_eq!(t.grid().cell(1, 0).underline, Underline::None);
}

#[test]
fn decscusr_and_cursor_visibility() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b[4 q");
  assert_eq!(t.grid().cursor_shape(), CursorShape::Underline);
  feed(&mut t, b"\x1b[6 q");
  assert_eq!(t.grid().cursor_shape(), CursorShape::Beam);
  feed(&mut t, b"\x1b[0 q");
  assert_eq!(t.grid().cursor_shape(), CursorShape::Block);

  feed(&mut t, b"\x1b[?25l");
  assert!(!t.grid().cursor_visible());
  feed(&mut t, b"\x1b[?25h");
  assert!(t.grid().cursor_visible());
}

#[test]
fn osc12_sets_and_resets_cursor_color() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b]12;#ff0000\x07");
  assert_eq!(t.grid().cursor_color(), Some((255, 0, 0)));
  feed(&mut t, b"\x1b]12;rgb:00/80/ff\x07");
  assert_eq!(t.grid().cursor_color(), Some((0, 0x80, 0xFF)));
  feed(&mut t, b"\x1b]112\x07");
  assert_eq!(t.grid().cursor_color(), None);
}

#[test]
fn osc12_queries_cursor_color() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b]12;#ff0000\x07");
  feed(&mut t, b"\x1b]12;?\x07");
  let resp = t.take_response();
  assert!(resp.starts_with(b"\x1b]12;rgb:"), "{resp:?}");
  // A query must report, not reset, the cursor colour.
  assert_eq!(t.grid().cursor_color(), Some((255, 0, 0)));
}

#[test]
fn decscusr_and_cursor_color() {
  use crate::grid::CursorShape;
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b[5 q"); // blinking bar
  assert_eq!(t.grid().cursor_shape(), CursorShape::Beam);
  feed(&mut t, b"\x1b[4 q"); // steady underline
  assert_eq!(t.grid().cursor_shape(), CursorShape::Underline);
  feed(&mut t, b"\x1b]12;#ff3030\x07");
  assert_eq!(t.grid().cursor_color(), Some((0xFF, 0x30, 0x30)));
  feed(&mut t, b"\x1b]112\x07");
  assert_eq!(t.grid().cursor_color(), None);
  feed(&mut t, b"\x1b[?25l"); // hide cursor
  assert!(!t.grid().cursor_visible());
}

#[test]
fn osc_palette_and_dynamic_colors() {
  use crate::theme::Rgb;
  let mut t = Term::new(20, 2);
  // Set palette index 1 and foreground via OSC, then query them back.
  feed(&mut t, b"\x1b]4;1;#ff0000\x1b\\");
  assert_eq!(t.theme().palette[1], Rgb(0xFF, 0, 0));
  feed(&mut t, b"\x1b]10;rgb:00/80/ff\x1b\\");
  assert_eq!(t.theme().fg, Rgb(0, 0x80, 0xFF));
  feed(&mut t, b"\x1b]11;?\x07");
  let resp = t.take_response();
  assert!(resp.starts_with(b"\x1b]11;rgb:"));
  // Reset returns the palette entry to its default.
  feed(&mut t, b"\x1b]104;1\x1b\\");
  assert_ne!(t.theme().palette[1], Rgb(0xFF, 0, 0));
}

#[test]
fn xtgettcap_known_and_unknown() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1bP+q544e\x1b\\"); // "TN"
  assert_eq!(t.take_response(), b"\x1bP1+r544e=62656572\x1b\\"); // = "beer"
  feed(&mut t, b"\x1bP+q6162\x1b\\"); // "ab", unknown
  assert_eq!(t.take_response(), b"\x1bP0+r6162\x1b\\");
}

#[test]
fn bracketed_paste_and_sync_modes() {
  let mut t = Term::new(20, 2);
  feed(&mut t, b"\x1b[?2004h");
  assert!(t.grid().bracketed_paste());
  feed(&mut t, b"\x1b[?2004$p");
  assert_eq!(t.take_response(), b"\x1b[?2004;1$y");
  feed(&mut t, b"\x1b[?2026h");
  assert!(t.grid().sync_active());
  feed(&mut t, b"\x1b[?2026l\x1b[?2026$p");
  assert!(!t.grid().sync_active());
  assert_eq!(t.take_response(), b"\x1b[?2026;2$y");
}

#[test]
fn osc52_set_and_query() {
  let mut t = Term::new(20, 2);
  // Set clipboard to "hi" (base64 "aGk=").
  feed(&mut t, b"\x1b]52;c;aGk=\x07");
  let ops = t.take_clipboard_ops();
  match ops.as_slice() {
    [
      ClipboardOp::Set {
        primary: false,
        text,
      },
    ] => assert_eq!(text, "hi"),
    other => panic!("unexpected ops: {other:?}"),
  }
  // Query the primary selection.
  feed(&mut t, b"\x1b]52;p;?\x07");
  let ops = t.take_clipboard_ops();
  assert!(matches!(ops.as_slice(), [ClipboardOp::Query {
    primary: true,
  }]));
}

#[test]
fn mouse_modes_track_protocol_and_encoding() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"\x1b[?1002h\x1b[?1006h");
  assert_eq!(t.grid().mouse_protocol(), MouseProtocol::Button);
  assert_eq!(t.grid().mouse_encoding(), MouseEncoding::Sgr);
  feed(&mut t, b"\x1b[?1002$p");
  assert_eq!(t.take_response(), b"\x1b[?1002;1$y");
  feed(&mut t, b"\x1b[?1003h"); // any-event supersedes button-event
  assert_eq!(t.grid().mouse_protocol(), MouseProtocol::Any);
  feed(&mut t, b"\x1b[?1000l"); // turning a mouse mode off clears reporting
  assert_eq!(t.grid().mouse_protocol(), MouseProtocol::Off);
  feed(&mut t, b"\x1b[?1004h");
  assert!(t.grid().focus_events());
}

#[test]
fn mouse_pixel_and_urxvt_encodings() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"\x1b[?1016h"); // SGR-pixel
  assert_eq!(t.grid().mouse_encoding(), MouseEncoding::SgrPixel);
  feed(&mut t, b"\x1b[?1016$p");
  assert_eq!(t.take_response(), b"\x1b[?1016;1$y");
  feed(&mut t, b"\x1b[?1015h"); // urxvt supersedes
  assert_eq!(t.grid().mouse_encoding(), MouseEncoding::Urxvt);
  feed(&mut t, b"\x1b[?1015l"); // reset to the default byte form
  assert_eq!(t.grid().mouse_encoding(), MouseEncoding::X10);
}

#[test]
fn title_stack_push_pop() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"\x1b]0;first\x07");
  feed(&mut t, b"\x1b[22t"); // push "first"
  feed(&mut t, b"\x1b]0;second\x07");
  assert_eq!(t.title(), Some("second"));
  feed(&mut t, b"\x1b[23t"); // pop -> "first"
  assert_eq!(t.title(), Some("first"));
}

#[test]
fn sgr_sets_pen_colours() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b[31;1mX");
  let cell = t.grid().cell(0, 0);
  assert_eq!(cell.fg, Color::Indexed(1));
  assert!(cell.flags.contains(Flags::BOLD));
}

#[test]
fn truecolor_semicolon_and_colon() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b[38;2;10;20;30mA");
  assert_eq!(t.grid().cell(0, 0).fg, Color::Rgb(10, 20, 30));
  feed(&mut t, b"\x1b[38:2:40:50:60mB");
  assert_eq!(t.grid().cell(1, 0).fg, Color::Rgb(40, 50, 60));
}

#[test]
fn device_status_reports_cursor() {
  let mut t = Term::new(20, 4);
  feed(&mut t, b"\x1b[3;5H\x1b[6n");
  assert_eq!(t.take_response(), b"\x1b[3;5R");
}

#[test]
fn line_drawing_charset() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b(0qx\x1b(B");
  assert_eq!(t.grid().row_text(0), "─│");
}

#[test]
fn osc133_marks_capture_last_command_output() {
  let mut t = Term::new(12, 6);
  feed(&mut t, b"\x1b]133;A\x07$ echo hi\r\n"); // prompt + typed command
  feed(&mut t, b"\x1b]133;C\x07hi\r\n"); // output start, then output
  feed(&mut t, b"\x1b]133;D\x07"); // command finished
  assert_eq!(t.grid().last_command_output().as_deref(), Some("hi\n"));
}

#[test]
fn osc_notifications_collected() {
  let mut t = Term::new(20, 2);
  // Neovim uses OSC 9;4 for its native progress indicator. This is not an
  // iTerm-style OSC 9 notification body.
  feed(&mut t, b"\x1b]9;4;1;0\x1b\\");
  feed(&mut t, b"\x1b]9;hello\x07");
  feed(&mut t, b"\x1b]777;notify;Title;Body\x07");
  let n = t.take_notifications();
  assert_eq!(n, vec![
    Notification {
      title: None,
      body:  "hello".into(),
    },
    Notification {
      title: Some("Title".into()),
      body:  "Body".into(),
    },
  ]);
  assert!(t.take_notifications().is_empty());
}

#[test]
fn osc99_collects_chunked_title_and_body() {
  let mut t = Term::new(20, 2);
  feed(&mut t, b"\x1b]99;i=job:d=0:p=title;Build\x1b\\");
  feed(&mut t, b"\x1b]99;i=job:d=0:p=body:e=1;eHl6\x1b\\");
  assert!(t.take_notifications().is_empty());
  feed(&mut t, b"\x1b]99;i=job:p=body:e=1;eHl6\x1b\\");
  assert_eq!(t.take_notifications(), vec![Notification {
    title: Some("Build".into()),
    body:  "xyzxyz".into(),
  }]);
}

#[test]
fn osc_progress_tracks_neovim_state_without_notifying() {
  let mut t = Term::new(20, 2);
  feed(&mut t, b"\x1b]9;4;1;42\x1b\\");
  assert_eq!(t.progress(), Some(Progress::Normal(42)));
  assert!(t.take_notifications().is_empty());
  feed(&mut t, b"\x1b]9;4;0;0\x1b\\");
  assert_eq!(t.progress(), None);
}

#[test]
fn osc7_tracks_cwd_and_decodes_percent() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b]7;file://hermes/home/user/my%20dir\x07");
  assert_eq!(t.cwd(), Some("/home/user/my dir"));
  // A non-file or relative URI leaves the previous value untouched? It
  // simply does not match a path, so cwd stays None here.
  let mut t2 = Term::new(20, 1);
  feed(&mut t2, b"\x1b]7;file://host\x07");
  assert_eq!(t2.cwd(), None);
}

#[test]
fn title_via_osc() {
  let mut t = Term::new(20, 1);
  feed(&mut t, b"\x1b]0;hello\x07");
  assert_eq!(t.title(), Some("hello"));
}

#[test]
fn alt_screen_preserves_primary() {
  let mut t = Term::new(20, 2);
  feed(&mut t, b"main");
  feed(&mut t, b"\x1b[?1049h");
  assert_eq!(t.grid().row_text(0), "");
  feed(&mut t, b"\x1b[?1049l");
  assert_eq!(t.grid().row_text(0), "main");
}
