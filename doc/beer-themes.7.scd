beer-themes(7)

# NAME

beer-themes - colour theming for _beer_(1)

# DESCRIPTION

A *beer* theme is an ordinary _beer.toml_(5) fragment that sets the *[colors]*
table. Because *--config* is repeatable and later files win, a theme is best
kept in its own file and layered over the main config:

```
beer --config ~/.config/beer/beer.toml --config ~/.config/beer/themes/dark.toml
```

or listed in *BEER_CONFIG*. Everything a theme can set is described under
*[colors]* in _beer.toml_(5); this page explains how the pieces fit together
and how applications recolour the terminal at runtime.

Colours are X11 specs: _#rrggbb_ or _rgb:rr/gg/bb_ (1-4 hex digits per channel).

# THE PALETTE

Terminals address colour in three layers, and a theme can set all of them:

*Default foreground and background*
	*foreground* and *background* are the colours used when a cell selects no
	explicit colour (SGR 39 / 49).

*The 16 named colours*
	*regular* (indices 0-7) and *bright* (8-15) are the ANSI palette an
	application selects with SGR 30-37, 40-47, 90-97, and 100-107. Unset entries
	keep the built-in xterm values, so a theme may override only the entries it
	cares about.

*256-colour and truecolor*
	Indices 16-255 are the standard xterm cube and greyscale ramp and are not
	configurable; direct 24-bit colours (SGR 38;2 / 48;2) are used verbatim.

*bold-as-bright* renders bold text in the bright variant of its colour, the
traditional terminal behaviour; set it _false_ to keep bold text in its base
colour.

# CURSOR AND SELECTION

*cursor* sets the cursor colour; left unset, the cursor inverts the cell beneath
it. *selection-foreground* and *selection-background* colour selected text, and
*match-background* and *match-current-background* colour scrollback search
matches and the focused match.

# TRANSPARENCY

*alpha* sets background opacity from _0.0_ (fully transparent) to _1.0_ (opaque)
via premultiplied ARGB; only the background is made translucent, and any blur is
left to the compositor.

# RUNTIME COLOUR ESCAPES

Applications and colour-scheme tools can change the palette live; *beer* honours
the standard OSC sequences and answers a *?* query with the current value:

[[ *OSC*
:< *Effect*
|  *4* ; _n_ ; _spec_
:  Set palette index _n_; *104* resets one or all indices
|  *10* / *11* / *12*
:  Set default foreground / background / cursor colour
|  *110* / *111* / *112*
:  Reset foreground / background / cursor to the configured value
|  *17* / *19*
:  Set selection background / foreground

A reload on *SIGUSR1* re-reads the config and its theme files and re-applies the
palette, so editing a theme and signalling *beer* recolours a live window
without restarting it.

# EXAMPLE

A minimal dark theme in _~/.config/beer/themes/dark.toml_:

```
[colors]
foreground = "#c5c8c6"
background = "#1d1f21"
cursor = "#c5c8c6"
regular = [
	"#282a2e", "#a54242", "#8c9440", "#de935f",
	"#5f819d", "#85678f", "#5e8d87", "#707880",
]
bright = [
	"#373b41", "#cc6666", "#b5bd68", "#f0c674",
	"#81a2be", "#b294bb", "#8abeb7", "#c5c8c6",
]
```

# SEE ALSO

_beer_(1), _beer.toml_(5)
