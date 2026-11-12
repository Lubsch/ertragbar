{
  outputs =
    { self, nixpkgs }:
    let
      mapSystems = f: builtins.mapAttrs f nixpkgs.legacyPackages;

      # Baked into the binary at build time (see FONT_PATH in src/main.rs).
      font = pkgs: "${pkgs.adwaita-fonts}/share/fonts/Adwaita/AdwaitaSans-Regular.ttf";
    in
    {
      packages = mapSystems (
        _: pkgs: rec {
          ertragbar = pkgs.rustPlatform.buildRustPackage {
            pname = "ertragbar";
            version = "0.1.0";

            # Only what cargo needs, so editing flake.nix etc. doesn't rebuild.
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;

            ERTRAGBAR_FONT = font pkgs;

            # Expects mango's `mmsg` on PATH at runtime.
            meta.mainProgram = "ertragbar";
          };
          default = ertragbar;
        }
      );

      devShells = mapSystems (
        _: pkgs: {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustfmt
              rustc
              clippy
              cargo-show-asm
              cargo-bloat
            ];
            ERTRAGBAR_FONT = font pkgs;
          };
        }
      );
    };
}
