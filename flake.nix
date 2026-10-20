{
  description = "dev-zero: the dz CLI and TUI for dev-null.sh";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          package = pkgs.rustPlatform.buildRustPackage {
            pname = "dev-zero";
            version = manifest.package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
                ./scripts
                ./npm
                ./Formula
                ./README.md
                ./LICENSE
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.makeWrapper ];
            nativeCheckInputs = [
              pkgs.openssh
              pkgs.nodejs
            ];
            postPatch = ''
              patchShebangs npm/bin/dz.cjs
            '';
            postInstall = ''
              wrapProgram "$out/bin/dz" --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.openssh ]}
            '';
            meta = {
              description = manifest.package.description;
              homepage = manifest.package.repository;
              license = pkgs.lib.licenses.mit;
              mainProgram = "dz";
              platforms = systems;
            };
          };
        in
        {
          default = package;
          dev-zero = package;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          meta.description = "Browse dev-null.sh from the terminal";
          program = "${self.packages.${system}.default}/bin/dz";
        };
        dz = self.apps.${system}.default;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
              nodejs
              openssh
            ];
          };
        }
      );

      checks = forAllSystems (system: {
        build = self.packages.${system}.default;
      });
      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
