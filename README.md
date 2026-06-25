# beer

`beer` is a small Wayland-native terminal emulator written in Rust. It renders
through `wl_shm` with the CPU, uses a real PTY for the child shell, and keeps
the architecture close to the model used by terminals like foot: no GPU
renderer, no tabs, no ligatures, and no async runtime.

The project is still pre-1.0, but it is already usable as a single-window
terminal on Wayland.

## Features

- Software-rendered Wayland window with server-side decoration negotiation.
- PTY-backed login shell with resize propagation through `TIOCSWINSZ`.
- VT parsing through `vte`, including cursor movement, erase/edit operations,
  scroll regions, alternate screen, SGR attributes, truecolor, 256-color, OSC
  palette/theme escapes, title updates, DA/DSR, XTGETTCAP, and synchronized
  output.
- Font discovery and rasterization through fontconfig and FreeType, including
  fallback faces, styled variants, bounded glyph caching, and color emoji.
- Scrollback with wheel and key scrolling, reflow on resize, and incremental
  search.
- Mouse selection with word, line, and rectangular selection; clipboard and
  primary-selection integration; bracketed paste; OSC 52 set/query.
- Mouse and focus reporting for terminal applications.
- TOML configuration with live reload on `SIGUSR1`.
- Shell integration through OSC 7 and OSC 133 for cwd-aware new windows, prompt
  jumping, and piping the last command's output.
- OSC 8 hyperlinks, visible URL hint mode, and OSC 9/777/99 notifications.

## Not Implemented Yet

- Daemon/server mode and client mode.
- Kitty keyboard protocol and Unicode codepoint input mode.
- Touchscreen input.
- Conformance and performance hardening passes beyond the current unit tests.

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

or after a release build:

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

`--server` exists as a future mode switch and currently returns an error.

## Configuration

By default, `beer` reads:

```text
$XDG_CONFIG_HOME/beer/beer.toml
```

or, if `XDG_CONFIG_HOME` is unset:

```text
~/.config/beer/beer.toml
```

A missing file uses defaults. A malformed file logs a warning and falls back to
defaults. Unknown keys are ignored for forward compatibility.

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

The code is organized as a single crate with internal modules for the PTY, VT
model, grid, font pipeline, renderer, input encoding, configuration, and Wayland
frontend. Keep feature logic in the module that owns the concept; avoid adding
more state directly to the Wayland event root or the core grid when a focused
submodule can own it.

## License

EUPL-1.2.
