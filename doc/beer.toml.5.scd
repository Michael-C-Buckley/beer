beer.toml(5)

# NAME

beer.toml - configuration file for _beer_(1)

# DESCRIPTION

*beer* reads a TOML configuration file from
_$XDG_CONFIG_HOME/beer/beer.toml_, falling back to
_~/.config/beer/beer.toml_. The *--config* flag (repeatable, later files
override earlier ones) and the *BEER_CONFIG* environment variable (colon-
separated paths, lowest priority) can supply additional files. A missing
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

*font-bold* = _string_, *font-italic* = _string_, *font-bold-italic* = _string_
	Font families for the bold, italic, and bold-italic styles. Unset resolves
	the primary family's corresponding style through fontconfig.

*font-fallback* = _array of string_
	Fallback families, tried in order for code points the primary family lacks,
	before fontconfig coverage matching. Default empty.

*font-features* = _array of string_
	OpenType feature settings applied while shaping. Each entry is a four-letter
	tag with an optional value: _ss01_ or _+ss01_ enables a feature, _-liga_
	disables it, and _cv01=2_ sets an explicit value. Default empty.

*font-variations* = _array of string_
	Variation-axis settings for variable fonts, each _tag=value_ (for example
	_wght=550_ or _slnt=-8_), applied to the primary family and its style and
	fallback variants. Unnamed axes keep their default and static fonts are
	unaffected. Default empty.

*ligatures* = _bool_
	Shape each row's runs so ligatures and contextual alternates form. When
	_false_, cells render in isolation. Default _true_.

*hinting* = _none_ | _slight_ | _normal_
	Outline grid-fitting strength. _none_ disables grid-fitting, _slight_ uses
	the light autohinter (grayscale only), and _normal_ (the default) keeps the
	FreeType default.

*adjust-cell-width* = _integer_, *adjust-cell-height* = _integer_
	Pixels added to the cell width and height. Negative values tighten the
	grid. Scaled with the output. Default _0_ each.

*adjust-baseline* = _integer_
	Pixels the text baseline is shifted down within the cell. Default _0_.

*thicken* = _bool_
	Thicken every glyph by one coverage pixel, a light synthetic weight applied
	on top of any real or synthetic bold. Default _false_.

*subpixel* = _none_ | _rgb_ | _bgr_
	Subpixel (LCD) antialiasing order. _none_ (the default) renders grayscale
	coverage. _rgb_ and _bgr_ enable horizontal LCD rendering; pick the one
	matching the display's physical subpixel layout, as the wrong choice
	produces colour fringing. Colour glyphs (emoji) are unaffected.

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

*alpha-blending* = _native_ | _linear_ | _linear-corrected_
	How glyph coverage is composited over the background. _native_ blends in
	sRGB space (like Foot). _linear_ blends in linear light (gamma-correct, but
	thin text looks lighter). _linear-corrected_ (the default) blends in linear
	light but remaps coverage to keep the perceived weight of _native_ while
	avoiding dark fringing on coloured text, matching Kitty and Ghostty.
	Subpixel (LCD) glyphs always composite in _native_; over a translucent
	background all modes fall back to _native_.

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

# [mouse-bindings]

A table of _button-chord_ = _action_ entries, merged over the built-in
defaults. A chord is optional modifiers and a button joined by _+_, e.g.
_"Shift+Middle"_. Buttons are _Left_, _Middle_, and _Right_. The actions are
the same set as *[key-bindings]*. An action of _"none"_ unbinds the chord. The
left button's select/drag is a built-in gesture and is not rebindable.

The only default is _"Middle" = "paste-primary"_.

```
[mouse-bindings]
"Right" = "paste"
"Middle" = "none"
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
