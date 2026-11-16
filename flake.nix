{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} ({config, ...}: let
      flakePartsConfig = config;
    in {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      debug = true;

      flake = {
        nixosModules = {
          ouro = {
            config,
            lib,
            pkgs,
            ...
          }: {
            options = let
              inherit (lib) types;
            in {
              vpnNamespaces = lib.mkOption {
                type = types.attrsOf (types.submodule (vpnNamespaceModule @ {name, ...}: {
                  options = {
                    ouro = lib.mkOption {
                      default = {};
                      type = types.submodule (ouroSubmodule: {
                        options = {
                          enable = lib.mkEnableOption "ouro module subsystem for vpnNamespaces";
                          ipv6 = lib.mkEnableOption "IPv6 VPN peer firewall rules";
                          gateway = lib.mkOption {
                            type = types.str;
                            default = "10.2.0.1";
                            description = ''
                              Gateway IP Address of VPN Subnet.
                              Use `sudo ip netns exec ${name} bash -c "ip a"` to find out.
                              For Proton, default is `10.2.0.1`
                            '';
                          };
                          interface = lib.mkOption {
                            type = types.str;
                            default = "${name}0";
                            description = ''
                              Interface of the VPN connection in the confinement.
                              Use `sudo ip netns exec ${name} bash -c "ip a"` to find out.
                              Usually should be `${name}0`, but do double-check it.
                            '';
                          };
                          slskd = lib.mkOption {
                            default = {};
                            type = types.submodule {
                              options = {
                                enable = lib.mkEnableOption "Soulseek support";
                              };
                            };
                          };
                          transmission = lib.mkOption {
                            default = {};
                            type = types.submodule {
                              options = {
                                enable = lib.mkEnableOption "Soulseek support";
                                # TODO: needs modificaitons in `ouro` itself
                                #       to allow loading from files
                                # rpc_file = ...
                                RPC_USER = lib.mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                RPC_PASS = lib.mkOption {
                                  type = types.nullOr types.str;
                                  default = null;
                                };
                                ADDRESS = lib.mkOption {
                                  type = types.str;
                                  default = "127.0.0.1:9091";
                                  description = ''
                                    The address and port that ouro will provide to `transmission-remote`
                                    for it to connect to. Can be <IP>:<Port> or just <IP>.
                                  '';
                                };
                              };
                            };
                          };
                        };
                      });
                    };
                  };
                }));
              };
            };
            # TODO(reo101): assert that only one of `slskd` and `transmission` are enabled
            # slskd.enable -> !transmission.enable
            # transmission.enable -> !slskd.enable
            config = {
              systemd.services = lib.pipe config.vpnNamespaces [
                # name = { ..., ouro = { STUFF }, ... }
                (lib.mapAttrs (vpnNamespace: vpnNamespaceConfig: vpnNamespaceConfig.ouro))
                # name = { STUFF }
                (lib.filterAttrs (vpnNamespace: ouroConfig: ouroConfig.enable))
                # name = { enable = true, STUFF }
                (lib.mapAttrs' (vpnNamespace: ouroConfig:
                  lib.nameValuePair "ouro-${vpnNamespace}" {
                    description = ''
                      Ouro stuff for ${vpnNamespace}
                    '';
                    wantedBy =
                      if ouroConfig.transmission.enable && !ouroConfig.slskd.enable
                      then ["transmission.service"]
                      else ["multi-user.target"];
                    after = ["slskd.service" "${vpnNamespace}.service"]
                      ++ lib.optional (ouroConfig.transmission.enable && !ouroConfig.slskd.enable) "transmission.service";
                    partOf = lib.optional (ouroConfig.transmission.enable && !ouroConfig.slskd.enable) "transmission.service";
                    environment = {
                      # TODO(reo101): toggle with a module option
                      "RUST_LOG" = "debug";
                    };
                    path = [
                      pkgs.iptables
                    ] ++ lib.optionals ouroConfig.transmission.enable [
                      # NOTE: used for `transmission-remote`
                      config.services.transmission.package
                    ];
                    serviceConfig = let
                      inherit
                        (flakePartsConfig.allSystems.${pkgs.stdenv.hostPlatform.system}.packages)
                        ouro
                        ;
                      inherit
                        (ouroConfig)
                        gateway
                        interface
                        ;
                      ouroArgs = if ouroConfig.slskd.enable then
                        "slskd"
                      else if ouroConfig.transmission.enable then
                        if (ouroConfig.transmission.RPC_USER == null) != (ouroConfig.transmission.RPC_PASS == null) then
                          throw "Both RPC_USER and RPC_PASS must be set, or both left unset."
                        else
                          "transmission --address ${ouroConfig.transmission.ADDRESS}"
                          + lib.optionalString (ouroConfig.transmission.RPC_USER != null)
                            " --rpc-user ${ouroConfig.transmission.RPC_USER} --rpc-pass ${ouroConfig.transmission.RPC_PASS}"
                      else
                        throw "Either `slskd` or `transmission` need to be enabled!";
                    in {
                      ExecStart = ''
                        ${lib.getExe' pkgs.iproute2 "ip"} netns exec ${vpnNamespace} \
                          ${lib.getExe pkgs.bashNonInteractive} -c "${lib.getExe ouro} --gateway ${gateway} --vpn-interface ${interface} ${lib.optionalString ouroConfig.ipv6 "--ipv6 "}${ouroArgs}"
                      '';
                      Restart = "always";
                    };
                    vpnConfinement = {
                      enable = true;
                      inherit vpnNamespace;
                    };
                  }))
              ];
            };
          };
          default = config.flake.nixosModules.ouro;
        };
      };

      perSystem = {
        config,
        self',
        inputs',
        pkgs,
        lib,
        system,
        ...
      }: {
        _module.args.pkgs = import inputs.nixpkgs {
          inherit system;
          overlays = [
            inputs.rust-overlay.overlays.default
          ];
        };

        packages = {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "ouro";
            version = "0.1.5";

            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./src
                ./Cargo.toml
                ./Cargo.lock
              ];
            };

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = with pkgs; [
              pkg-config
            ];

            meta = {
              mainProgram = "ouro";
            };
          };

          ouro = self'.packages.default;
        };

        devShells = {
          default = pkgs.mkShell rec {
            buildInputs = with pkgs; [
              (rust-bin.stable.latest.default.override (oldAttrs: {
                extensions =
                  (oldAttrs.extensions or [])
                  ++ [
                    "rust-analyzer"
                  ];
              }))
              pkg-config
              clang
            ];

            shellHook = ''
              export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath buildInputs}"
            '';
          };
        };
      };
    });
}
