{
  lib,
  rustPlatform,
  pkg-config,
  makeBinaryWrapper,
  ncurses,
  scdoc,
  installShellFiles,
  wayland,
  libxkbcommon,
  freetype,
  fontconfig,
}:
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "beer";
  version = "0.0.1";

  src = let
    fs = lib.fileset;
    s = ../.;
  in
    fs.toSource {
      root = s;
      fileset = fs.unions [
        (fs.fileFilter (file: builtins.any file.hasExt ["rs"]) (s + /src))
        (s + /Cargo.lock)
        (s + /Cargo.toml)
        (s + /terminfo)
        (s + /doc)
      ];
    };

  cargoLock.lockFile = "${finalAttrs.src}Cargo.lock";
  enableParallelBuilding = true;

  strictDeps = true;
  nativeBuildInputs = [
    pkg-config
    makeBinaryWrapper
    ncurses # tic
    scdoc # man page generation
    installShellFiles # installManPage
  ];

  buildInputs = [
    wayland
    libxkbcommon
    freetype
    fontconfig
  ];

  # Install the terminfo entry, and make the Wayland/xkb libraries (loaded via
  # dlopen, so not captured by rpath) discoverable at runtime.
  postInstall = ''
    mkdir -p "$out/share/terminfo"
    tic -x -o "$out/share/terminfo" terminfo/beer.info

    # Generate the man pages from their scdoc sources.
    scdoc < doc/beer.1.scd > beer.1
    scdoc < doc/beer.toml.5.scd > beer.toml.5
    installManPage beer.1 beer.toml.5

    wrapProgram "$out/bin/beer" \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [wayland libxkbcommon]}
  '';

  meta = {
    description = "A fast, software-rendered, Wayland-native terminal emulator";
    homepage = "https://github.com/NotAShelf/beer";
    license = lib.licenses.eupl12;
    maintainers = with lib.maintainers; [NotAShelf];
    mainProgram = "beer";
    platforms = lib.platforms.linux;
  };
})
