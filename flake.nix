{
  outputs =
    { self, nixpkgs }:
    let
      mapSystems = f: builtins.mapAttrs f nixpkgs.legacyPackages;
    in
    {

      devShells = mapSystems (
        _: pkgs: {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              cargo-show-asm
            ];
          };
        }
      );
    };
}
