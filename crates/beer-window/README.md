# beer-window

`beer-window` defines the platform-neutral seam between the terminal application
and a window-system backend.

The `App` trait owns terminal behavior and receives neutral keyboard, pointer,
touch, IME, timer, file-descriptor, clipboard, and rendering callbacks. The
`WindowCtx` trait gives it the platform operations needed to create windows,
present CPU-painted frames, update titles and cursors, handle selection, and
schedule work. `WindowId` and the event enums keep Wayland types out of the
application layer.

## Development

```sh
# Run the tests for beer-window
$ cargo test -p beer-window
```

## License

EUPL-1.2.
