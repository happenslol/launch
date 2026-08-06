# The login manager. A separate module from `programs.launch` because it is a
# separate program with a separate lifecycle: a system service that owns a VT and
# runs before anybody has logged in, rather than something in a user's session.
{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.launch.greeter;
  format = pkgs.formats.toml {};

  settings =
    lib.recursiveUpdate {
      terminal = {
        inherit (cfg) vt;
        switch = true;
      };

      greeter =
        {
          inherit (cfg) user;
          command = cfg.greeterCommand;
          service = cfg.pam.greeterService;
        }
        # TOML has no null, so an unset output has to be an absent key rather than
        # a present empty one - which the daemon would read as an output named "".
        // lib.optionalAttrs (cfg.primaryOutput != null) {
          primary_output = cfg.primaryOutput;
        };

      session = {
        inherit (cfg) command;
        service = cfg.pam.passwordService;
        fingerprint_service = cfg.pam.fingerprintService;
        inherit (cfg) fingerprint;
      };

      users = {
        minimum_uid = cfg.users.minimumUid;
        maximum_uid = cfg.users.maximumUid;
        inherit (cfg.users) include exclude;
      };
    }
    cfg.settings;

  configFile = format.generate "launch-greetd.toml" settings;

  # No binds at all. There must be no way out of the login screen into a shell,
  # and the compositor hosting it needs nothing but the ability to draw.
  niriConfig = pkgs.writeText "launch-greeter.kdl" ''
    input {
        keyboard {
            xkb {
                layout "${config.services.xserver.xkb.layout}"
                variant "${config.services.xserver.xkb.variant}"
                options "${config.services.xserver.xkb.options}"
                model "${config.services.xserver.xkb.model}"
            }
        }
    }

    cursor {
        hide-when-typing
    }

    // Black, because this is what shows in the gap between the login screen's
    // surfaces going away and niri itself exiting - and the screen has just
    // faded to black to hand over. Left at niri's default it would flash its
    // backdrop colour on the way out.
    layout {
        background-color "#000000"
    }

    hotkey-overlay {
        skip-at-startup
    }

    // The login screen owns niri's lifetime: when it exits without having
    // authenticated anybody, niri goes too and the daemon starts a fresh one.
    spawn-at-startup "${pkgs.writeShellScript "launch-greeter-session" ''
      ${cfg.package}/bin/launch greet
      ${cfg.niri.package}/bin/niri msg action quit --skip-confirmation
    ''}"
  '';
in {
  options.programs.launch.greeter = {
    enable = lib.mkEnableOption "the launch login manager";

    package = lib.mkOption {
      type = lib.types.package;
      default = config.programs.launch.package;
      defaultText = lib.literalExpression "config.programs.launch.package";
      description = "Package providing `launch-greetd` and `launch`.";
    };

    vt = lib.mkOption {
      type = lib.types.either lib.types.ints.positive (lib.types.enum ["next" "current" "none"]);
      default = 1;
      description = ''
        Virtual terminal the login screen owns. Keep this off the VT any other
        display manager uses; two of them on one VT will fight over it.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "launch-greeter";
      description = ''
        Unprivileged account the login screen runs as. It also owns the IPC
        socket, and that ownership is the whole of the access control on it.
      '';
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "launch-greeter";
      description = "Primary group of {option}`user`.";
    };

    command = lib.mkOption {
      type = lib.types.str;
      example = "niri --session";
      description = ''
        Command run as whoever logs in.

        Run as `sh -c "exec <command>"`, so the session ends up as the leader of
        its own logind session rather than a child of a shell that contributes
        nothing but confuses signal delivery and scope accounting. The `exec` is
        also why this must be a *single* command: in `exec a; b`, the `b` is
        unreachable. Point it at a script if you need more than one thing to
        happen.
      '';
    };

    greeterCommand = lib.mkOption {
      type = lib.types.str;
      defaultText = lib.literalExpression "niri -c <generated config>";
      description = ''
        Shell command line that starts the compositor hosting the login screen.
        Defaults to niri with a generated config that spawns `launch greet` and
        has no key bindings.
      '';
    };

    primaryOutput = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "eDP-1";
      description = ''
        Output the prompt is drawn on. Every other output gets a plain backdrop.
        Null lets the login screen take the first output it is offered.
      '';
    };

    fingerprint = lib.mkOption {
      type = lib.types.bool;
      default = config.services.fprintd.enable;
      defaultText = lib.literalExpression "config.services.fprintd.enable";
      description = ''
        Run a second PAM stack for the fingerprint reader alongside the password
        one, so a finger and a password can be offered at the same time.
      '';
    };

    niri = {
      package = lib.mkOption {
        type = lib.types.package;
        default = pkgs.niri;
        defaultText = lib.literalExpression "pkgs.niri";
        description = "Compositor used to host the login screen.";
      };
    };

    pam = {
      passwordService = lib.mkOption {
        type = lib.types.str;
        default = "launch-greeter";
        description = "PAM service for password authentication.";
      };

      fingerprintService = lib.mkOption {
        type = lib.types.str;
        default = "launch-greeter-fingerprint";
        description = ''
          PAM service whose auth stack is `pam_fprintd` and nothing else.
        '';
      };

      greeterService = lib.mkOption {
        type = lib.types.str;
        default = "launch-greeter-session";
        description = ''
          PAM service for the login screen's own session. Its auth stack is never
          run, so it needs only an account and session half.
        '';
      };
    };

    users = {
      minimumUid = lib.mkOption {
        type = lib.types.int;
        default = 1000;
        description = "Lowest uid treated as a human account.";
      };

      maximumUid = lib.mkOption {
        type = lib.types.int;
        default = 60000;
        description = "Highest uid treated as a human account.";
      };

      include = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        description = "Accounts to offer regardless of their uid.";
      };

      exclude = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        description = "Accounts to hide even when their uid is in range.";
      };
    };

    settings = lib.mkOption {
      type = format.type;
      default = {};
      description = ''
        Merged last into the generated configuration, so anything the options
        above do not cover is still reachable.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.command != "";
        message = "programs.launch.greeter.command must be set to the session to start.";
      }
    ];

    # greetd has no `vt` option any more - nixpkgs fixed it to VT 1 - so the
    # overlap is decidable without reading anything from it.
    warnings =
      lib.optional (config.services.greetd.enable && cfg.vt == 1)
      ''
        greetd is enabled and always uses VT 1, which programs.launch.greeter is
        also set to. They will fight over it: give the launch greeter a different
        vt, or disable greetd.
      '';

    programs.launch.greeter.greeterCommand =
      lib.mkDefault "${cfg.niri.package}/bin/niri -c ${niriConfig}";

    environment.etc."launch/greetd.toml".source = configFile;

    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      home = "/var/lib/${cfg.user}";
      createHome = true;
      # Deliberately no video/input/seat groups. niri reaches DRM and evdev
      # through libseat -> logind, which hands those to whichever session is
      # active on the seat - and the daemon creates exactly such a session by
      # opening a PAM session on the target VT. If `LIBSEAT_BACKEND=builtin` ever
      # becomes necessary, `video` and `input` are the escape hatch, but shipping
      # them by default would hand the login screen more than it needs.
      extraGroups = [];
    };

    users.groups.${cfg.group} = {};

    # The login screen's own session. Its auth half is never run - the daemon
    # opens this one without authenticating - so it needs only enough to become a
    # logind session on the right VT.
    security.pam.services.${cfg.pam.greeterService} = {
      startSession = true;
      setLoginUid = true;
    };

    security.pam.services.${cfg.pam.passwordService} = {
      # The fingerprint worker owns the reader. Leaving pam_fprintd in this stack
      # too would put both workers in a queue for the same device, which is the
      # sequential behaviour this whole design exists to avoid.
      fprintAuth = lib.mkForce false;
      startSession = true;
      setLoginUid = true;
      updateWtmp = true;
      enableGnomeKeyring = lib.mkDefault config.services.gnome.gnome-keyring.enable;
    };

    security.pam.services.${cfg.pam.fingerprintService} = lib.mkIf cfg.fingerprint {
      # `unixAuth = false` plus `fprintAuth = true` is what produces a
      # fingerprint-only auth stack out of the nixpkgs option model: the fprintd
      # rule is `sufficient` and gated only on fprintAuth, both pam_unix auth
      # rules are gated on unixAuth, and the whole block of modules needing
      # PAM_AUTHTOK vanishes with them.
      unixAuth = false;
      fprintAuth = lib.mkForce true;

      # The daemon needs `pam_fprintd`'s own return code, and the default stack
      # hides it. `sufficient` means a failure falls through to the trailing
      # `pam_deny`, whose PAM_AUTH_ERR is what `pam_authenticate` then reports -
      # so a machine with no reader at all is indistinguishable from a finger
      # that did not match, and the login screen says "not recognised" to
      # somebody who never touched a sensor.
      #
      # `required` with no deny after it lets the real code through, which is how
      # PAM_AUTHINFO_UNAVAIL reaches the daemon and turns the indicator off
      # quietly. This is the shape GDM's own gdm-fingerprint stack uses.
      rules.auth = {
        fprintd.control = lib.mkForce "required";
        deny.enable = false;
      };
      # There is no password in PAM_AUTHTOK to unlock a keyring with, so asking
      # would only fail. A fingerprint login leaves the keyring locked.
      enableGnomeKeyring = false;
      # Required on both services: the session is opened on whichever stack
      # authenticated, so a fingerprint login would otherwise produce a session
      # with no logind registration at all.
      startSession = true;
      setLoginUid = true;
      updateWtmp = true;
    };

    systemd.services.launch-greetd = {
      description = "launch login manager";
      wantedBy = ["graphical.target"];
      after = [
        # Removes /run/nologin. Starting earlier means PAM refuses every login for
        # the first seconds of uptime.
        "systemd-user-sessions.service"
        "plymouth-quit-wait.service"
        "getty@tty${toString cfg.vt}.service"
      ];
      wants = ["systemd-user-sessions.service"];
      # Stops the getty, and - paired with the `after` above - makes the stop
      # happen before our start rather than concurrently. Otherwise both own the
      # tty and fight over VT ownership and KDSETMODE.
      conflicts = ["getty@tty${toString cfg.vt}.service"];

      # `nixos-rebuild switch` must not kill the running session.
      restartIfChanged = false;

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/launch-greetd --config /etc/launch/greetd.toml";
        # Holds ExecStart back until the rest of the boot transaction is done, so
        # the first frame does not race console output.
        Type = "idle";
        # The daemon outlives every session - it respawns the login screen when a
        # session ends - so it only ever exits on failure or shutdown. greetd
        # upstream uses on-success, which would be right only if it exited after
        # handing off.
        Restart = "always";
        StateDirectory = "launch-greetd";
        # systemd's default sets SIGPIPE to SIG_IGN, and that disposition is
        # inherited by every child - which gives the classic `yes | head` hang
        # inside the user's session.
        IgnoreSIGPIPE = false;
        # The default gives a private session keyring, so keys installed by PAM
        # modules during pam_open_session would land somewhere the user's own
        # session cannot see.
        KeyringMode = "shared";
        # So terminal-attached job-control processes actually exit at logout.
        SendSIGHUP = true;
        # Stopping means ending whatever session is running and waiting for its
        # worker to finish PAM teardown, which the default 90s would let drag out
        # across a shutdown.
        TimeoutStopSec = "30s";
      };
    };

    # A login manager *is* the graphical target: without this the machine boots to
    # multi-user, nothing pulls the unit in, and there is no way to log in.
    #
    # A default rather than a plain assignment so it cannot collide with another
    # display manager's identical setting while both are installed - which is the
    # normal state during a cutover.
    systemd.defaultUnit = lib.mkDefault "graphical.target";

    # A desktop daemon for the greeter's own account would be a second launch
    # instance fighting the login screen for the same abstract socket.
    systemd.user.services.launch.unitConfig.ConditionUser = "!${cfg.user}";
  };
}
