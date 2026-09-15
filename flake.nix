{
  description = "LinnStrument CLI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = (import nixpkgs { inherit system; }).pkgs;
      in {
        inherit pkgs;
        devShells.default = pkgs.stdenv.mkDerivation {
          name = "linnstrument-cli-shell";
          buildInputs = with pkgs; [
            rustup
          ];
        };
      }
    );
}
