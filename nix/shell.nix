{
  mkShell,
  rustc,
  cargo,
  rust-analyzer,
  rustfmt,
  clippy,
  taplo,
  cargo-deny,
  pkg-config,
  wayland,
  wayland-protocols,
  wayland-scanner,
  libxkbcommon,
  freetype,
  fontconfig,
  harfbuzz,
}:
mkShell {
  name = "rust";

  strictDeps = true;
  nativeBuildInputs = [
    cargo
    rustc
    clippy
    rustfmt
    rustc
    cargo

    # Tools
    rustfmt
    clippy
    cargo
    taplo

    # LSP
    rust-analyzer
    cargo-deny
    pkg-config
  ];

  buildInputs = [
    wayland
    wayland-protocols
    wayland-scanner
    libxkbcommon
    freetype
    fontconfig
    harfbuzz
  ];
}
