# beer-wayland

`beer-wayland` is the Wayland backend for the `beer` terminal. It owns the
smithay-client-toolkit connection, calloop event loop, Wayland surfaces, input
handlers, clipboard integration, frame callbacks, and `wl_shm` buffer ring.

[`beer-window::App`]: ../beer-window/README.md

It drives an application implementing [`beer-window::App`] and exposes the
backend entry point as `beer_wayland::run`. Terminal state stays in the
application crate; this backend translates compositor events into the
compact `beer-window` application/backend vocabulary and routes platform
actions back to the compositor. There's not much that interests you here unless
you're a developer. Build or test this crate with:

```sh
# Build beer-wayland
$ cargo build -p beer-wayland

# Run the tests for the wayland crate
$ cargo test -p beer-wayland
```

## License

EUPL-1.2.
