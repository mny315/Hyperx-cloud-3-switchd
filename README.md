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

By default the daemon remembers the current non-HyperX output. To force a specific speaker sink:

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
```

## License

MIT
