{
  description = "Linux client for OPPO and OnePlus device connectivity";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = lib.genAttrs systems;

      dlopenDeps =
        pkgs: with pkgs; [
          libglvnd
          libxcb
          libxkbcommon
          vulkan-loader
          wayland
        ];

      nativeDeps =
        pkgs: with pkgs; [
          fontconfig
          freetype
          gst_all_1.gstreamer
          gst_all_1.gst-plugins-base
          gst_all_1.gst-plugins-bad
          libxcb
          libxkbcommon
          vulkan-loader
          wayland
        ];

      buildDeps =
        pkgs: with pkgs; [
          pkg-config
          rustPlatform.bindgenHook
          wayland-scanner
        ];

      # Elements for PipeWire capture, WebRTC/ICE transport, and H.264/H.265 codecs.
      # `libnice` provides `nicesrc`/`nicesink`; `pipewire` provides `pipewiresrc`.
      gstRuntimePlugins =
        pkgs:
        with pkgs.gst_all_1;
        [
          gstreamer
          gst-plugins-base
          gst-plugins-good
          gst-plugins-bad
          gst-plugins-ugly
        ]
        ++ (with pkgs; [
          libnice
          pipewire
        ]);

      # The repository is not a Cargo workspace, so the crate directory and
      # lock file are named explicitly.
      oplus-connect =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "oplus-connect";
          version = (lib.importTOML ./oplus-connect-gui/Cargo.toml).package.version;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./oplus-connect-gui
              ./oplus-connect-daemon
              ./oplus-connect.desktop
              ./LICENSE
            ];
          };
          cargoRoot = "oplus-connect-gui";
          buildAndTestSubdir = "oplus-connect-gui";
          cargoLock.lockFile = ./oplus-connect-gui/Cargo.lock;

          buildInputs = nativeDeps pkgs;
          nativeBuildInputs = [ pkgs.makeWrapper ] ++ buildDeps pkgs;
          postInstall = ''
            mv $out/bin/oplus-connect-gui $out/bin/oplus-connect

            install -Dm644 oplus-connect.desktop $out/share/applications/oplus-connect.desktop
            install -Dm644 oplus-connect-gui/assets/app-icon.png \
              $out/share/icons/hicolor/512x512/apps/oplus-connect.png
            install -d $out/share/oplus-connect/assets
            install -m644 oplus-connect-gui/assets/*.png $out/share/oplus-connect/assets/
            install -Dm644 LICENSE $out/share/licenses/oplus-connect/LICENSE

            wrapProgram $out/bin/oplus-connect \
              --prefix PATH : ${lib.makeBinPath [ pkgs.iproute2 ]} \
              --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath (dlopenDeps pkgs)} \
              --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : ${lib.makeSearchPath "lib/gstreamer-1.0" (gstRuntimePlugins pkgs)}
          '';

          meta = {
            description = "Linux client for OPPO and OnePlus device connectivity";
            homepage = "https://github.com/YuKongA/oplus-connect";
            license = lib.licenses.mit;
            mainProgram = "oplus-connect";
            platforms = lib.platforms.linux;
          };
        };

      # The GUI runs the daemon in-process, so there is no separate daemon package.
      packagesFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = oplus-connect pkgs;
          oplus-connect = oplus-connect pkgs;
        };
    in
    {
      packages = forAllSystems packagesFor;

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = lib.getExe self.packages.${system}.default;
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages =
              buildDeps pkgs
              ++ nativeDeps pkgs
              ++ gstRuntimePlugins pkgs
              ++ (with pkgs; [
                cargo
                clippy
                iproute2
                rustc
                rustfmt
              ]);

            shellHook = ''
              export LD_LIBRARY_PATH="${lib.makeLibraryPath (dlopenDeps pkgs)}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            '';
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          nixosEval = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              {
                programs.oplus-connect.enable = true;
                system.stateVersion = "26.05";
              }
            ];
          };
          tcpPorts = nixosEval.config.networking.firewall.allowedTCPPorts;
          udpPorts = nixosEval.config.networking.firewall.allowedUDPPorts;
        in
        {
          nixos-module =
            assert lib.elem 7239 tcpPorts;
            assert lib.elem 10150 tcpPorts;
            assert lib.elem 10150 udpPorts;
            assert lib.elem 10151 udpPorts;
            assert nixosEval.config.services.pipewire.enable;
            assert nixosEval.config.xdg.portal.enable;
            assert nixosEval.config.hardware.graphics.enable;
            pkgs.runCommand "oplus-connect-nixos-module-check" { } ''
              touch "$out"
            '';

          gst-plugins =
            pkgs.runCommand "oplus-connect-gst-plugins-check"
              {
                nativeBuildInputs = [ pkgs.gst_all_1.gstreamer.bin ];
              }
              ''
                export GST_PLUGIN_SYSTEM_PATH_1_0="${lib.makeSearchPath "lib/gstreamer-1.0" (gstRuntimePlugins pkgs)}"
                export HOME="$TMPDIR"
                for element in \
                  webrtcbin appsink appsrc queue capsfilter \
                  nicesrc nicesink dtlsenc dtlsdec srtpenc srtpdec sctpenc sctpdec \
                  rtpopusdepay opusdec audioconvert audioresample autoaudiosink \
                  rtph264depay h264parse rtph265depay h265parse \
                  pipewiresrc videorate videoconvert videoscale videobox \
                  x264enc rtph264pay; do
                  gst-inspect-1.0 "$element" >/dev/null
                done
                touch "$out"
              '';
        }
      );

      nixosModules.default = import ./nix/nixos-module.nix { inherit self; };

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);

      overlays.default = final: _prev: {
        oplus-connect = oplus-connect final;
      };
    };
}
