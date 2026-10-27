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
            serviceCfg = config.services.meowland;
            programCfg = config.programs.meowland;
            format = pkgs.formats.toml { };
            packageOption =
              lib.mkPackageOption {
                meowland = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              } "meowland" { }
              // {
                defaultText = lib.literalExpression "inputs.meowland.packages.\${pkgs.stdenv.hostPlatform.system}.default";
              };
          in
          {
            options = {
              programs.meowland = {
                enable = lib.mkEnableOption "Meowland, a terminal Wayland compositor";
                package = packageOption;
              };

              services.meowland = {
                enable = lib.mkEnableOption "the socket-activated Meowland compositor service";
                package = packageOption;
                settings = lib.mkOption {
                  inherit (format) type;
                  default = { };
                  example = {
                    output = {
                      scale = 1.5;
                      refresh_rate = 60;
                    };
                    keyboard = {
                      layout = "us";
                      repeat_delay = 600;
                      repeat_rate = 25;
                    };
                  };
                  description = ''
                    Configuration written to
                    {file}`$XDG_CONFIG_HOME/meowland/config.toml`.

                    Output settings are `scale` (0.5–4) and `refresh_rate`
                    (1–1000 Hz). Keyboard settings are the XKB `layout`,
                    `variant`, and `options` strings, plus `repeat_delay`
                    in milliseconds and `repeat_rate` in events per second
                    (zero disables repeat). Unspecified settings use Meowland's
                    defaults. Changes take effect when the service restarts.
                  '';
                };
              };
            };

            config = lib.mkMerge [
              (lib.mkIf programCfg.enable {
                home.packages = [ programCfg.package ];
              })
              (lib.mkIf serviceCfg.enable {
                assertions = [
                  (lib.hm.assertions.assertPlatform "services.meowland" pkgs lib.platforms.linux)
                ];

                xdg.configFile."meowland/config.toml".source =
                  format.generate "meowland-config.toml" serviceCfg.settings;

                systemd.user.services.meowland = {
                  Unit = {
                    Description = "meowland Wayland compositor";
                    Requires = [ "meowland.socket" ];
                    After = [ "meowland.socket" ];
                    X-Restart-Triggers = [ "${config.xdg.configFile."meowland/config.toml".source}" ];
                  };

                  Service = {
                    Type = "notify";
                    NotifyAccess = "main";
                    KillMode = "mixed";
                    TimeoutStopSec = "5s";
                    ExecStart = "${lib.getExe serviceCfg.package} server";
                    Restart = "on-failure";
                    RestartSec = "1s";
                    UMask = "0077";
                  };

                  Install.Also = [ "meowland.socket" ];
                };

                systemd.user.sockets.meowland = {
                  Unit.Description = "meowland control and pane sockets";

                  Socket = {
                    ListenStream = [
                      "%t/meowland-control.sock"
                      "%t/meowland-pane.sock"
                    ];
                    SocketMode = "0600";
                    RemoveOnStop = true;
                  };

                  Install.WantedBy = [ "sockets.target" ];
                };
              })
            ];
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
            rustToolchain = p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
            craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter =
                path: type:
                craneLib.filterCargoSources path type
                || builtins.elem (pkgs.lib.baseNameOf (toString path)) [
                  "rustfmt.toml"
                  "clippy.toml"
                ];
            };

            nativeBuildInputs = [ pkgs.pkg-config ];

            buildInputs = [
              pkgs.libglvnd
              pkgs.libxkbcommon
              pkgs.libgbm
            ];

            commonArgs = {
              pname = "meowland";
              version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
              inherit
                src
                buildInputs
                nativeBuildInputs
                ;
              strictDeps = true;
              cargoExtraArgs = "--locked --workspace --all-features";
              LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.libglvnd ];
            };

            cargoArtifacts = craneLib.buildDepsOnly commonArgs;

            meowland = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                doCheck = false;
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
                  install -Dm644 ${./share/systemd/user/meowland.socket} \
                    $out/share/systemd/user/meowland.socket
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
                  cargoClippyExtraArgs = "--all-targets -- -D warnings";
                }
              );

              fmt = craneLib.cargoFmt { inherit src; };

              docs = craneLib.cargoDoc (
                commonArgs
                // {
                  inherit cargoArtifacts;
                  cargoDocExtraArgs = "--no-deps";
                  RUSTDOCFLAGS = "-D warnings";
                }
              );

              tests = craneLib.cargoTest (
                commonArgs
                // {
                  inherit cargoArtifacts;
                }
              );
            };

            devShells.default = craneLib.devShell {
              inherit (self') checks;

              env = {
                CARGO_NET_GIT_FETCH_WITH_CLI = "true";
                LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.libglvnd ];
              };

              packages = [
                config.treefmt.build.wrapper
                pkgs.cargo-audit
                pkgs.cargo-deny
                pkgs.cargo-edit
                pkgs.cargo-machete
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
                  package = rustToolchain pkgs;
                };
                taplo.enable = true;
              };
            };
          };
      }
    );
}
