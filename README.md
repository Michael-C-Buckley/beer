# beer

A fast, software-rendered, **Wayland-native** terminal emulator written in Rust.
Lightweight in dependencies, on disk, and in memory.

## Build

A Nix dev shell provides the toolchain and native libraries:

```sh
# Enter a devshell for necessary deps
$ nix develop

# Build in release mode
cargo build --release
```

## License

EUPL-1.2.
