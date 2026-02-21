{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-parts.url = "github:hercules-ci/flake-parts";
    crate2nix = {
      url = "github:nix-community/crate2nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    cache-nix-action = {
      url = "github:nix-community/cache-nix-action";
      flake = false;
    };
    systems.url = "github:nix-systems/default";
  };

  outputs =
    inputs@{ flake-parts, systems, cache-nix-action, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = import systems;

      flake = {
        nixosModules.default = import ./nix/module.nix inputs;
      };

      perSystem =
        { system, ... }:
        let
          fenixPkgs = inputs.fenix.packages.${system};
          nightlyToolchain = fenixPkgs.toolchainOf {
            channel = "nightly";
            date = "2025-10-31";
            sha256 = "sha256-SqCt0WGZzKVpy2f6PPjLyfFsAs4wmjp3Jokaa/OdaNY=";
          };
          wasmTarget = fenixPkgs.targets.wasm32-unknown-unknown.toolchainOf {
            channel = "nightly";
            date = "2025-10-31";
            sha256 = "sha256-SqCt0WGZzKVpy2f6PPjLyfFsAs4wmjp3Jokaa/OdaNY=";
          };
          rustToolchain = fenixPkgs.combine [
            nightlyToolchain.completeToolchain
            wasmTarget.rust-std
          ];

          pkgs = import inputs.nixpkgs {
            inherit system;
          };

          cargoNix = inputs.crate2nix.tools.${system}.appliedCargoNix {
            name = "mangochill";
            src = ./.;
          };

          mangochill = (
            cargoNix.workspaceMembers.mangochill.build.override {
              crateOverrides = pkgs.defaultCrateOverrides // {
                mangochill = attrs: {
                  extraRustcOpts = attrs.extraRustcOpts ++ [
                    "-C"
                    "target-cpu=native"
                  ];
                  nativeBuildInputs = [ pkgs.capnproto ];
                };
              };
            }
          ).overrideAttrs {
            meta.mainProgram = "mangochill-server";
          };

          mangosrc = pkgs.fetchFromGitHub {
            owner = "farnoy";
            repo = "MangoHud";
            rev = "ae5a7dcf49227af0548cfd4d11ae3754c2fb9cb8";
            sha256 = "sha256-vv5b7eSei/qCtysKf53IeoZZifldXphpqUDMDKO12sU=";
          };
          mangoSubprojects = {
            imgui = rec {
              version = "1.91.6";
              src = pkgs.fetchFromGitHub {
                owner = "ocornut";
                repo = "imgui";
                tag = "v${version}";
                hash = "sha256-CLS26CRzzY4vUBgILjSQVvziHMyPGK4fwwcLZcOAzPw=";
              };
              patch = pkgs.fetchurl {
                url = "https://wrapdb.mesonbuild.com/v2/imgui_${version}-3/get_patch";
                hash = "sha256-L3l3EUugfQZVmq+IkKkqTr0lGGWS1ER5VGBaryJEY00=";
              };
            };
            implot = rec {
              version = "0.16";
              src = pkgs.fetchFromGitHub {
                owner = "epezent";
                repo = "implot";
                tag = "v${version}";
                hash = "sha256-/wkVsgz3wiUVZBCgRl2iDD6GWb+AoHN+u0aeqHHgem0=";
              };
              patch = pkgs.fetchurl {
                url = "https://wrapdb.mesonbuild.com/v2/implot_${version}-1/get_patch";
                hash = "sha256-HGsUYgZqVFL6UMHaHdR/7YQfKCMpcsgtd48pYpNlaMc=";
              };
            };
            vulkan-headers = rec {
              version = "1.3.283";
              src = pkgs.fetchFromGitHub {
                owner = "KhronosGroup";
                repo = "Vulkan-Headers";
                tag = "v${version}";
                hash = "sha256-DpbTYlEJPtyf/m9QEI8fdAm1Hw8MpFd+iCd7WB2gp/M=";
              };
              patch = pkgs.fetchurl {
                url = "https://wrapdb.mesonbuild.com/v2/vulkan-headers_${version}-1/get_patch";
                hash = "sha256-AOMNNRF66QoZtbiHh0b86vMbQXeLgXyp5rOuYGO+gjM=";
              };
            };
          };
          mangoOverride = oldAttrs: {
            src = mangosrc;
            postUnpack = ''
              (
                cd "$sourceRoot/subprojects"
                cp -R --no-preserve=mode,ownership ${mangoSubprojects.imgui.src} imgui-${mangoSubprojects.imgui.version}
                cp -R --no-preserve=mode,ownership ${mangoSubprojects.implot.src} implot-${mangoSubprojects.implot.version}
                cp -R --no-preserve=mode,ownership ${mangoSubprojects.vulkan-headers.src} Vulkan-Headers-${mangoSubprojects.vulkan-headers.version}
              )
            '';
            postPatch = ''
              substituteInPlace bin/mangohud.in \
                --subst-var-by libraryPath ${
                  pkgs.lib.makeSearchPath "lib/mangohud" [
                    (placeholder "out")
                  ]
                } \
                --subst-var-by version "${oldAttrs.version}" \
                --subst-var-by dataDir ${placeholder "out"}/share

              (
                cd subprojects
                unzip ${mangoSubprojects.imgui.patch}
                unzip ${mangoSubprojects.implot.patch}
                unzip ${mangoSubprojects.vulkan-headers.patch}
              )
            '';
          };
          mangohud =
            (pkgs.mangohud.override {
              mangohud32 = pkgs.pkgsi686Linux.mangohud.overrideAttrs mangoOverride;
            }).overrideAttrs mangoOverride;

          devShell = pkgs.mkShell {
            nativeBuildInputs = [
              pkgs.capnproto
              pkgs.wasm-pack
              rustToolchain
            ];
          };
        in
        {
          packages = {
            mangochill = mangochill;
            mangochill-server = mangochill;
            mangochill-client = mangochill.overrideAttrs { meta.mainProgram = "mangochill-client"; };
            mangohud = mangohud;
          };

          devShells.default = devShell;

          packages.saveFromGC =
            (import "${cache-nix-action}/saveFromGC.nix" {
              inherit pkgs inputs;
              inputsInclude = [
                "nixpkgs"
                "crate2nix"
                "flake-parts"
                "fenix"
                "systems"
              ];
              derivations = [
                devShell
              ];
              paths = [];
            }).package;
        };
    };
}
