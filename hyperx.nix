{ pkgs, ... }:

let
  # HyperX linker
  hyperxDynamicLinker = pkgs.stdenv.cc.bintools.dynamicLinker;

  # HyperX libraries
  hyperxLibraryPath = pkgs.lib.makeLibraryPath [
    pkgs.systemd
    pkgs.stdenv.cc.cc.lib
    pkgs.glibc
  ];

  # HyperX udev
  hyperxUdevRules = pkgs.writeTextFile {
    name = "hyperx-udev-rules";
    destination = "/lib/udev/rules.d/99-HyperHeadset.rules";
    text = ''
      SUBSYSTEM=="usb", ATTRS{idVendor}=="03f0", ATTRS{idProduct}=="06be", TAG+="uaccess"
      KERNEL=="hidraw*", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="03f0", ATTRS{idProduct}=="06be", TAG+="uaccess"
    '';
  };

  # HyperX audio
  hyperxAudioWatch = pkgs.writeShellApplication {
    name = "hyperx-audio-watch";

    runtimeInputs = with pkgs; [
      pulseaudio
      jq
      gnugrep
      coreutils
      systemd
    ];

    text = ''
      set +e
      set +o pipefail
      set -u

      CLI="$HOME/.local/bin/hyper_headset_cli"
      CLI_LOADER="${hyperxDynamicLinker}"
      CLI_LIBRARY_PATH="${hyperxLibraryPath}"
      POLL="''${POLL:-1}"
      # Preferred desktop speakers. Override from the systemd service environment
      # if the PipeWire node name ever changes again after an update.
      SPEAKER_SINK="''${SPEAKER_SINK:-alsa_output.usb-Generic_USB_Audio-00.pro-output-2}"

      # hyper_headset_cli is an external ELF binary in ~/.local/bin. After the
      # update its embedded ELF interpreter can point at a collected Nix store
      # path. LD_LIBRARY_PATH alone cannot fix that, because the kernel needs
      # the interpreter before the process environment exists. Run the binary
      # through the current Nix dynamic linker explicitly.
      export LD_LIBRARY_PATH="$CLI_LIBRARY_PATH:''${LD_LIBRARY_PATH:-}"

      log() {
        echo "$(date '+%F %T') $*" >&2
      }

      pulse_ready() {
        pactl info >/dev/null 2>&1
      }

      wait_pulse() {
        local attempts=30

        while [ "$attempts" -gt 0 ]; do
          if pulse_ready; then
            return 0
          fi

          attempts=$((attempts - 1))
          sleep 1
        done

        log "PulseAudio/PipeWire pactl API is not ready after waiting"
        return 1
      }

      get_sinks() {
        pactl -f json list sinks 2>/dev/null || echo "[]"
      }

      default_sink() {
        pactl -f json info 2>/dev/null | jq -r '.default_sink_name // ""' 2>/dev/null
      }

      hyperx_sink() {
        get_sinks | jq -r '
          def haystack:
            [
              .name? // "",
              .description? // "",
              .properties?."device.description"? // "",
              .properties?."device.product.name"? // "",
              .properties?."alsa.card_name"? // ""
            ] | join(" ");

          .[]? | objects | select(
            haystack | test("HyperX Cloud III S Wireless|usb-HP__Inc_HyperX_Cloud_III_S_Wireless"; "i")
          ) | .name? // ""
        ' 2>/dev/null | head -n 1
      }

      speaker_sink() {
        local sinks
        sinks=$(get_sinks)

        # First use the exact, known-good PipeWire sink for the desktop speakers.
        # Do not fall back to an arbitrary non-HyperX sink: that was the cause of
        # the output jumping between Pro, Pro 1, HDMI, etc. every polling cycle.
        local exact
        exact=$(printf '%s' "$sinks" | jq -r --arg name "$SPEAKER_SINK" '
          .[]? | objects | select((.name? // "") == $name) | .name? // ""
        ' 2>/dev/null | head -n 1)

        if [ -n "$exact" ] && [ "$exact" != "null" ]; then
          echo "$exact"
          return 0
        fi

        # Compatibility with the older profile names that existed before the
        # PipeWire/WirePlumber update. Restrict matching to the same USB Audio
        # device instead of picking the first unrelated sink in the system.
        local legacy
        legacy=$(printf '%s' "$sinks" | jq -r '
          def haystack:
            [
              .name? // "",
              .description? // "",
              .properties?."device.description"? // "",
              .properties?."device.product.name"? // "",
              .properties?."alsa.card_name"? // ""
            ] | join(" ");

          .[]? | objects | select(
            haystack | test(
              "usb-Generic_USB_Audio.*(HiFi|SPDIF|Speaker|Headphones)|USB Audio (S/PDIF Output|Speakers|Front Headphones)";
              "i"
            )
          ) | .name? // ""
        ' 2>/dev/null | head -n 1)

        if [ -n "$legacy" ] && [ "$legacy" != "null" ]; then
          echo "$legacy"
          return 0
        fi

        # If the preferred speakers disappear briefly, keep the current output
        # and retry on the next poll. Never jump to HDMI or another Pro endpoint.
        log "Preferred speaker sink is unavailable: $SPEAKER_SINK"
        return 1
      }

      headset_connected() {
        if [ ! -e "$CLI" ]; then
          log "CLI is missing: $CLI"
          echo ""
          return 0
        fi

        if [ ! -x "$CLI" ]; then
          log "CLI is not executable: $CLI"
          echo ""
          return 0
        fi

        local out
        if ! out=$(timeout --kill-after=1s 5s "$CLI_LOADER" --library-path "$CLI_LIBRARY_PATH" "$CLI" 2>&1); then
          log "CLI failed through loader $CLI_LOADER: $out"
          echo ""
          return 0
        fi

        local state
        state=$(printf '%s\n' "$out" | grep -i '^Connected:' | grep -ioE 'true|false' | tr '[:upper:]' '[:lower:]' | head -n 1)

        if [ -n "$state" ]; then
          echo "$state"
        else
          log "CLI output has no Connected field: $out"
          echo ""
        fi
      }

      target_for_state() {
        local state="$1"
        local target=""

        if [ "$state" = "true" ]; then
          target=$(hyperx_sink)
        elif [ "$state" = "false" ]; then
          target=$(speaker_sink)
        fi

        if [ -n "$target" ] && [ "$target" != "null" ]; then
          echo "$target"
          return 0
        fi

        log "No target sink found for headset state: $state"
        return 1
      }

      move_streams() {
        local target="$1"
        [ -n "$target" ] || return 0

        pactl -f json list sink-inputs 2>/dev/null | jq -r '.[]? | objects | .index? // empty' 2>/dev/null | while IFS= read -r input; do
          [ -n "$input" ] && [ "$input" != "null" ] || continue
          pactl move-sink-input "$input" "$target" 2>/dev/null || true
        done
      }

      switch_to() {
        local target="$1"
        [ -n "$target" ] || return 0

        local current
        current=$(default_sink)

        if [ "$current" != "$target" ]; then
          log "set-default sink $target"
          pactl set-default-sink "$target" 2>/dev/null || true
        fi

        move_streams "$target"
      }

      SLEEP_PID=""
      SUBSCRIBE_PID=""
      MAIN_PID="$$"

      wake_sleep() {
        if [ -n "''${SLEEP_PID:-}" ]; then
          kill "''${SLEEP_PID}" 2>/dev/null || true
        fi
      }

      cleanup() {
        if [ -n "''${SLEEP_PID:-}" ]; then
          kill "''${SLEEP_PID}" 2>/dev/null || true
        fi
        if [ -n "''${SUBSCRIBE_PID:-}" ]; then
          kill "''${SUBSCRIBE_PID}" 2>/dev/null || true
        fi
      }

      shutdown() {
        cleanup
        exit 0
      }

      subscribe_loop() {
        while true; do
          pactl subscribe 2>/dev/null | while IFS= read -r line; do
            case "$line" in
              *"sink"*|*"server"*) kill -USR1 "$MAIN_PID" 2>/dev/null || true ;;
            esac
          done
          sleep 1
        done
      }

      trap wake_sleep USR1
      trap cleanup EXIT
      trap shutdown INT TERM

      wait_pulse || true

      subscribe_loop &
      SUBSCRIBE_PID=$!

      last_state=""
      last_target=""

      while true; do
        state=$(headset_connected)

        if [ "$state" = "true" ] || [ "$state" = "false" ]; then
          target=""
          if target=$(target_for_state "$state"); then
            current=$(default_sink)
            if [ "$state" != "$last_state" ] || [ "$target" != "$last_target" ] || [ "$current" != "$target" ]; then
              switch_to "$target"
              last_state="$state"
              last_target="$target"
            fi
          else
            last_target=""
          fi
        fi

        sleep "$POLL" &
        SLEEP_PID=$!
        wait "''${SLEEP_PID}" 2>/dev/null || true
        SLEEP_PID=""
      done
    '';
  };
in
{
  # HyperX udev
  services.udev.packages = [ hyperxUdevRules ];

  # HyperX audio
  systemd.user.services.hyperx-audio-watch = {
    description = "HyperX headset audio autoswitch";
    wantedBy = [ "default.target" ];

    environment = {
      LD_LIBRARY_PATH = hyperxLibraryPath;
    };

    # Do not hard-bind this to PipeWire/WirePlumber unit names. The script waits
    # for the PulseAudio-compatible PipeWire API itself, which is less brittle
    # across NixOS/PipeWire updates.
    serviceConfig = {
      ExecStart = "${hyperxAudioWatch}/bin/hyperx-audio-watch";
      Restart = "always";
      RestartSec = 2;
      TimeoutStopSec = "3s";
    };
  };
}
