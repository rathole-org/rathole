{
  description = "A lightweight and high-performance reverse proxy for NAT traversal";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      overlays.default = final: prev: {
        rathole = final.rustPlatform.buildRustPackage {
          pname = "rathole";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = final.lib.fileset.toSource {
            root = ./.;
            fileset = final.lib.fileset.unions [
              ./src
              ./build.rs
              ./Cargo.toml
              ./Cargo.lock
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ final.pkg-config ];
          buildInputs = [ final.zlib ] ++ final.lib.optionals final.stdenv.isLinux [ final.openssl ];
          doCheck = false;
          meta = {
            description = "A lightweight and high-performance reverse proxy for NAT traversal";
            homepage = "https://github.com/rathole-org/rathole";
            license = final.lib.licenses.asl20;
            mainProgram = "rathole";
          };
        };
      };

      packages = forAllSystems (
        pkgs:
        let
          pkgs' = pkgs.extend self.overlays.default;
        in
        {
          rathole = pkgs'.rathole;
          default = pkgs'.rathole;
        }
      );

      apps = forAllSystems (pkgs: rec {
        rathole = {
          type = "app";
          program = pkgs.lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.rathole;
        };
        default = rathole;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.rathole ];
          packages = with pkgs; [
            rustfmt
            clippy
            rust-analyzer
          ];
        };
      });

      checks = forAllSystems (pkgs: {
        rathole = self.packages.${pkgs.stdenv.hostPlatform.system}.rathole;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
