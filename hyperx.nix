{ config, lib, pkgs, ... }:

let
  cfg = config.services.hyperx-cloud-3-switchd;

  package = pkgs.callPackage ./package.nix { };

  udevRules = pkgs.writeTextDir
    "lib/udev/rules.d/70-hyperx-cloud-3-switchd.rules"
    ''
      ACTION!="remove", SUBSYSTEM=="hidraw", KERNEL=="hidraw*", ATTRS{idVendor}=="03f0", ATTRS{idProduct}=="06be", TAG+="uaccess"
      ACTION!="remove", SUBSYSTEM=="hidraw", KERNEL=="hidraw*", ATTRS{idVendor}=="03f0", ATTRS{idProduct}=="02cc", TAG+="uaccess"
    '';

  daemonArgs =
    lib.optionals (cfg.speakerSink != null) [
      "--speaker-sink"
      cfg.speakerSink
    ]
    ++ [
      "--poll-ms"
      (toString cfg.pollIntervalMs)
      "--audio-verify-secs"
      (toString cfg.audioVerifyIntervalSec)
    ]
    ++ lib.optional cfg.verbose "--verbose";
in
{
  options.services.hyperx-cloud-3-switchd = {
    enable = lib.mkEnableOption "HyperX Cloud III S audio switch daemon";

    speakerSink = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "alsa_output.pci-0000_0b_00.4.analog-stereo";
      description = ''
        Exact PipeWire/PulseAudio sink used when the headset is off.
        When unset, the daemon remembers the current non-HyperX sink.
      '';
    };

    pollIntervalMs = lib.mkOption {
      type = lib.types.ints.positive;
      default = 250;
      description = "HID polling interval in milliseconds.";
    };

    audioVerifyIntervalSec = lib.mkOption {
      type = lib.types.ints.positive;
      default = 15;
      description = "Audio routing verification interval in seconds.";
    };

    verbose = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Print unchanged routing state too.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [
      package
    ];

    services.udev.packages = [
      udevRules
    ];

    systemd.user.services.hyperx-cloud-3-switchd = {
      description = "HyperX Cloud III S audio switch daemon";

      wantedBy = [
        "graphical-session.target"
      ];

      partOf = [
        "graphical-session.target"
      ];

      after = [
        "graphical-session.target"
        "pipewire-pulse.socket"
      ];

      wants = lib.optional config.services.pipewire.pulse.enable "pipewire-pulse.socket";

      serviceConfig = {
        Type = "simple";
        ExecStart = "${lib.getExe package} ${lib.escapeShellArgs daemonArgs}";

        Restart = "on-failure";
        RestartSec = 2;
        TimeoutStopSec = 5;

        StateDirectory = "hyperx-cloud-3-switchd";
        StateDirectoryMode = "0700";
        # Avoid libpulse creating its runtime directory under a read-only /run.
        Environment = lib.optional config.services.pipewire.pulse.enable
          "PULSE_SERVER=unix:%t/pulse/native";

        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectHome = "read-only";
        ProtectSystem = "strict";
      };
    };
  };
}
