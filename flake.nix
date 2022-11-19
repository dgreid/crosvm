{
  description = "crosvm";

  inputs.flake-utils.url = "github:numtide/flake-utils";

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }: let
    inherit (flake-utils) lib;
    version = self.shortRev or "dirty";
    src = self;
  in
    lib.eachSystem [
      lib.system.aarch64-linux
      lib.system.x86_64-linux
    ] (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
      in rec {
        packages = lib.flattenTree rec {
          crosvm = pkgs.callPackage ./package.nix {inherit src version;};
          crosvm-aarch64 = pkgs.pkgsCross.aarch64-multiplatform.callPackage ./package.nix {inherit src version;};
          default = crosvm;
        };
        checks = (nixpkgs.lib.mapAttrs (name: pkg: pkg.override { doCheck = true; }) (builtins.removeAttrs packages [ "default" ]));
        formatter = pkgs.alejandra;
      }
    );
}
