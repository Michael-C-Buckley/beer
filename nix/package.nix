{
  lib,
  rustPlatform,
}:
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "beer";
  version = "0.0.0";

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
      ];
    };

  cargoLock.lockFile = "${finalAttrs.src}/Cargo.lock";
  enableParallelBuilding = true;

  meta = {
    maintainers = with lib.maintainers; [NotAShelf];
    mainProgram = "beer";
  };
})
