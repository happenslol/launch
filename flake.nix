{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    fenix,
    ...
  }: let
    systems = ["x86_64-linux"];
    perSystem = f:
      nixpkgs.lib.foldAttrs nixpkgs.lib.mergeAttrs {}
      (map (s: nixpkgs.lib.mapAttrs (_: v: {${s} = v;}) (f s)) systems);

    nixosModule = {
      config,
      lib,
      pkgs,
      ...
    }: let
      cfg = config.programs.launch;
    in {
      options.programs.launch = {
        enable = lib.mkEnableOption "launch desktop environment";

        package = lib.mkOption {
          type = lib.types.package;
          default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          defaultText = lib.literalExpression "launch.packages.\${system}.default";
          description = "The launch package to use.";
        };

        autostart = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = ''Start the daemon with the graphical session.'';
        };
      };

      config = lib.mkIf cfg.enable {
        environment.systemPackages = [cfg.package];
        security.polkit.enable = true;
        services.upower.enable = lib.mkDefault true;

        security.pam.services.launch = {
          # we go through systemd for the fprint service, so no fprintAuth here
          fprintAuth = lib.mkForce false;
          enableGnomeKeyring = lib.mkDefault config.services.gnome.gnome-keyring.enable;
        };

        systemd.user.services.launch = lib.mkIf cfg.autostart {
          description = "launch desktop environment";
          partOf = ["graphical-session.target"];
          after = ["graphical-session.target"];
          wantedBy = ["graphical-session.target"];

          serviceConfig = {
            Type = "simple";
            # systemd service has to be a foreground process
            ExecStart = "${cfg.package}/bin/launch --foreground daemon";

            # We want other processes to be able to take over the socket
            # manually, so we have to prevent the systemd service from
            # restarting automatically.
            Restart = "no";
          };
        };
      };
    };
  in
    perSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [fenix.overlays.default];
      };

      craneLib =
        (crane.mkLib pkgs).overrideToolchain
        (pkgs.fenix.complete.withComponents [
          "cargo"
          "clippy"
          "rustc"
          "rustfmt"
          "rust-src"
          "rustc-codegen-cranelift-preview"
        ]);

      src = pkgs.lib.fileset.toSource {
        root = ./.;
        fileset = pkgs.lib.fileset.unions [
          ./assets
          (craneLib.fileset.commonCargoSources ./.)
        ];
      };

      args = {
        inherit src;
        strictDeps = true;
        # Build every workspace member, not just the root package. `launch` is
        # both the workspace root and a member, so without this cargo would
        # quietly build only it and leave the daemon uncompiled.
        cargoExtraArgs = "--locked --workspace";
        nativeBuildInputs = with pkgs; [pkg-config mold makeWrapper];
        buildInputs = with pkgs; [
          rustPlatform.bindgenHook
          libxkbcommon
          pipewire
          wireplumber
          pulseaudio
          fontconfig
          pam
        ];
      };

      cargoArtifacts = craneLib.buildDepsOnly args;
      cargoClippyExtraArgs = "--all-targets -- --deny warnings";

      # No D-Bus activation service for org.freedesktop.Notifications on purpose.
      # The daemon is one process for the whole session - launcher, menus, tray,
      # clock, lock screen - and needs the graphical session it draws into. An
      # activation can fire without one, from a notification sent over ssh or by
      # a timer, and it would race whoever holds the socket: each new instance
      # takes it from the last. The user service starts it instead, and a
      # notification sent while nothing is running is simply not shown.
      package = craneLib.buildPackage (args
        // {
          inherit cargoArtifacts;
          meta.mainProgram = "launch";
          postInstall = ''
            # Only `launch` is wrapped. `launch-greetd` re-execs /proc/self/exe
            # to spawn its session workers, and makeWrapper's shell shim would
            # make that resolve to the wrapper rather than the binary. It needs
            # no library path anyway - libpam comes in through RPATH.
            wrapProgram "$out/bin/launch" --prefix LD_LIBRARY_PATH : "${libraryPath}"
          '';
        });

      libraryPath = pkgs.lib.makeLibraryPath (with pkgs; [
        libxkbcommon
        vulkan-loader
        wayland
        pipewire
        pulseaudio
        fontconfig
      ]);
    in {
      devShells.default = craneLib.devShell {
        packages = with pkgs;
          [watchexec rust-analyzer-nightly]
          ++ (with args; (nativeBuildInputs ++ buildInputs));

        LD_LIBRARY_PATH = libraryPath;
      };

      checks = {
        inherit package;

        # `cargo fmt` takes neither --locked nor --workspace, so the shared
        # cargoExtraArgs has to be replaced rather than extended here.
        fmt = craneLib.cargoFmt (args // {inherit cargoArtifacts;} // {cargoExtraArgs = "--all";});
        fmt-toml = craneLib.taploFmt {src = pkgs.lib.sources.sourceFilesBySuffices src [".toml"];};
        test = craneLib.cargoTest (args // {inherit cargoArtifacts;});
        clippy = craneLib.cargoClippy (args // {inherit cargoArtifacts cargoClippyExtraArgs;});
      };

      packages.default = package;
    })
    // {
      overlays.default = final: _prev: {
        launch = self.packages.${final.stdenv.hostPlatform.system}.default;
      };

      nixosModules.default = nixosModule;
      nixosModules.launch = nixosModule;
    };
}
