# HyperX Cloud III S Switchd

Small Linux daemon for **HyperX Cloud III S Wireless**.

It automatically switches the default PipeWire/PulseAudio output depending on the headset power state and moves active audio streams to the selected output.

- headset on → HyperX
- headset off → previous non-HyperX output

## NixOS

Add the flake input:

```nix
inputs.hyperx-cloud-3-switchd = {
  url = "github:mny315/Hyperx-cloud-3-switchd";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Add the NixOS module to your host:

```nix
modules = [
  inputs.hyperx-cloud-3-switchd.nixosModules.default
];
```

Enable the service:

```nix
services.hyperx-cloud-3-switchd.enable = true;
```

Then rebuild:

```bash
sudo nixos-rebuild switch --flake .#hostname
```

If the dongle was already connected during the rebuild, reconnect it so the new udev rule is applied.

By default the daemon saves the selected non-HyperX output in
`~/.local/state/hyperx-cloud-3-switchd/speaker.json` (or under `$XDG_STATE_HOME`).
It restores this choice after daemon updates, logins and audio-server restarts.
To change it, select another output while the headset is off. The headset is
recognized automatically by its USB IDs/name; it does not need teaching again.

The saved identity includes the card and output profile, so separate analog and
S/PDIF outputs on one card are not confused when PipeWire changes generated names.
Unavailable analog ports are skipped. A temporarily missing saved output is kept
in the state file and restored when it returns.

To force a specific speaker sink:

```nix
services.hyperx-cloud-3-switchd = {
  enable = true;
  speakerSink = "alsa_output.pci-0000_0b_00.4.analog-stereo";
};
```

Find sink names with:

```bash
pactl list short sinks
```

## Logs

```bash
systemctl --user status hyperx-cloud-3-switchd
journalctl --user -u hyperx-cloud-3-switchd -f
```

## Development

```bash
./run.sh
./run.sh check
./run.sh integration
```

The integration check uses a private PulseAudio instance with silent virtual
outputs. It tests updates, missing/renamed outputs, stream moves, server hangs
and server restarts without changing the desktop audio session.

Build the installable Nix package with `nix build`.

Audio changes and new streams trigger reconciliation on the next poll; the
15-second verification timer is an additional safety check. Failed stream moves
are retried after one second, and unresponsive audio connections are replaced.

`--state-file PATH` overrides the state location for standalone runs or testing.
The NixOS service creates a writable persistent state directory even with its
filesystem restrictions enabled. After upgrading an existing installation,
select the speakers once while the headset is off to seed the new state file.

## License

MIT
