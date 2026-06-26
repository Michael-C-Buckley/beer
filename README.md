# 🍺 beer

**A terminal worth pouring time into.**

A small, fast, Wayland-native terminal emulator written in Rust drawn entirely
on the CPU, with no GPU pipeline, no tabs, no ligatures, and no async runtime to
get in the way.

## Why another terminal?

[foot]: https://codeberg.org/dnkl/foot

Most terminals reach for the GPU and a stack of abstractions before they draw a
single glyph. `beer` goes the other way. It takes its cues from [foot] keep the
moving parts few, render with the CPU through `wl_shm`, talk to a real PTY, and
stay out of the way. The result is a terminal that starts instantly, sits
quietly in the background, and spends its cycles on your shell rather than on
itself. It is the kind of tool you forget is running which, for a terminal, is
the highest compliment. Pour one, get to work.

## Project status

`beer` is pre-1.0 but already comfortable as your daily single-window terminal
on Wayland. It's made clear in documentation about what it does and does not do
yet, and it would rather do a smaller set of things well than a larger set
half-heartedly. Feature requests are welcome, but not everything will be
implemented.

## What's in here

This is a Cargo workspace with two crates:

- **[`crates/beer`](crates/beer/README.md)** - the terminal application itself.
  Start here for the feature list, installation, configuration, and day-to-day
  use.
- **[`crates/beer-protocols`](crates/beer-protocols/README.md)** - the reusable,
  self-documenting building blocks the terminal is made of, and a readable
  reference for the escape sequences and protocols `beer` speaks.

If you just want to run it, head to the
[application README](crates/beer/README.md). If you are curious how a terminal
actually talks to the programs inside it, the
[protocols README](crates/beer-protocols/README.md) is a friendly tour.

## License

[EUPL-1.2](https://joinup.ec.europa.eu/collection/eupl/eupl-text-eupl-12).
