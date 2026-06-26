# beer-protocols

The terminal-protocol building blocks beer is built on: byte codecs, the
terminfo capability table, character-set translation, SGR parsing, and the
keyboard/mouse wire encoders. Everything here is a pure function or a plain data
type, with no terminal state, so each protocol detail can be read and tested on
its own. beer feeds these into its `vte`-driven dispatcher and its grid model.

This README doubles as the reference for **which escape sequences, OSC commands,
and POSIX behaviours beer actually implements** - the parts a user notices.
Sequences not listed here are parsed and ignored rather than passed through.
Notation: `ESC` is `0x1b`, `CSI` is `ESC [`, `OSC` is `ESC ]`, `DCS` is `ESC P`,
`ST` is the string terminator `ESC \` (a `BEL` is also accepted to end an OSC).
`Ps` is a numeric parameter, `Pt` a text parameter.

## Crate layout

<!--markdownlint-disable MD013-->

| Module    | What it covers                                                                 |
| --------- | ------------------------------------------------------------------------------ |
| `codec`   | base64 (OSC 52), hex (XTGETTCAP names), `file://` URI percent-decoding (OSC 7) |
| `caps`    | terminfo capabilities answered over XTGETTCAP                                  |
| `charset` | G0/G1 designation and DEC special-graphics line drawing                        |
| `sgr`     | the multi-parameter SGR colour and underline forms                             |
| `key`     | legacy xterm/VT and kitty keyboard-protocol key encoding                       |
| `mouse`   | X10 / UTF-8 / SGR mouse-report encoding                                        |
| `style`   | the enums an SGR/DECSET stream selects                                         |

<!--markdownlint-enable MD013-->

## C0 control characters

<!--markdownlint-disable MD013-->

| Byte          | Name     | Effect                                                        |
| ------------- | -------- | ------------------------------------------------------------- |
| `0x07`        | BEL      | rings the bell (visual flash / command / urgency, per config) |
| `0x08`        | BS       | backspace                                                     |
| `0x09`        | HT       | tab to next stop                                              |
| `0x0A`-`0x0C` | LF/VT/FF | line feed                                                     |
| `0x0D`        | CR       | carriage return                                               |
| `0x0E`        | SO       | invoke G1 (shift out)                                         |
| `0x0F`        | SI       | invoke G0 (shift in)                                          |

<!--markdownlint-enable MD013-->

## Cursor movement and editing (CSI)

<!--markdownlint-disable MD013-->

| Sequence                 | Name        | Effect                                     |
| ------------------------ | ----------- | ------------------------------------------ |
| `CSI Ps A`               | CUU         | cursor up                                  |
| `CSI Ps B` / `CSI Ps e`  | CUD         | cursor down                                |
| `CSI Ps C` / `CSI Ps a`  | CUF         | cursor forward                             |
| `CSI Ps D`               | CUB         | cursor back                                |
| `CSI Ps E`               | CNL         | cursor down, to column 1                   |
| `CSI Ps F`               | CPL         | cursor up, to column 1                     |
| `CSI Ps G` / `CSI Ps \`` | CHA         | move to column                             |
| `CSI Ps d`               | VPA         | move to row                                |
| `CSI Ps ; Ps H` / `f`    | CUP/HVP     | move to row;col                            |
| `CSI Ps J`               | ED          | erase in display (0 below, 1 above, 2 all) |
| `CSI Ps K`               | EL          | erase in line (0 right, 1 left, 2 all)     |
| `CSI Ps @`               | ICH         | insert blank characters                    |
| `CSI Ps P`               | DCH         | delete characters                          |
| `CSI Ps L`               | IL          | insert lines                               |
| `CSI Ps M`               | DL          | delete lines                               |
| `CSI Ps X`               | ECH         | erase characters                           |
| `CSI Ps S`               | SU          | scroll up                                  |
| `CSI Ps T`               | SD          | scroll down                                |
| `CSI Ps ; Ps r`          | DECSTBM     | set scroll region (top;bottom)             |
| `CSI Ps g`               | TBC         | clear tab stop (0) or all tabs (3)         |
| `CSI s` / `CSI u`        | SCOSC/SCORC | save / restore cursor                      |

<!--markdownlint-enable MD013-->

## ESC (non-CSI)

<!--markdownlint-disable MD013-->

| Sequence              | Name        | Effect                                                              |
| --------------------- | ----------- | ------------------------------------------------------------------- |
| `ESC D`               | IND         | line feed                                                           |
| `ESC M`               | RI          | reverse index                                                       |
| `ESC E`               | NEL         | next line                                                           |
| `ESC 7` / `ESC 8`     | DECSC/DECRC | save / restore cursor                                               |
| `ESC H`               | HTS         | set tab stop                                                        |
| `ESC c`               | RIS         | full reset                                                          |
| `ESC ( c` / `ESC ) c` | SCS         | designate G0 / G1 charset (`0` = DEC special graphics, `B` = ASCII) |

<!--markdownlint-enable MD013-->

## Graphic rendition (SGR, `CSI Ps m`)

Bold (1), dim (2), italic (3), underline (4), blink (5/6), reverse (7), hidden
(8), strike (9), and their resets (22-29). Overline (53) and its reset (55).
Underline styles via `4:x` and SGR 21: single, double, curly, dotted, dashed.

Colours: the 16 ANSI colours (30-37, 90-97 foreground; 40-47, 100-107
background) and default (39/49). Extended colour for foreground (38), background
(48), and underline (58/59), in both the legacy semicolon form (`38;5;n`,
`38;2;r;g;b`) and the colon-subparameter form (`38:5:n`, `38:2:r:g:b`, with an
ignored colour-space id). 256-colour and 24-bit truecolor are fully supported.

## Modes (DECSET/DECRST `CSI [?] Ps h` / `l`)

| Mode            | Name    | Effect                                 |
| --------------- | ------- | -------------------------------------- |
| `4` (ANSI)      | IRM     | insert/replace                         |
| `?1`            | DECCKM  | application cursor keys                |
| `?6`            | DECOM   | origin mode                            |
| `?7`            | DECAWM  | autowrap                               |
| `?25`           | DECTCEM | cursor visibility                      |
| `?9`            | -       | X10 mouse reporting                    |
| `?1000`         | -       | normal mouse (press/release)           |
| `?1002`         | -       | button-event mouse (drag)              |
| `?1003`         | -       | any-event mouse (all motion)           |
| `?1004`         | -       | focus in/out reporting                 |
| `?1005`         | -       | UTF-8 mouse coordinates                |
| `?1006`         | -       | SGR mouse coordinates                  |
| `?47` / `?1047` | -       | alternate screen                       |
| `?1049`         | -       | alternate screen + save/restore cursor |
| `?2004`         | -       | bracketed paste                        |
| `?2026`         | -       | synchronized output                    |

Mode state is reportable with **DECRQM** (`CSI [?] Ps $ p`), which replies
`CSI [?] Ps ; state $ y` (1 set, 2 reset, 0 unrecognized).

## Cursor style (DECSCUSR, `CSI Ps SP q`)

`0`/`1` blinking block, `2` steady block, `3` blinking underline, `4` steady
underline, `5` blinking bar, `6` steady bar. The configured default applies
until an application overrides it.

## Device reports

<!--markdownlint-disable MD013-->

| Sequence             | Name      | Reply                                                      |
| -------------------- | --------- | ---------------------------------------------------------- |
| `CSI c`              | DA1       | `CSI ?62;22c` (VT220 + ANSI colour)                        |
| `CSI > c`            | DA2       | `CSI >0;276;0c`                                            |
| `CSI = c`            | DA3       | `DCS !\|00000000 ST`                                       |
| `CSI 5 n`            | DSR       | `CSI 0n` (terminal OK)                                     |
| `CSI 6 n`            | CPR       | `CSI row;col R` (cursor position)                          |
| `CSI > q`            | XTVERSION | `DCS >\|beer(version) ST`                                  |
| `DCS + q <names> ST` | XTGETTCAP | per name, `DCS 1 + r name=value ST` or `DCS 0 + r name ST` |

<!--markdownlint-enable MD013-->

XTGETTCAP answers `TN` (terminal name `beer`), `Co`/`colors` (256), and `RGB`
(`8/8/8`, i.e. truecolor).

## Window title

`OSC 0 ; Pt` and `OSC 2 ; Pt` set the title. The title stack (`CSI 22 t` push,
`CSI 23 t` pop) is supported, so full-screen programs can save and restore it.

## OSC commands

<!--markdownlint-disable MD013-->

| Command                           | Effect                                           |
| --------------------------------- | ------------------------------------------------ |
| `OSC 0` / `OSC 2`                 | set window title                                 |
| `OSC 4 ; idx ; spec`              | set / query palette entry (`?` queries)          |
| `OSC 104 [; idx ...]`             | reset palette (all, or listed entries)           |
| `OSC 10` / `OSC 11`               | set / query default foreground / background      |
| `OSC 110` / `OSC 111`             | reset foreground / background                    |
| `OSC 12` / `OSC 112`              | set / reset cursor colour                        |
| `OSC 17` / `OSC 19`               | set / query selection background / foreground    |
| `OSC 7 ; file://host/path`        | report working directory (used for new windows)  |
| `OSC 8 ; params ; URI`            | hyperlink (empty URI ends it)                    |
| `OSC 9 ; Pt`                      | desktop notification (iTerm2 style)              |
| `OSC 777 ; notify ; title ; body` | desktop notification (rxvt style)                |
| `OSC 99 ; metadata ; body`        | desktop notification (kitty style, single-chunk) |
| `OSC 52 ; target ; data`          | clipboard set (base64) or query (`?`)            |
| `OSC 133 ; A/B/C/D`               | shell-integration prompt marks                   |

<!--markdownlint-enable MD013-->

Colour specs follow the X11 forms `#rrggbb` and `rgb:rr/gg/bb` (1-4 hex digits
per channel). Colour queries reply in the `rgb:rrrr/gggg/bbbb` form.

### Clipboard (OSC 52)

`OSC 52 ; c ; <base64>` sets the clipboard, `; p ;` the primary selection;
`; c ; ?` queries. For privacy, a query is answered from the text beer itself
last placed there, **not** the live system clipboard - so a remote program
cannot read what another application copied.

### Shell integration (OSC 133 / OSC 7)

Prompt marks `A` (prompt start), `B` (command start), `C` (output start), and
`D` (command end) drive prompt-jumping and "pipe last command output". `OSC 7`
tracks the working directory so a new window opens in the same place.

## Keyboard

The legacy xterm/VT encoding covers cursor keys (with DECCKM application mode),
the editing keypad (Insert/Delete/PageUp/PageDown), F1-F12, and the xterm
modifier parameter (`CSI 1 ; m <letter>`), with Alt sending an ESC (meta)
prefix.

The **kitty keyboard protocol** (`CSI > flags u` push, `CSI < flags u` pop,
`CSI = flags ; mode u` set, `CSI ? u` query) is implemented with the
disambiguate, report-event-types, report-alternate-keys, report-all-keys, and
report-associated-text enhancement flags. Keys are encoded as
`CSI
code[:shifted] ; mod[:event] [; text] u`, with a 32-deep push/pop stack.
The common keys (Enter, Tab, Backspace) keep their legacy bytes when unmodified
so a plain shell stays usable.

## Mouse

Reports are framed in the legacy byte form (`CSI M Cb Cx Cy`), the UTF-8
coordinate form (DECSET 1005), or the SGR form (`CSI < Cb ; Cx ; Cy M/m`, DECSET
1006). Shift/Alt/Ctrl modifier bits and the motion bit are encoded; legacy
releases collapse to button code 3. Reporting level is chosen by the application
via the mouse modes above; with reporting off the pointer drives local selection
and scrollback.

## POSIX / terminal behaviour

- A real PTY pair (`rustix` `openpt`/`grantpt`/`unlockpt`), with the child's
  `TERM` set to the configured value (default `beer`) and the window size kept
  in sync via `TIOCSWINSZ` (`SIGWINCH` reaches the child).
- The child's exit status is propagated as beer's own exit code.
- Alternate screen, scroll regions, autowrap and reflow on resize, and a
  scrollback buffer.
- Bracketed paste (DECSET 2004) and synchronized output (DECSET 2026) so
  applications can paste safely and update atomically.
