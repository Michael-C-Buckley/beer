# beer

`beer` is a small, lightweight terminal that aims to get out of the way quickly
and stay there.

It is written in Rust, rendered on the CPU, and intentionally small in scope:
there is no GPU renderer, tab system, ligature shaping, or async runtime. The
project is pre-1.0, but it is already comfortable as a standalone terminal or as
a resident daemon hosting multiple Wayland windows.

## Features

- A software-rendered Wayland window using `wl_shm`, frame callbacks, buffer
  reuse, server-side decoration negotiation, fullscreen, and fractional
  compositor scaling.
- A PTY-backed login shell with `TIOCSWINSZ` resize propagation, inherited or
  forwarded environment, configurable `TERM`, and child exit-status propagation.
- VT parsing through `vte`: cursor movement, erase and insert operations, scroll
  regions, alternate screen, autowrap, SGR attributes, truecolor, 256-colour,
  dynamic palette and theme changes, title stacks, device reports, XTGETTCAP,
  synchronized output, bracketed paste, and focus reporting.
- Kitty protocol support for keyboard progressive enhancement, graphics,
  Unicode-aware text sizing, and desktop notifications, alongside legacy
  xterm/VT keyboard and mouse encodings.
- Fontconfig and FreeType font discovery with styled variants, per-codepoint
  fallback, HarfBuzz shaping, bounded glyph caching, subpixel rendering, and
  colour emoji.
- Scrollback with wheel and key scrolling, resize reflow, prompt-aware jumps,
  last-command-output piping, and incremental search with match highlighting.
- Word, line, rectangular, and URL-aware selection with clipboard and primary
  selection support, OSC 52 set/query, OSC 8 hyperlinks, URL hint mode, and
  configurable URL launch commands.
- Configurable keyboard, text, and mouse bindings, runtime font-size controls,
  cursor shape and blink settings, visual bells, bell commands, compositor
  urgency, desktop notification commands, idle inhibition, and theme alpha
  blending.
- TOML configuration with unknown-key warnings and live reload on `SIGUSR1`.
- Standalone and daemon modes: a resident server can host multiple windows,
  while clients forward their working directory and environment over a private
  Unix socket.

## Build

The repository provides a Nix dev shell with the Rust toolchain and native
Wayland/font dependencies:

```sh
# Enter the development shell.
$ nix develop

# Build an optimized local binary.
$ cargo build --release
```

The Nix package builds the binary, terminfo entry, and man pages:

```sh
# Build the Nix package.
$ nix build
```

## Run

From a Wayland session:

```sh
# Start Beer from your terminal.
$ beer
```

or, after a release build:

```sh
# Run the release binary directly.
$ ./target/release/beer
```

Useful flags:

```sh
# Pass a config file to Beer.
$ beer --config /path/to/beer.toml

# Check the version with -V or --version.
$ beer --version

# See the help text.
$ beer --help
```

`--server` starts a resident daemon. A plain `beer` invocation connects to that
daemon when available and opens a new window; `--no-daemon` forces a private
standalone process. The daemon socket lives under `$XDG_RUNTIME_DIR` and is
restricted to the current user.

## Configuration

By default, `beer` reads `$XDG_CONFIG_HOME/beer/beer.toml`, or, if
`XDG_CONFIG_HOME` is unset `~/.config/beer/beer.toml`. If the configuration file
is missing, then Beer uses its defaults. A malformed file logs a warning and
falls back to defaults. Unknown keys are ignored for forward compatibility.

Example:

```toml
[main]
font = "monospace"
font-size = 16
pad-x = 2
pad-y = 2

[colors]
background = "#181818"
foreground = "#c5c8c6"
alpha = 1.0

[key-bindings]
"Ctrl+Shift+C" = "copy"
"Ctrl+Shift+V" = "paste"
"Ctrl+Shift+F" = "search"

[url]
launch = ["xdg-open"]
```

See `doc/beer.toml.5.scd` for the full configuration reference.

## Development

The expected verification set is:

```sh
# Check Rust formatting.
$ cargo fmt --all -- --check

# Run Clippy with warnings denied.
$ cargo clippy --all-targets --all-features -- -D warnings

# Build a release-optimized binary.
$ cargo build --release

# Run the test suite.
$ cargo test

# Check dependency policy.
$ cargo deny check

# Verify the Nix package.
$ nix build
```

The application crate has internal modules for the PTY, VT model, grid, font
pipeline, renderer, configuration, and graphics; platform integration lives in
`beer-wayland`, while reusable protocol codecs and key/mouse encoders live in
`beer-protocols`. The platform-neutral seam is defined by `beer-window`, and
daemon framing is kept in `beer-ipc`.

## License

EUPL-1.2.
