{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        flake-utils.follows = "flake-utils";
      };
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs = {
        nixpkgs.follows = "nixpkgs";
      };
    };
  };
  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        craneLib = crane.lib.${system};
        src = craneLib.cleanCargoSource ./.;

        commonArgs = { inherit src; };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        bin = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });
      in
      with pkgs;
      {
        packages = {
          inherit bin;
          default = bin;
        };

        devShells.default = mkShell {
          buildInputs = [
            emmet-ls
            sqlx-cli
            cargo-watch
            rust-analyzer
            rust-bin.stable.latest.default
            nodePackages.typescript-language-server
            nodePackages.vscode-langservers-extracted
          ];

          shellHook = ''
            export DATABASE_URL="sqlite:test.db"
          '';
        };

        formatter = nixpkgs-fmt;
      }
    );
}
