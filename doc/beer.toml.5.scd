beer.toml(5)

# NAME

beer.toml - configuration file for _beer_(1)

# DESCRIPTION

*beer* reads a TOML configuration file from
_$XDG_CONFIG_HOME/beer/beer.toml_, falling back to
_~/.config/beer/beer.toml_, or from the path given to *--config*. A missing
file uses the built-in defaults; a malformed file is reported and the defaults
are used. Unknown keys are ignored, so a file written for a newer *beer* still
loads.

Colours are X11 specs: either _#rrggbb_ or _rgb:rr/gg/bb_ (1-4 hex digits per
channel).

# [main]

*font* = _string_
	Primary font family, resolved via fontconfig. Default _monospace_.

*font-size* = _integer_
	Font size in pixels. Default _16_.

*term* = _string_
	The _TERM_ value exported to the child shell. Default _beer_.

*initial-cols* = _integer_, *initial-rows* = _integer_
	Initial window size in character cells. Default _80_ by _24_.

*pad-x* = _integer_, *pad-y* = _integer_
	Inner padding in pixels between the window edge and the cell grid.
	Default _2_ each.

*word-delimiters* = _string_
	Characters that break a word for double-click selection. Unset keeps the
	built-in set (whitespace and common punctuation, keeping _.-/:~\__ inside
	words).

*idle-inhibit* = _bool_
	While the window is focused, ask the compositor not to blank the screen or
	start the screensaver (idle-inhibit-v1). A backgrounded window stops
	inhibiting. Default _false_.

# [colors]

*foreground* = _color_, *background* = _color_
	Default text and background colours. Default _#c5c8c6_ on _#181818_.

*cursor* = _color_
	Cursor colour. Unset follows the cell under the cursor.

*selection-foreground* = _color_, *selection-background* = _color_
	Selection colours. The background defaults to _#44475a_.

*regular* = _[8 colors]_, *bright* = _[8 colors]_
	The eight regular (indices 0-7) and bright (8-15) palette entries. Unset
	entries keep the xterm defaults.

*match-background* = _color_, *match-current-background* = _color_
	Highlight colours for search matches and the focused match.

*alpha* = _float_
	Background opacity, _0.0_ (transparent) to _1.0_ (opaque). Default _1.0_.

*bold-as-bright* = _bool_
	Render bold text with the bright palette variant. Default _true_.

# [cursor]

*style* = _string_
	Default cursor shape: _block_, _beam_ (or _bar_), or _underline_. An
	application may override this at runtime via DECSCUSR. Default _block_.

*blink* = _bool_
	Whether the cursor blinks by default. Default _false_.

# [scrollback]

*lines* = _integer_
	Lines of history retained for the main screen. Default _10000_.

# [bell]

*visual* = _bool_
	Briefly flash the screen on the terminal bell. Default _false_.

*command* = _[argv]_
	Command run on the bell, e.g. _["paplay", "/usr/share/sounds/bell.oga"]_.
	Unset runs nothing.

*urgent* = _bool_
	Request the compositor's attention (xdg-activation) when the bell rings
	while the window is unfocused. Default _false_.

# [mouse]

*scroll-multiplier* = _float_
	Multiplier applied to the lines scrolled per wheel notch. Default _1.0_.

*alternate-scroll* = _bool_
	On the alternate screen, translate the wheel into cursor-key presses so
	full-screen programs that did not request mouse reporting (such as _less_
	and _man_) still scroll. Default _true_.

# [key-bindings]

A table of _chord_ = _action_ entries, merged over the built-in defaults. A
chord is modifiers and a key joined by _+_, e.g. _"Ctrl+Shift+C"_. An action of
_"none"_ unbinds the chord.

Recognized actions: _copy_, _paste_, _paste-primary_, _scrollback-up_,
_scrollback-down_, _scrollback-top_, _scrollback-bottom_, _search_,
_font-increase_, _font-decrease_, _font-reset_, _fullscreen_, _new-window_,
_jump-prompt-up_, _jump-prompt-down_, _pipe-command-output_, _url-mode_,
_unicode-input_.

```
[key-bindings]
"Ctrl+Shift+C" = "copy"
"Ctrl+`" = "none"
```

# [shell-integration]

Behaviour driven by *OSC 7* (cwd) and *OSC 133* (prompt) marks.

*pipe-command* = _[argv]_
	Command the _pipe-command-output_ binding feeds the last command's output
	to on stdin, e.g. _["less"]_. Empty (the default) disables the action.

# [url]

*launch* = _[argv]_
	Launcher the URL is appended to when an _OSC 8_ hyperlink is clicked or a
	hint-mode label is chosen. Default _["xdg-open"]_.

# [notify]

*command* = _[argv]_
	Notifier for desktop notifications (_OSC 9_/_777_/_99_); the title and body
	are appended as the final two arguments. Default _["notify-send"]_.

# [text-bindings]

A table of _chord_ = _text_ entries that send a literal string to the shell.
The text understands the escapes _\\e_, _\\n_, _\\r_, _\\t_, _\\\\_, and
_\\xNN_.

```
[text-bindings]
"Ctrl+Shift+Return" = "\\x1b\\r"
```

# SEE ALSO

_beer_(1)
