{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    systems.url = "github:nix-systems/default-linux";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } (
      {
        inputs,
        self,
        ...
      }:
      {
        imports = [ inputs.treefmt-nix.flakeModule ];

        systems = import inputs.systems;

        flake.homeModules.default =
          {
            config,
            lib,
            pkgs,
            ...
          }:
          let
            cfg = config.programs.meowland;
          in
          {
            options.programs.meowland = {
              enable = lib.mkEnableOption "meowland";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
                description = "Package to install for Meowland.";
              };
            };

            config = lib.mkIf cfg.enable {
              home.packages = [ cfg.package ];

              systemd.user.services.meowland = {
                Unit.Description = "meowland Wayland compositor";

                Service = {
                  Type = "simple";
                  ExecStart = "${lib.getExe cfg.package} server";
                  Restart = "on-failure";
                  RestartSec = "1s";
                };

                Install.WantedBy = [ "default.target" ];
              };
            };
          };

        perSystem =
          {
            system,
            pkgs,
            self',
            config,
            ...
          }:
          let
            nightlyToolchain =
              p:
              p.rust-bin.nightly.latest.default.override {
                extensions = [
                  "rust-src"
                  "rust-analyzer"
                ];
              };
            craneLib = (inputs.crane.mkLib pkgs).overrideToolchain nightlyToolchain;

            # Keep rustfmt.toml in crane's filtered source for nightly formatting.
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter =
                path: type:
                craneLib.filterCargoSources path type || pkgs.lib.baseNameOf (toString path) == "rustfmt.toml";
            };

            nativeBuildInputs = [ pkgs.pkg-config ];

            buildInputs = [
              pkgs.libglvnd
              pkgs.libxkbcommon
              pkgs.libgbm
            ];

            commonArgs = {
              inherit
                src
                buildInputs
                nativeBuildInputs
                ;
              strictDeps = true;
              cargoExtraArgs = "--locked";
              LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.libglvnd ];
            };

            cargoArtifacts = craneLib.buildDepsOnly commonArgs;

            meowland = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                meta.mainProgram = "meowland";

                nativeBuildInputs = nativeBuildInputs ++ [
                  pkgs.installShellFiles
                  pkgs.makeWrapper
                ];

                postInstall = ''
                  wrapProgram $out/bin/meowland \
                    --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath [ pkgs.libglvnd ]}

                  install -Dm644 ${./share/systemd/user/meowland.service} \
                    $out/share/systemd/user/meowland.service
                  substituteInPlace $out/share/systemd/user/meowland.service \
                    --replace-fail 'ExecStart=meowland ' "ExecStart=$out/bin/meowland "

                  installShellCompletion --cmd meowland \
                    --bash <($out/bin/meowland completions bash) \
                    --fish <($out/bin/meowland completions fish) \
                    --zsh <($out/bin/meowland completions zsh)
                '';
              }
            );
          in
          {
            _module.args.pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [ inputs.rust-overlay.overlays.default ];
            };

            packages.default = meowland;

            checks = {
              clippy = craneLib.cargoClippy (
                commonArgs
                // {
                  inherit cargoArtifacts;
                  cargoClippyExtraArgs = "--all-targets --all-features -- -D warnings";
                }
              );

              test = craneLib.cargoTest (
                commonArgs
                // {
                  inherit cargoArtifacts;
                  cargoTestExtraArgs = "--all-targets --all-features";
                }
              );

              fmt = craneLib.cargoFmt { inherit src; };
            };

            devShells.default = craneLib.devShell {
              inherit (self') checks;

              env = {
                CARGO_NET_GIT_FETCH_WITH_CLI = "true";
                LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.libglvnd ];
              };

              packages = [ config.treefmt.build.wrapper ];

              inherit buildInputs nativeBuildInputs;
            };

            treefmt = {
              programs = {
                nixfmt.enable = true;
                statix.enable = true;
                deadnix.enable = true;
                rustfmt = {
                  enable = true;
                  package = nightlyToolchain pkgs;
                };
                taplo.enable = true;
              };
            };
          };
      }
    );
}
