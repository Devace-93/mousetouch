{
  description = "Apple Magic Mouse multitouch gestures for Linux/Wayland";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
    in
    {
      packages = forAllSystems (pkgs: rec {
        mousetouch = pkgs.rustPlatform.buildRustPackage {
          pname = "mousetouch";
          inherit version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          # The engine tests create uinput devices, which the sandbox lacks.
          checkFlags = [ "--skip" "engine::" ];

          nativeBuildInputs = [ pkgs.wrapGAppsHook4 pkgs.gobject-introspection ];
          buildInputs = [ pkgs.gtk4 pkgs.libadwaita ];
          # The hook only wraps programs during fixup, and gappsWrapperArgs
          # (GI_TYPELIB_PATH, GSETTINGS_SCHEMA_DIR, ...) is empty before
          # that, so the settings app is wrapped by hand in postFixup.
          dontWrapGApps = true;

          postInstall =
            let python = pkgs.python3.withPackages (ps: [ ps.pygobject3 ]);
            in ''
              install -Dm755 ui/mousetouch-settings $out/bin/mousetouch-settings
              substituteInPlace $out/bin/mousetouch-settings \
                --replace-fail "#!/usr/bin/python3" "#!${python}/bin/python3"
              install -Dm644 packaging/io.github.Devace_93.MouseTouch.desktop $out/share/applications/io.github.Devace_93.MouseTouch.desktop
              install -Dm644 packaging/io.github.Devace_93.MouseTouch.svg $out/share/icons/hicolor/scalable/apps/io.github.Devace_93.MouseTouch.svg
              install -Dm644 packaging/60-mousetouch.rules $out/lib/udev/rules.d/60-mousetouch.rules
              install -Dm644 packaging/mousetouch.conf $out/lib/modules-load.d/mousetouch.conf
            '';

          postFixup = ''
            wrapProgram $out/bin/mousetouch-settings "''${gappsWrapperArgs[@]}"
          '';

          meta = with pkgs.lib; {
            description = "Apple Magic Mouse multitouch gestures (workspace swipes, pinch, taps) for Linux/Wayland";
            homepage = "https://github.com/Devace-93/mousetouch";
            license = licenses.mit;
            platforms = platforms.linux;
            mainProgram = "mousetouch";
          };
        };
        default = mousetouch;
      });

      # NixOS: add the flake as an input and
      #   imports = [ mousetouch.nixosModules.default ];
      #   services.mousetouch.enable = true;
      nixosModules.default = { config, lib, pkgs, ... }:
        let cfg = config.services.mousetouch;
        in {
          options.services.mousetouch = {
            enable = lib.mkEnableOption "the MouseTouch (Magic Mouse gestures) daemon";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.mousetouch;
              description = "Package providing mousetouch.";
            };
          };
          config = lib.mkIf cfg.enable {
            boot.kernelModules = [ "uinput" ];
            services.udev.packages = [ cfg.package ];
            environment.systemPackages = [ cfg.package ];
            systemd.user.services.mousetouch = {
              description = "MouseTouch: Magic Mouse gestures daemon (userspace driver)";
              wantedBy = [ "default.target" ];
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/mousetouch run";
                Restart = "on-failure";
                RestartSec = 2;
                Nice = -5;
              };
            };
          };
        };
    };
}
