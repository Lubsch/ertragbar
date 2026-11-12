{
  outputs =
    { self, nixpkgs }:
    let
      mapSystems = f: builtins.mapAttrs f nixpkgs.legacyPackages;

      # Adwaita Sans cut down to what the bar can draw: printable ASCII plus
      # the battery arrows/dash, minus hinting and OpenType layout tables
      # (fontdue uses neither). ~43 KB instead of ~880 KB, which fontdue parses
      # in ~0.5ms and ~0.4 MiB instead of ~16ms and ~10 MiB. Embedded into
      # the binary (see FONT in src/render.rs); if you add text with other
      # characters, extend the list -- the font_covers_all_text test fails
      # until you do.
      mkFont =
        pkgs:
        pkgs.runCommand "ertragbar-font.ttf"
          { nativeBuildInputs = [ pkgs.python3Packages.fonttools ]; }
          ''
            pyftsubset ${pkgs.adwaita-fonts}/share/fonts/Adwaita/AdwaitaSans-Regular.ttf \
              --unicodes=U+0020-007E,U+2014,U+25B2,U+25BC \
              --no-hinting --layout-features= \
              --output-file=$out
          '';
    in
    {
      packages = mapSystems (
        _: pkgs: rec {
          font = mkFont pkgs;
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

            ERTRAGBAR_FONT = font;

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
            ERTRAGBAR_FONT = mkFont pkgs;
          };
        }
      );
    };
}
