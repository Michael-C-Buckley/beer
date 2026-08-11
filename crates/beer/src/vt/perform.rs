use vte::Perform;

use super::{
  Charset,
  ClipboardOp,
  CursorShape,
  DaLevel,
  Dynamic,
  Notification,
  Params,
  Progress,
  Rgb,
  Term,
  base64_decode,
  charset,
  file_uri_path,
  n,
  osc_text,
  parse_index,
  parse_spec,
  prompt_kind,
  raw,
  rgb_tuple,
  translate,
};

#[expect(
  clippy::absolute_paths,
  clippy::cast_possible_truncation,
  reason = "VT parameter values are bounded protocol fields at this dispatch \
            boundary"
)]
impl Perform for Term {
  fn print(&mut self, c: char) {
    let c = translate(self.active_charset(), c);
    self.single_shift = None;
    self.grid.print(c);
  }

  fn execute(&mut self, byte: u8) {
    match byte {
      0x07 => self.bell = true,
      0x08 => self.grid.backspace(),
      0x09 => self.grid.tab(),
      0x0A..=0x0C => self.grid.line_feed(),
      0x0D => self.grid.carriage_return(),
      0x0E => self.gl = 1,
      0x0F => self.gl = 0,
      _ => {},
    }
  }

  fn csi_dispatch(
    &mut self,
    params: &Params,
    intermediates: &[u8],
    _ignore: bool,
    action: char,
  ) {
    let private = intermediates.first() == Some(&b'?');
    match action {
      'A' => self.grid.cursor_up(n(params, 0, 1)),
      'B' | 'e' => self.grid.cursor_down(n(params, 0, 1)),
      'C' | 'a' => self.grid.cursor_fwd(n(params, 0, 1)),
      'D' => self.grid.cursor_back(n(params, 0, 1)),
      'E' => {
        self.grid.cursor_down(n(params, 0, 1));
        self.grid.carriage_return();
      },
      'F' => {
        self.grid.cursor_up(n(params, 0, 1));
        self.grid.carriage_return();
      },
      'G' | '`' => self.grid.move_to_col(n(params, 0, 1) - 1),
      'd' => self.grid.move_to_row(n(params, 0, 1) - 1),
      'H' | 'f' => self.grid.move_to(n(params, 1, 1) - 1, n(params, 0, 1) - 1),
      'J' => self.grid.erase_display(raw(params, 0)),
      'K' => self.grid.erase_line(raw(params, 0)),
      '@' => self.grid.insert_chars(n(params, 0, 1)),
      'P' => self.grid.delete_chars(n(params, 0, 1)),
      'L' => self.grid.insert_lines(n(params, 0, 1)),
      'M' => self.grid.delete_lines(n(params, 0, 1)),
      'X' => self.grid.erase_chars(n(params, 0, 1)),
      'b' => self.grid.repeat(n(params, 0, 1)),
      'S' => self.grid.scroll_up(n(params, 0, 1)),
      'T' => self.grid.scroll_down(n(params, 0, 1)),
      'm' => self.sgr(params),
      'r' => {
        let top = n(params, 0, 1) - 1;
        let bottom = match params.iter().nth(1).and_then(|p| p.first().copied())
        {
          Some(0) | None => self.grid.rows() - 1,
          Some(v) => (v as usize).saturating_sub(1),
        };
        self.grid.set_scroll_region(top, bottom);
      },
      'h' => self.set_mode(params, private, true),
      'l' => self.set_mode(params, private, false),
      'c' => {
        self.device_attrs(match intermediates.first() {
          Some(b'>') => DaLevel::Secondary,
          Some(b'=') => DaLevel::Tertiary,
          _ => DaLevel::Primary,
        });
      },
      'q' if intermediates.first() == Some(&b'>') => self.report_version(),
      'q' if intermediates.first() == Some(&b' ') => {
        let code = raw(params, 0);
        self.grid.set_cursor_shape(match code {
          3 | 4 => CursorShape::Underline,
          5 | 6 => CursorShape::Beam,
          _ => CursorShape::Block,
        });
        // Even codes are steady; 0/1 and other odd codes blink.
        self.grid.set_cursor_blink(code == 0 || code % 2 == 1);
      },
      'p' if intermediates.contains(&b'$') => self.report_mode(params, private),
      'n' => self.device_status(params),
      // `CSI s` is DECSLRM when left/right margins are enabled, else DECSC.
      's' => {
        if self.grid.lr_margins_enabled() {
          let left = n(params, 0, 1) - 1;
          let right =
            match params.iter().nth(1).and_then(|p| p.first().copied()) {
              Some(0) | None => self.grid.cols() - 1,
              Some(v) => (v as usize).saturating_sub(1),
            };
          self.grid.set_lr_margins(left, right);
        } else {
          self.grid.save_cursor();
        }
      },
      // `CSI u` is SCORC, but the kitty keyboard protocol overloads it with
      // private prefixes: `?` query, `>` push, `<` pop, `=` set flags.
      'u' => {
        match intermediates.first() {
          Some(b'?') => self.report_kitty_flags(),
          Some(b'>') => self.grid.kitty_push(n(params, 0, 0) as u8),
          Some(b'<') => self.grid.kitty_pop(n(params, 0, 1)),
          Some(b'=') => {
            self
              .grid
              .kitty_set(n(params, 0, 0) as u8, n(params, 1, 1) as u8);
          },
          _ => self.grid.restore_cursor(),
        }
      },
      // `CSI 14/16/18 t` report pixel/character geometry (used by graphics
      // clients to size images); other `t` operations are title-stack ops.
      't' => {
        match raw(params, 0) {
          14 | 16 | 18 => self.report_geometry(raw(params, 0)),
          _ => self.title_stack_op(params),
        }
      },
      'g' => {
        match raw(params, 0) {
          3 => self.grid.clear_all_tabs(),
          _ => self.grid.clear_tab(),
        }
      },
      _ => tracing::trace!("unhandled CSI {action:?} {intermediates:?}"),
    }
  }

  fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
    match (intermediates.first().copied(), byte) {
      (None, b'D') => self.grid.line_feed(),
      (None, b'M') => self.grid.reverse_index(),
      (None, b'E') => self.grid.next_line(),
      (None, b'7') => self.grid.save_cursor(),
      (None, b'8') => self.grid.restore_cursor(),
      (None, b'H') => self.grid.set_tab(),
      // DECKPAM / DECKPNM: application vs numeric keypad.
      (None, b'=') => self.grid.set_app_keypad(true),
      (None, b'>') => self.grid.set_app_keypad(false),
      // SS2/SS3 shift the next character into G2/G3; LS2/LS3 lock GL there.
      (None, b'N') => self.single_shift = Some(2),
      (None, b'O') => self.single_shift = Some(3),
      (None, b'n') => self.gl = 2,
      (None, b'o') => self.gl = 3,
      (None, b'c') => {
        self.grid.hard_reset();
        self.g0 = Charset::Ascii;
        self.g1 = Charset::Ascii;
        self.g2 = Charset::Ascii;
        self.g3 = Charset::Ascii;
        self.gl = 0;
        self.single_shift = None;
      },
      (Some(b'#'), b'8') => self.grid.decaln(),
      (Some(b'('), c) => self.g0 = charset(c),
      (Some(b')'), c) => self.g1 = charset(c),
      (Some(b'*'), c) => self.g2 = charset(c),
      (Some(b'+'), c) => self.g3 = charset(c),
      _ => {},
    }
  }

  fn osc_dispatch(&mut self, params: &[&[u8]], bell: bool) {
    match params.first() {
      Some(&n) if n == b"0" || n == b"2" => {
        if let Some(text) = params.get(1) {
          self.title = Some(String::from_utf8_lossy(text).into_owned());
        }
      },
      // OSC 7: the shell reports its cwd as a `file://host/path` URI.
      Some(&n) if n == b"7" => {
        if let Some(uri) = params.get(1) {
          self.cwd = file_uri_path(uri);
        }
      },
      // OSC 133: shell-integration prompt marks (A/B/C/D, with optional
      // `;key=value` attributes we ignore).
      Some(&n) if n == b"133" => {
        if let Some(kind) = params
          .get(1)
          .and_then(|p| p.first())
          .and_then(|&b| prompt_kind(b))
        {
          self.grid.set_prompt_mark(kind);
        }
      },
      // OSC 8: hyperlink. `OSC 8 ; params ; URI ST`; an empty URI ends the
      // link. The URI is everything after the second field, rejoined since
      // a URI may itself contain ';'.
      Some(&n) if n == b"8" => {
        let uri_bytes = params
          .get(2..)
          .map(|parts| parts.join(&b';'))
          .unwrap_or_default();
        let uri = std::str::from_utf8(&uri_bytes).unwrap_or("");
        self.grid.set_link((!uri.is_empty()).then_some(uri));
      },
      // OSC 9;4 is the terminal-progress protocol used by Neovim.
      Some(&n) if n == b"9" && params.get(1) == Some(&&b"4"[..]) => {
        self.progress = match params.get(2).copied() {
          Some(b"0") => None,
          Some(b"1") => {
            params
              .get(3)
              .and_then(|p| std::str::from_utf8(p).ok())
              .and_then(|p| p.parse::<u8>().ok())
              .filter(|&p| p <= 100)
              .map(Progress::Normal)
          },
          Some(b"2") => Some(Progress::Error),
          Some(b"3") => Some(Progress::Paused),
          Some(b"4") => Some(Progress::Indeterminate),
          _ => self.progress,
        };
      },
      // OSC 9: iTerm2-style notification (`OSC 9 ; body`).
      Some(&n) if n == b"9" => {
        if let Some(body) = osc_text(params.get(1)) {
          self.notifications.push(Notification { title: None, body });
        }
      },
      // OSC 777: `OSC 777 ; notify ; title ; body`.
      Some(&n) if n == b"777" && params.get(1) == Some(&&b"notify"[..]) => {
        let title = osc_text(params.get(2));
        if let Some(body) = osc_text(params.get(3)) {
          self.notifications.push(Notification { title, body });
        }
      },
      // OSC 99: kitty desktop-notification protocol. We honour the common
      // single-chunk form, taking the payload as the body and ignoring the
      // metadata key=value field.
      Some(&n) if n == b"99" => {
        if let Some(body) = osc_text(params.get(2)).filter(|b| !b.is_empty()) {
          self.notifications.push(Notification { title: None, body });
        }
      },
      // OSC 4: set/query palette entries (pairs of index;spec).
      Some(&n) if n == b"4" => self.osc_palette(params, bell),
      // OSC 104: reset palette (all, or the listed indices).
      Some(&n) if n == b"104" => {
        if params.len() <= 1 {
          self.theme.reset_palette();
        } else {
          for p in &params[1..] {
            if let Some(i) = parse_index(p) {
              self.theme.reset_palette_index(i);
            }
          }
        }
      },
      // OSC 10/11: foreground / background; OSC 110/111 reset them.
      Some(&n) if n == b"10" => {
        self.osc_dynamic_color(Dynamic::Fg, params.get(1), bell);
      },
      Some(&n) if n == b"11" => {
        self.osc_dynamic_color(Dynamic::Bg, params.get(1), bell);
      },
      Some(&n) if n == b"110" => self.theme.reset_fg(),
      Some(&n) if n == b"111" => self.theme.reset_bg(),
      // OSC 17/19: selection (highlight) background / foreground.
      Some(&n) if n == b"17" => {
        self.osc_dynamic_color(Dynamic::SelBg, params.get(1), bell);
      },
      Some(&n) if n == b"19" => {
        self.osc_dynamic_color(Dynamic::SelFg, params.get(1), bell);
      },
      // OSC 12: set or query cursor colour; OSC 112: reset to default. A query
      // reports the effective colour the renderer uses: the OSC-set colour, the
      // configured cursor colour, then the foreground.
      Some(&n) if n == b"12" => {
        match params.get(1) {
          Some(spec) if **spec == b"?"[..] => {
            let rgb = self
              .grid
              .cursor_color()
              .map(|(r, g, b)| Rgb(r, g, b))
              .or(self.theme.cursor)
              .unwrap_or(self.theme.fg);
            self.reply_color("12", rgb, bell);
          },
          spec => {
            let color = spec.and_then(|s| parse_spec(s)).map(rgb_tuple);
            self.grid.set_cursor_color(color);
          },
        }
      },
      Some(&n) if n == b"112" => self.grid.set_cursor_color(None),
      // OSC 52: clipboard get/set. Pc selects the target, Pd is base64 or
      // `?` to query. We only touch `c` (clipboard) and `p` (primary).
      Some(&n) if n == b"52" => {
        let target = params.get(1).copied().unwrap_or(b"");
        let data = params.get(2).copied().unwrap_or(b"");
        let primary = target.first() == Some(&b'p');
        if data == b"?" {
          self.clipboard_ops.push(ClipboardOp::Query { primary });
        } else if let Some(text) =
          base64_decode(data).and_then(|b| String::from_utf8(b).ok())
        {
          self.clipboard_ops.push(ClipboardOp::Set { primary, text });
        }
      },
      // OSC 66: kitty text-sizing protocol. `OSC 66 ; metadata ; text`,
      // where metadata is a colon-separated key=value list and the text
      // (which may itself contain ';') is rejoined and laid out scaled.
      Some(&n) if n == b"66" => {
        let size = beer_protocols::text_size::parse(
          params.get(1).copied().unwrap_or(b""),
        );
        let text = params
          .get(2..)
          .map(|parts| parts.join(&b';'))
          .unwrap_or_default();
        if let Ok(text) = std::str::from_utf8(&text) {
          self.grid.print_sized(text, size);
        }
      },
      _ => {},
    }
  }

  fn hook(&mut self, _: &Params, intermediates: &[u8], _: bool, action: char) {
    // XTGETTCAP arrives as `DCS + q <names> ST`, DECRQSS as `DCS $ q <req> ST`.
    if action == 'q' && intermediates == [b'+'] {
      self.xtgettcap = Some(Vec::new());
    } else if action == 'q' && intermediates == [b'$'] {
      self.decrqss = Some(Vec::new());
    }
  }

  fn put(&mut self, byte: u8) {
    if let Some(buf) = self.xtgettcap.as_mut() {
      buf.push(byte);
    }
    if let Some(buf) = self.decrqss.as_mut() {
      buf.push(byte);
    }
  }

  fn unhook(&mut self) {
    if let Some(payload) = self.xtgettcap.take() {
      self.answer_xtgettcap(&payload);
    }
    if let Some(payload) = self.decrqss.take() {
      self.answer_decrqss(&payload);
    }
  }
}
