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

        rustToolchain = pkgs.rust-bin.stable.latest.default;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            (pkgs.lib.hasSuffix "\.sql" path) ||
            (pkgs.lib.hasSuffix "\.css" path) ||
            (pkgs.lib.hasSuffix "\.js" path) ||
            (pkgs.lib.hasSuffix "\.svg" path) ||
            (pkgs.lib.hasSuffix "/.sqlx" path) ||
            (pkgs.lib.hasInfix "/.sqlx/" path) ||
            (craneLib.filterCargoSources path type)
          ;
        };
        commonArgs = {
          inherit src;
          preBuild = ''
            export SQLX_OFFLINE=true
          '';
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        bin = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });

        docker = pkgs.dockerTools.buildLayeredImage {
          name = "ordo";
          tag = "latest";
          created = "now";
          config.Cmd = [ "${bin}/bin/ordo" "--database=ordo.db" ];
          config.Expose = "3030";
        };
      in
      with pkgs;
      {
        packages = {
          inherit bin docker;
          default = bin;
        };

        devShells.default = mkShell {
          buildInputs = [
            emmet-ls
            sqlx-cli
            cargo-watch
            rust-analyzer
            rustToolchain

            typescript-language-server
            vscode-langservers-extracted

            nodejs
            pnpm
          ];

          shellHook = ''
            export DATABASE_URL="sqlite:test.db"
          '';
        };

        formatter = nixpkgs-fmt;
      }
    );
}
