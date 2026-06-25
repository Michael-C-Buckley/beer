beer(1)

# NAME

beer - a fast, software-rendered, Wayland-native terminal emulator

# SYNOPSIS

*beer* [_options_]

# DESCRIPTION

*beer* is a lightweight, CPU-rendered terminal emulator for Wayland. It spawns
the user's login shell in a pseudo-terminal and renders its output into a
server-decorated window, with truecolor, styled text, scrollback, incremental
search, mouse selection, and clipboard integration.

# OPTIONS

*--server*
	Run as a daemon hosting multiple windows. (Not yet implemented.)

*--config* _PATH_
	Load configuration from _PATH_ instead of the default location (see
	*FILES*).

*-h*, *--help*
	Print usage and exit.

*-V*, *--version*
	Print the version and exit.

# KEY BINDINGS

These are the built-in defaults; all are configurable in the *key-bindings*
table of _beer.toml_(5).

[[ *Binding*
:< *Action*
|  Ctrl+Shift+C
:  Copy the selection to the clipboard
|  Ctrl+Shift+V
:  Paste from the clipboard
|  Ctrl+Shift+F
:  Start incremental scrollback search
|  Shift+Page_Up / Shift+Page_Down
:  Scroll the viewport one page
|  Ctrl+Shift+Home / Ctrl+Shift+End
:  Scroll to the top / bottom of history
|  Ctrl+Plus / Ctrl+Equal / Ctrl+Minus
:  Increase / decrease the font size
|  Ctrl+0
:  Reset the font size
|  F11
:  Toggle fullscreen

In incremental search, type to refine the query, *Up*/*Return* jumps to the
previous match, *Down* to the next, and *Escape* exits.

# MOUSE

Left-click and drag selects text; double-click selects a word, triple-click a
line, and Ctrl+drag selects a rectangular block. The selection is copied to the
primary selection on release; middle-click pastes it. The wheel scrolls
history. Holding *Shift* forces local selection even while an application has
requested mouse reporting.

# ENVIRONMENT

*SHELL*
	The shell to spawn; defaults to _/bin/sh_.

*TERM*
	Set to the configured terminal name (default _beer_) for the child.

*BEER_LOG*
	Log filter, in the _tracing_ env-filter syntax (e.g. _debug_).

# SIGNALS

*SIGUSR1*
	Reload the configuration file and re-apply it without restarting.

# FILES

_$XDG_CONFIG_HOME/beer/beer.toml_
	The configuration file (falls back to _~/.config/beer/beer.toml_). See
	_beer.toml_(5).

# SEE ALSO

_beer.toml_(5)
