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
  harfbuzz,
}: let
  cargoTOML = lib.importTOML ../Cargo.toml;
in
  rustPlatform.buildRustPackage (finalAttrs: {
    pname = "beer";
    version = cargoTOML.workspace.package.version;

    src = let
      fs = lib.fileset;
      s = ../.;
    in
      fs.toSource {
        root = s;
        fileset = fs.unions [
          (s + /crates)
          (s + /Cargo.lock)
          (s + /Cargo.toml)
          (s + /terminfo)
          (s + /doc)
          (s + /contrib)
        ];
      };

    cargoLock.lockFile = "${finalAttrs.src}/Cargo.lock";
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
      harfbuzz
    ];

    # Install the terminfo entry, and make the Wayland/xkb libraries (loaded via
    # dlopen, so not captured by rpath) discoverable at runtime.
    postInstall = ''
      mkdir -p "$out/share/terminfo"
      tic -x -o "$out/share/terminfo" terminfo/beer.info

      # Generate the man pages from their scdoc sources.
      scdoc < doc/beer.1.scd > beer.1
      scdoc < doc/beer.toml.5.scd > beer.toml.5
      scdoc < doc/beer-themes.7.scd > beer-themes.7
      installManPage beer.1 beer.toml.5 beer-themes.7

      # Desktop entry, scalable icon, and a commented example config.
      install -Dm644 contrib/dev.notashelf.beer.desktop \
        "$out/share/applications/dev.notashelf.beer.desktop"
      install -Dm644 contrib/dev.notashelf.beer.svg \
        "$out/share/icons/hicolor/scalable/apps/dev.notashelf.beer.svg"
      install -Dm644 contrib/beer.toml "$out/share/doc/beer/beer.toml.example"

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
