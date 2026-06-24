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
  ncurses,
  wayland,
  wayland-protocols,
  wayland-scanner,
  libxkbcommon,
  freetype,
  fontconfig,
  harfbuzz,
}:
mkShell {
  name = "beer-dev";

  strictDeps = true;
  nativeBuildInputs = [
    cargo
    rustc
    clippy
    rustfmt
    taplo
    rust-analyzer
    cargo-deny
    pkg-config
    ncurses # tic, to compile the terminfo entry
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

  # Compile the beer terminfo into a project-local database and put it ahead of
  # the system one, so TERM=beer resolves inside the shell (and for the shell
  # beer spawns, which inherits this env). The trailing ':' keeps the system
  # database on the search path.
  shellHook = ''
    if tic -x -o "$PWD/.terminfo" terminfo/beer.info 2>/dev/null; then
      export TERMINFO_DIRS="$PWD/.terminfo:"
    fi
  '';
}
