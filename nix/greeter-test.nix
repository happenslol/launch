# Boots the login manager in a VM and drives a real login through it.
#
# This is the only place the daemon half runs: forking a worker, running a PAM
# stack, opening a logind session and handing over a VT all need root and a
# console, which no unit test can provide. The login screen itself is left out -
# it needs a compositor to draw into, and `greet-client` speaks the same protocol
# from a shell instead.
{
  self,
  pkgs,
  ...
}: let
  system = pkgs.stdenv.hostPlatform.system;
in
  pkgs.testers.runNixOSTest {
    name = "launch-greeter";

    nodes.machine = {
      config,
      pkgs,
      ...
    }: {
      imports = [self.nixosModules.launch self.nixosModules.greeter];

      programs.launch.greeter = {
        enable = true;
        # VT 2, so a failure leaves the getty on VT 1 to look at.
        vt = 2;
        # A script rather than a compound command, because the command is `exec`ed:
        # in `exec a; b` the `b` never runs, and the session would end the moment
        # the marker was written.
        command = "${pkgs.writeShellScript "launch-test-session" ''
          id -un > /tmp/session-user
          exec sleep infinity
        ''}";
        # No compositor: the daemon's own behaviour is what is under test, and a
        # shell keeps the greeter session alive the same way niri would.
        greeterCommand = "sleep infinity";
        # No reader in a VM, so the stack is there to prove the daemon copes with
        # a fingerprint path that cannot work rather than to authenticate anyone.
        fingerprint = true;
      };

      # The module defaults this to config.programs.launch.package, which needs
      # the desktop half too.
      programs.launch.enable = true;

      users.users.ada = {
        isNormalUser = true;
        uid = 1001;
        description = "Ada Lovelace";
        password = "correct-horse";
      };

      users.users.grace = {
        isNormalUser = true;
        uid = 1002;
        description = "Grace Hopper";
        password = "battery-staple";
      };

      environment.systemPackages = [
        config.programs.launch.package
        self.packages.${system}.greet-client
      ];

      # So a failure says which PAM code came back rather than only that one did.
      systemd.services.launch-greetd.environment.LAUNCH_GREETD_LOG = "launch_greetd=debug,warn";

      virtualisation.memorySize = 3072;
    };

    testScript = ''
      import json

      GREETER = "launch-greeter"

      def events(output):
          """Every event frame the client printed, decoded."""
          return [
              json.loads(line.removeprefix("event "))
              for line in output.splitlines()
              if line.startswith("event ")
          ]

      def kinds(output):
          """The tag of each event. The protocol is internally tagged, so the
          variant name is a "type" field rather than a wrapping object."""
          return [e["type"] for e in events(output)]

      def only(output, tag):
          """Every event of one kind."""
          return [e for e in events(output) if e["type"] == tag]

      def properties(session):
          return dict(
              line.split("=", 1)
              for line in machine.succeed(f"loginctl show-session {session}").splitlines()
              if "=" in line
          )

      def find_session(user, klass):
          """The login session for a user, by class.

          Not simply the first session that mentions the name: logind also keeps a
          synthetic Class=manager session per user for their systemd instance, and
          that one is on no VT at all.
          """
          ids = machine.succeed(
              "loginctl list-sessions --no-legend | awk '{print $1}'"
          ).split()

          for session in ids:
              found = properties(session)
              if found.get("Name") == user and found.get("Class") == klass:
                  return found

          raise AssertionError(f"no {klass} session for {user} among {ids}")

      def drive(*steps):
          """Runs greet-client as the greeter user, which is who owns the socket."""
          joined = " ".join(steps)
          return machine.succeed(
              f"su -s /bin/sh {GREETER} -c 'greet-client {joined}' 2>&1"
          )

      machine.wait_for_unit("launch-greetd.service")

      with subtest("the login screen gets a session of its own"):
          machine.wait_until_succeeds("test -S /run/launch-greetd-*.sock", timeout=30)
          machine.wait_until_succeeds(
              f"loginctl list-sessions --no-legend | grep -w {GREETER}", timeout=30
          )
          # class=greeter is what pam_systemd was told through XDG_SESSION_CLASS,
          # and it is what stops logind treating the login screen as a logged-in
          # user.
          greeter_session = find_session(GREETER, "greeter")
          assert greeter_session["VTNr"] == "2", greeter_session

      with subtest("the socket is the greeter's alone"):
          mode = machine.succeed("stat -c '%a %U' /run/launch-greetd-*.sock").strip()
          assert mode == f"600 {GREETER}", f"unexpected socket ownership: {mode}"

      with subtest("accounts in the uid range are offered"):
          out = drive("hello")
          welcome = only(out, "welcome")[0]
          names = sorted(u["name"] for u in welcome["users"])
          assert names == ["ada", "grace"], f"unexpected accounts: {names}"
          assert welcome["default_user"] in names, welcome["default_user"]
          # Only the greeter and root are outside the range, so neither may appear.
          assert GREETER not in names and "root" not in names, names

      with subtest("a wrong password is rejected and can be retried"):
          out = drive("hello", "auth:ada", "password:wrong", "auth:ada")
          failed = [e for e in only(out, "failed") if e["source"] == "password"]
          assert failed, kinds(out)
          assert failed[0]["failure"]["type"] == "rejected", failed[0]
          # The retry has to produce a fresh prompt, or nothing could be typed
          # into the login screen a second time.
          assert len(only(out, "prompt")) >= 2, kinds(out)

      with subtest("a fingerprint path that cannot work goes quiet"):
          # No reader and no fprintd, so pam_fprintd answers PAM_AUTHINFO_UNAVAIL.
          # That has to arrive as the indicator turning off, never as a failure the
          # user has to read.
          # `settle` rather than stopping at the prompt: the second worker fails on
          # its own schedule, well after the password path has asked its question.
          out = drive("hello", "auth:ada", "settle")
          states = [e["state"] for e in only(out, "fingerprint")]
          assert "off" in states, f"expected the reader to go quiet: {states}"
          failures = [e for e in only(out, "failed") if e["source"] == "fingerprint"]
          assert not failures, f"an unusable reader reported a failure: {failures}"

      with subtest("the right password starts the session"):
          out = drive("hello", "auth:ada", "password:correct-horse", "start")
          assert "authenticated" in kinds(out), kinds(out)
          assert "session_started" in kinds(out), kinds(out)

          # The greeter is asked to leave and the parked worker takes the VT, so
          # the session only actually runs once the login screen has gone.
          machine.wait_for_file("/tmp/session-user", timeout=30)
          assert machine.succeed("cat /tmp/session-user").strip() == "ada"

      with subtest("logind sees the session as ada on the right VT"):
          machine.wait_until_succeeds(
              "loginctl list-sessions --no-legend | grep -w ada", timeout=30
          )
          session = find_session("ada", "user")
          assert session["VTNr"] == "2", session
          # greetd never sets XDG_SESSION_TYPE, which leaves logind calling a
          # Wayland session a tty one and portals picking the wrong backend.
          assert session["Type"] == "wayland", session
          # The last account to log in is remembered for next time.
          assert machine.succeed("cat /var/lib/launch-greetd/last-user").strip() == "ada"

      with subtest("a login screen that will not leave is evicted"):
          # The stand-in greeter is a `sleep`, which never quits on its own - the
          # real one exits on SessionStarted. So this run exercises the eviction
          # path for real: SIGTERM after the patience elapses, and the session
          # starting once it is gone. That path has no other test, and a wedged
          # login screen is the one failure that leaves a machine unusable.
          machine.succeed(
              "journalctl -u launch-greetd | grep -q 'The login screen has not exited'"
          )
          # SIGTERM was enough, so it never had to be killed.
          machine.fail(
              "journalctl -u launch-greetd | grep -q 'ignored SIGTERM'"
          )

      with subtest("the worker survives to close the session"):
          # Killing the worker instead of the session process is what leaks a
          # logind session, so the worker must still be there holding it open.
          machine.succeed("pgrep -f 'launch-greetd --session-worker' >/dev/null")
    '';
  }
