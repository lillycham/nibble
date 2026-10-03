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
              mlx-lm = prev.mlx-lm.overridePythonAttrs (old: rec {
                # Ahead of nixpkgs (0.31.3), which can't load models converted
                # with the newer config names, such as LFM2.5. Drop this once
                # nixpkgs catches up.
                version = "0.32.0";
                src = pkgs.fetchFromGitHub {
                  owner = "ml-explore";
                  repo = "mlx-lm";
                  tag = "v${version}";
                  hash = "sha256-ZkzwImue0UJ+ZRNJVanziqEhdYSO2dS8Y9yQAIIDHK8=";
                };
                build-system = old.build-system ++ [ final.setuptools-scm ];
                doCheck = false;
                dependencies = old.dependencies ++ [ final.sentencepiece ];
                # A small model sometimes ends its turn without the closing
                # </tool_call> tag. The server then sees a stop with no state,
                # and throws the whole call away. Keep any call text that is
                # left when generation ends.
                postPatch = (old.postPatch or "") + ''
                  substituteInPlace mlx_lm/server.py \
                    --replace-fail 'if prev_state == "tool" and tool_text:' 'if tool_text:'
                '';
              });
            };
          };
          env = python.withPackages (ps: [ ps.mlx-lm ]);
        in
        pkgs.writeShellScriptBin "nibble-mlx-server" ''
          exec ${env}/bin/mlx_lm.server "$@"
        '';

      # The window's icon, drawn in gui/icon/nibble.svg and turned into the
      # .icns file an app bundle wants, at every size macOS asks for.
      appIcon = pkgs: pkgs.runCommand "nibble-icon" {
        nativeBuildInputs = [ pkgs.librsvg pkgs.libicns ];
      } ''
        for n in 16 32 128 256 512 1024; do
          rsvg-convert -w $n -h $n ${./gui/icon/nibble.svg} -o icon_$n.png
        done
        mkdir $out
        png2icns $out/nibble.icns icon_*.png
      '';

      appleSilicon = pkgs: pkgs.stdenv.hostPlatform.isDarwin && pkgs.stdenv.hostPlatform.isAarch64;
    in
    {
      packages = forAll (pkgs: rec {
        nibble = pkgs.rustPlatform.buildRustPackage {
          pname = "nibble";
          version = "0.1.0";
          # Only what the build reads, so a change to the GUI or the notes
          # doesn't rebuild the CLI.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./eval ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          # `nibble serve` starts nibble-mlx-server by name. Put it at the end
          # of PATH, so `nix run` works with no dev shell and no config file,
          # and anything the user has earlier on PATH still wins.
          nativeBuildInputs = pkgs.lib.optional (appleSilicon pkgs) pkgs.makeWrapper;
          postFixup = pkgs.lib.optionalString (appleSilicon pkgs) ''
            wrapProgram $out/bin/nibble --suffix PATH : ${mlxServer pkgs}/bin
          '';
          meta = {
            description = "A small local-model harness for tasks that don't need a big agent";
            license = pkgs.lib.licenses.mit;
            mainProgram = "nibble";
          };
        };
        default = nibble;
      } // pkgs.lib.optionalAttrs (appleSilicon pkgs) {
        mlx-server = mlxServer pkgs;
      } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
        # The native chat window. macOS only for now: GPUI on Linux needs the
        # Wayland and X11 libraries wired in, which nobody has tried here.
        nibble-gui = pkgs.rustPlatform.buildRustPackage {
          pname = "nibble-gui";
          version = "0.1.0";
          src = ./gui;
          cargoLock.lockFile = ./gui/Cargo.lock;
          # An app bundle around the binary, so it has a place in the Dock and
          # Spotlight can find it. bin/nibble-gui stays, as a link into it.
          postInstall = ''
            app=$out/Applications/nibble.app/Contents
            mkdir -p $app/MacOS $app/Resources
            cp ${appIcon pkgs}/nibble.icns $app/Resources/nibble.icns
            mv $out/bin/nibble-gui $app/MacOS/nibble-gui
            ln -s $app/MacOS/nibble-gui $out/bin/nibble-gui
            cat > $app/Info.plist <<EOF
            <?xml version="1.0" encoding="UTF-8"?>
            <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
            <plist version="1.0">
            <dict>
              <key>CFBundleName</key><string>nibble</string>
              <key>CFBundleDisplayName</key><string>nibble</string>
              <key>CFBundleIdentifier</key><string>com.lillycham.nibble</string>
              <key>CFBundleExecutable</key><string>nibble-gui</string>
              <key>CFBundleIconFile</key><string>nibble</string>
              <key>CFBundlePackageType</key><string>APPL</string>
              <key>CFBundleShortVersionString</key><string>0.1.0</string>
              <key>CFBundleVersion</key><string>0.1.0</string>
              <key>LSMinimumSystemVersion</key><string>13.0</string>
              <key>NSHighResolutionCapable</key><true/>
            </dict>
            </plist>
            EOF
          '';
          meta = {
            description = "A native chat window for nibble";
            # One file is adapted from GPUI's Apache-2.0 example.
            license = with pkgs.lib.licenses; [ mit asl20 ];
            mainProgram = "nibble-gui";
          };
        };
      });

      homeModules.default = import ./nix/module.nix self;

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc clippy rustfmt rust-analyzer ]
            ++ lib.optional (appleSilicon pkgs) (mlxServer pkgs);
        };
      });
    };
}
