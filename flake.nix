{
  description = "notify-rs: a notification center and media card for Wayland, with no dependencies";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        notify-rs = pkgs.rustPlatform.buildRustPackage {
          pname = "notify-rs";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = self;
          # the crate has no dependencies, so the lock file only names itself
          cargoLock.lockFile = ./Cargo.lock;
          # the tests need python3 + Pillow, a dbus-daemon and installed fonts; run them with
          # `cargo test` in `nix develop` instead
          doCheck = false;
          postInstall = ''
            install -Dm644 contrib/notify-rs.service $out/lib/systemd/user/notify-rs.service
            substituteInPlace $out/lib/systemd/user/notify-rs.service \
              --replace-fail /usr/local/bin/notify-rs $out/bin/notify-rs
          '';
          meta = {
            description = "Notification center and media card for Wayland, with no dependencies";
            license = pkgs.lib.licenses.mit;
            platforms = pkgs.lib.platforms.linux;
            mainProgram = "notify-rs";
          };
        };
        default = notify-rs;
      });

      overlays.default = final: prev: {
        notify-rs = self.packages.${final.stdenv.hostPlatform.system}.notify-rs;
      };

      # services.notify-rs.enable = true;  (NixOS)
      nixosModules.default = { config, lib, pkgs, ... }:
        let cfg = config.services.notify-rs;
        in
        {
          options.services.notify-rs = {
            enable = lib.mkEnableOption "notify-rs, the notification center and media card";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.notify-rs;
              defaultText = lib.literalExpression "notify-rs.packages.\${system}.notify-rs";
              description = "The notify-rs package to run.";
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];
            systemd.user.services.notify-rs = {
              description = "notify-rs notification center";
              wantedBy = [ "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              after = [ "graphical-session.target" ];
              unitConfig.StartLimitIntervalSec = 0;
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/notify-rs daemon";
                Restart = "on-failure";
                RestartSec = 2;
              };
            };
          };
        };

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            dbus # dbus-daemon, for the integration tests
            curl # cover art over https
            (python3.withPackages (ps: [ ps.pillow ])) # the mock Wayland compositor and image references
            noto-fonts-cjk-sans
            noto-fonts
            nerd-fonts.jetbrains-mono
          ];
          # let the tests and `notify-rs preview` find the fonts
          XDG_DATA_DIRS = "${pkgs.noto-fonts}/share:${pkgs.noto-fonts-cjk-sans}/share:${pkgs.nerd-fonts.jetbrains-mono}/share";
        };
      });
    };
}
