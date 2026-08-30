# beer-ipc

`beer-ipc` contains the small Unix-socket protocol used to connect `beer`
clients to a daemon process. It keeps daemon framing and socket discovery
separate from terminal state and Wayland objects.

## Protocol

`OpenRequest` carries the client's working directory, environment, command, hold
flag, title, and app-id. Requests are encoded as a big-endian `u32`
length-prefixed frame containing a length-prefixed UTF-8 working directory,
environment key/value pairs, and backward-compatible launch-option trailers.
The server returns one byte containing the window's exit status. Frames are
bounded, sockets are restricted to mode `0600`, and the runtime directory must
be private and owned by the current user. The socket is derived from
`$XDG_RUNTIME_DIR` and `$WAYLAND_DISPLAY`. When `LISTEN_PID` matches and
`LISTEN_FDS=1`, the server instead takes ownership of the activated listener on
file descriptor 3.

## Usage

This is an internal workspace library used by `beer` and `beer-wayland`; it is
not a standalone executable. Run the workspace tests with:

```sh
# Run the tests for the IPC crate
$ cargo test -p beer-ipc
```

## License

EUPL-1.2.
