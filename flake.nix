{
  description = "swarm-discovery dev shell";

  inputs = {
    nixpkgs.url      = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url  = "github:numtide/flake-utils";
    crate2nix.url    = "github:nix-community/crate2nix";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, crate2nix, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        cargoNix = (import ./Cargo.nix {
          inherit pkgs;
          release = true;
        });
      in
      {
        packages.default = cargoNix.rootCrate.build;

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rust-bin.nightly.latest.default
            rust-analyzer
            crate2nix.packages.${system}.default
          ];
        };
      }
    );
}
