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
    nixpkgs,
    crane,
    fenix,
    ...
  }: let
    systems = ["x86_64-linux"];
    perSystem = f:
      nixpkgs.lib.foldAttrs nixpkgs.lib.mergeAttrs {}
      (map (s: nixpkgs.lib.mapAttrs (_: v: {${s} = v;}) (f s)) systems);
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
        nativeBuildInputs = with pkgs; [pkg-config mold makeWrapper];
        buildInputs = with pkgs; [
          rustPlatform.bindgenHook
          libxkbcommon
          pipewire
          wireplumber
          pulseaudio
          fontconfig
        ];
      };

      cargoArtifacts = craneLib.buildDepsOnly args;
      cargoClippyExtraArgs = "--all-targets -- --deny warnings";

      # D-Bus activation service so launch is auto-started as the notification
      # daemon when a notification is sent and no provider owns the name yet.
      # `@bin@` is replaced with the wrapped binary at install time.
      notificationsService = pkgs.writeText "org.freedesktop.Notifications.service" ''
        [D-BUS Service]
        Name=org.freedesktop.Notifications
        Exec=@bin@ --foreground daemon
      '';

      package = craneLib.buildPackage (args
        // {
          inherit cargoArtifacts;
          postInstall = ''
            wrapProgram "$out/bin/launch" --prefix LD_LIBRARY_PATH : "${libraryPath}"

            install -Dm644 ${notificationsService} \
              "$out/share/dbus-1/services/org.freedesktop.Notifications.service"
            substituteInPlace "$out/share/dbus-1/services/org.freedesktop.Notifications.service" \
              --replace-fail "@bin@" "$out/bin/launch"
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

        fmt = craneLib.cargoFmt (args // {inherit cargoArtifacts;});
        fmt-toml = craneLib.taploFmt {src = pkgs.lib.sources.sourceFilesBySuffices src [".toml"];};
        test = craneLib.cargoTest (args // {inherit cargoArtifacts;});
        clippy = craneLib.cargoClippy (args // {inherit cargoArtifacts cargoClippyExtraArgs;});
      };

      packages.default = package;
    });
}
