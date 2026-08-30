beer(1)

# NAME

beer - a fast, software-rendered, Wayland-native terminal emulator

# SYNOPSIS

*beer* [_options_] [--] [_command_ [_argument_ ...]]

# DESCRIPTION

*beer* is a lightweight, CPU-rendered terminal emulator for Wayland. It spawns
the user's login shell in a pseudo-terminal and renders its output into a
Wayland window, with truecolor, styled text, scrollback, incremental search,
mouse and touch selection, and clipboard integration.

# OPTIONS

*--server*
	Run as a daemon: a single process that hosts many windows over a Unix
	socket (see *DAEMON MODE*).

*--no-daemon*
	Always open a private, standalone window; never connect to or start a
	server. Without this flag a plain *beer* prefers a running server.

*--config* _PATH_
	Load configuration from _PATH_ on top of the defaults. Repeatable; when
	given more than once, later files override keys from earlier ones and
	have higher priority than files listed in *BEER_CONFIG* (see
	*ENVIRONMENT*). Without any *--config* flags, the default location is
	used (see *FILES*).

*--title* _TITLE_
	Initial title for the new window.

*--app-id* _APP_ID_
	Wayland _app_id_ for the new window.

*--working-directory* _PATH_
	Start the shell or command in _PATH_. The default is the caller's current
	directory, including when a daemon hosts the window.

*--hold*
	Keep the final terminal contents visible after the child exits. Closing the
	held window returns the child's exit status.

_command_ [_argument_ ...]
	Run _command_ instead of the login shell. Use *--* before a command whose
	name or arguments could be parsed as *beer* options.

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
|  Ctrl+Shift+N
:  Open a new window in the shell's current directory
|  Ctrl+Shift+Up / Ctrl+Shift+Down
:  Jump to the previous / next shell prompt (OSC 133)
|  Ctrl+Shift+O
:  Enter URL hint mode
|  Ctrl+Shift+Y
:  Enter URL hint mode and copy the chosen URL
|  Ctrl+Shift+U
:  Enter a Unicode codepoint by hex value

The *pipe-command-output* action (unbound by default) feeds the last command's
output to the configured command; see *shell-integration* in _beer.toml_(5).

In incremental search, type to refine the query, *Up*/*Return* jumps to the
previous match, *Down* to the next, and *Escape* exits. Search is literal,
crosses soft-wrapped rows, and becomes case-sensitive when the query contains
an uppercase letter.

In URL hint mode each visible URL is tagged with a label; type the label to open
it in the configured launcher, or press *Escape* to cancel. The copy variant
writes the chosen URL to the clipboard. Detection covers _scheme://_ URLs,
_www._ addresses, and _mailto:_, _tel:_, _magnet:_, and _news:_ links.

In Unicode codepoint-input mode, type the hexadecimal value of a codepoint (e.g.
_2603_ for a snowman) and press *Return* or *Space* to insert it; *Escape*
cancels.

*beer* implements the kitty keyboard protocol (progressive enhancement via
*CSI u*), so applications that request it receive disambiguated keys, key
release and repeat events, alternate keys, and associated text.

# MOUSE

Left-click and drag selects text; double-click selects a word, triple-click a
line, and Ctrl+drag selects a rectangular block. The selection is copied to the
primary selection on release; middle-click pastes it. The wheel scrolls
history. Holding *Shift* forces local selection even while an application has
requested mouse reporting. Clicking an *OSC 8* hyperlink opens it; hovering one
underlines it.

Touch-drag scrolls history. Holding one touch point selects text; dragging then
extends the selection and lifting copies it to the primary selection.

# SHELL INTEGRATION

By default *beer* installs a small Bash, Zsh, or Fish hook. It tracks the
working directory (*OSC 7*, used by new windows) and prompt marks (*OSC 133*)
for prompt jumping and piping the last command's output. Arbitrary commands are
not modified. Applications may raise desktop notifications via *OSC 9*,
*OSC 777*, and *OSC 99*.

# DAEMON MODE

*beer --server* runs one process that hosts many windows, sharing a single
Wayland connection and glyph cache. It listens on a Unix socket at
_$XDG_RUNTIME_DIR/beer-$WAYLAND_DISPLAY.sock_. The runtime directory must be
private to the current user; the server refuses to use a shared temporary
directory for this IPC channel.
With systemd-style socket activation (_LISTEN_PID_ matching this process and
_LISTEN_FDS=1_), the server accepts the listener on file descriptor 3 instead
of binding its own path.

A plain *beer* (without *--server* or *--no-daemon*) is the client: it forwards
the working directory, environment, command, hold flag, title, and app-id to a
running server, which opens the requested window, and mirrors that window's exit
status. If no server is running it falls back to a standalone window, so the
same command works with or without a daemon. *--no-daemon* forces the
standalone path.

Whether the server keeps running after its last window closes is set by
*server-resident* in _beer.toml_(5); by default it stays resident.

# GRAPHICS AND TEXT SIZING

*beer* implements the kitty graphics protocol (inline raster images via the
*APC* _G_ transmission, placements, animation, and Unicode placeholders) and the
kitty text-sizing protocol (*OSC 66*), which scales a run of text across a block
of cells. It does not implement sixel.

# ENVIRONMENT

*SHELL*
	The login shell to spawn when no command is supplied. If unset, *beer* uses
	the account's configured login shell, then falls back to _/bin/sh_.

*TERM*
	Set to the configured terminal name (default _xterm-256color_) for the child.

*TERM_PROGRAM*, *TERM_PROGRAM_VERSION*
	Set to _beer_ and the running beer version.

*KITTY_WINDOW_ID*
	Set for the child process to the window's numeric id, so programs that key
	off it (kitty graphics placeholders) have a stable per-window handle.

*BEER_CONFIG*
	A colon-separated list of config file paths, like *PATH*. These are
	lower priority than paths given via *--config* so explicit flags always
	win. Empty elements are ignored.

*BEER_LOG*
	Log filter, in the _tracing_ env-filter syntax (e.g. _debug_).

# SIGNALS

*SIGUSR1*
	Reload the configuration file and re-apply it to every open window without
	restarting.

# FILES

_$XDG_CONFIG_HOME/beer/beer.toml_
	The configuration file (falls back to _~/.config/beer/beer.toml_). See
	_beer.toml_(5).

# SEE ALSO

_beer.toml_(5), _beer-themes_(7)
