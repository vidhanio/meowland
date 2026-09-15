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
        ...
      }:
      {
        imports = [ inputs.treefmt-nix.flakeModule ];

        systems = import inputs.systems;

        perSystem =
          {
            system,
            pkgs,
            self',
            config,
            ...
          }:
          let
            craneLib = (inputs.crane.mkLib pkgs).overrideToolchain (
              p:
              p.rust-bin.nightly.latest.default.override {
                extensions = [
                  "rust-src"
                  "rust-analyzer"
                ];
              }
            );

            src = craneLib.cleanCargoSource ./.;

            # `libEGL` is loaded at runtime rather than linked, so it has to be
            # in the closure for `MEOWLAND_GPU_BUFFERS` to work at all.
            buildInputs = [
              pkgs.libglvnd
              pkgs.libxkbcommon
            ];

            nativeBuildInputs = [ pkgs.pkg-config ];

            commonArgs = {
              inherit
                src
                buildInputs
                nativeBuildInputs
                ;
              strictDeps = true;
              cargoExtraArgs = "--locked";
            };

            cargoArtifacts = craneLib.buildDepsOnly commonArgs;

            meowland = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                meta.mainProgram = "meowland";
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

              env.CARGO_NET_GIT_FETCH_WITH_CLI = "true";

              # `libEGL` is `dlopen`ed at runtime, which the loader only finds
              # through its search path - `buildInputs` alone will not do. The
              # driver itself is left to the system's own vendor setup.
              shellHook = ''
                export LD_LIBRARY_PATH=${
                  pkgs.lib.makeLibraryPath [ pkgs.libglvnd ]
                }''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
              '';

              # `foot` is a wl_shm-only client: the reference app for testing
              # that this compositor can actually host something.
              packages = [
                pkgs.foot
                config.treefmt.build.wrapper
              ];

              inherit buildInputs nativeBuildInputs;
            };

            treefmt = {
              programs = {
                nixfmt.enable = true;
                statix.enable = true;
                deadnix.enable = true;
                rustfmt = {
                  enable = true;
                  package = pkgs.rust-bin.nightly.latest.rustfmt;
                };
                taplo.enable = true;
              };
            };
          };
      }
    );
}
