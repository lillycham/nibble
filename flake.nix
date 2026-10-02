{
  description = "nibble: a small local-model harness for tasks that don't need a big agent";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});

      # MLX from Apple's PyPI wheels, as in tiny-lm. The nixpkgs build has no Metal
      # support, because Apple's Metal shader compiler can't run in the Nix sandbox.
      mlxWheel = pkgs: ps:
        let
          version = "0.32.2";
          wheel = args: ps.buildPythonPackage (args // { inherit version; format = "wheel"; });
          mlx-metal = wheel {
            pname = "mlx-metal";
            src = pkgs.fetchurl {
              url = "https://files.pythonhosted.org/packages/79/ec/34f37376e26d537fadffb99af3a760d6545e37f5e1a30a552baadf237fc5/mlx_metal-${version}-py3-none-macosx_15_0_arm64.whl";
              sha256 = "55a369250d220b2cf10213a87a2ac1b1a420608c5b35b1df4e7147ac8e32f121";
            };
          };
        in
        wheel {
          pname = "mlx";
          src = pkgs.fetchurl {
            url = "https://files.pythonhosted.org/packages/f8/c8/6928f4b9ca8f190c7c7a19c0a67920aa1742c62c6e66b05f7a1e21da728c/mlx-${version}-cp314-cp314-macosx_15_0_arm64.whl";
            sha256 = "8fc433e35a7058e30f7a225c39fce06ba41394fe8e9fd67383b70f0b06de398c";
          };
          # mlx/core.so looks for libmlx.dylib in its own mlx/lib, as a pip install
          # would have it. So put mlx-metal's lib there, and don't list mlx-metal as a
          # separate package.
          postInstall = ''
            cp -r ${mlx-metal}/${ps.python.sitePackages}/mlx/lib $out/${ps.python.sitePackages}/mlx/
          '';
          dontCheckRuntimeDeps = true;
          pythonImportsCheck = [ "mlx.core" ];
        };

      # mlx_lm.server on its own, without a python3 on PATH. nibble starts it on
      # demand, so nothing else needs to see the interpreter.
      mlxServer = pkgs:
        let
          python = pkgs.python3.override {
            self = python;
            packageOverrides = final: prev: {
              mlx = mlxWheel pkgs final;
              # The tests want a GPU, which the sandbox doesn't have. nixpkgs only
              # gets sentencepiece through the test inputs, but mlx-lm needs it at
              # runtime, so add it back as a real dependency.
              mlx-lm = prev.mlx-lm.overridePythonAttrs (old: {
                doCheck = false;
                dependencies = old.dependencies ++ [ final.sentencepiece ];
              });
            };
          };
          env = python.withPackages (ps: [ ps.mlx-lm ]);
        in
        pkgs.writeShellScriptBin "nibble-mlx-server" ''
          exec ${env}/bin/mlx_lm.server "$@"
        '';

      appleSilicon = pkgs: pkgs.stdenv.hostPlatform.isDarwin && pkgs.stdenv.hostPlatform.isAarch64;
    in
    {
      packages = forAll (pkgs: rec {
        nibble = pkgs.rustPlatform.buildRustPackage {
          pname = "nibble";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta.mainProgram = "nibble";
        };
        default = nibble;
      } // pkgs.lib.optionalAttrs (appleSilicon pkgs) {
        mlx-server = mlxServer pkgs;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc clippy rustfmt rust-analyzer ]
            ++ lib.optional (appleSilicon pkgs) (mlxServer pkgs);
        };
      });
    };
}
