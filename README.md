# 🍺 beer

**A terminal worth pouring time into.**

A small, fast, Wayland-native terminal emulator written in Rust drawn entirely
on the CPU, with no GPU pipeline, no tabs, and no async runtime to get in the
way.

## Why another terminal?

[foot]: https://codeberg.org/dnkl/foot

Two reasons, actually. The first one and the one that motivated me try and build
my own terminal is, obviously, that I just wanted to. While I don't necessarily
find Wayland interesting, I've been meaning to get a bit acquainted with it for
personal reasons. That, combined with my personal disagreements with foot's
handling of fullscreen transparency, and a C codebase I found hard to keep up to
date _if_ I were to fork, I decided to look for a new terminal.

Most terminals reach for the GPU and a stack of abstractions before they draw a
single glyph. `beer` goes the other way. It takes its cues from [foot] to keep
the moving parts few, render with the CPU through `wl_shm`, talk to a real PTY,
and stay out of the way. Since I could not identify and settle with a terminal
that checks all the boxes, I decided to write one for real.

The result is a terminal that starts instantly, sits quietly in the background,
and spends its cycles on your shell rather than on itself. It is the kind of
tool you forget is running which, for a terminal, is the highest compliment.
Pour one, get to work.

## Project status

`beer` is pre-1.0 but already comfortable as a daily driver on Wayland. It
renders truecolor text with styled attributes and colour emoji, scrolls back
with incremental search, handles mouse selection and clipboard, follows the
compositor's fractional scale, and speaks the kitty keyboard, graphics, and
text-sizing protocols. It runs standalone or as a daemon that hosts many windows
from one process.

Feature requests are welcome, but not everything will be implemented.

> [!TIP]
> Manual pages (`beer(1)`, `beer.toml(5)`, `beer-themes(7)`) are generated from
> `doc/` with scdoc and installed by the package; a commented starting config
> lives at [`contrib/beer.toml`](contrib/beer.toml).

## What's in here

This is a Cargo workspace with five production crates and one separate fuzzing
crate:

- **[`crates/beer`](crates/beer/README.md)** - the terminal application itself.
  Start here for the feature list, installation, configuration, and day-to-day
  use.
- **[`crates/beer-protocols`](crates/beer-protocols/README.md)** - the reusable,
  self-documenting building blocks the terminal is made of, and a readable
  reference for the escape sequences and protocols `beer` speaks.
- **[`crates/beer-ipc`](crates/beer-ipc/README.md)** - the Unix-socket protocol
  used by daemon and client processes.
- **[`crates/beer-window`](crates/beer-window/README.md)** - the
  platform-neutral application/windowing contract.
- **[`crates/beer-wayland`](crates/beer-wayland/README.md)** - the Wayland
  backend that drives the application contract.

`cargo-fuzz` targets for protocol parsers and decoders.

If you just want to run it, head to the
[application README](crates/beer/README.md). If you are curious how a terminal
actually talks to the programs inside it, the
[protocols README](crates/beer-protocols/README.md) is a friendly tour.

## License

[EUPL-1.2](https://joinup.ec.europa.eu/collection/eupl/eupl-text-eupl-12).
