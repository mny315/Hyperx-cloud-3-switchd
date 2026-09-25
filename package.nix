{ lib, rustPlatform, pkg-config, pulseaudio, systemd }:

rustPlatform.buildRustPackage {
  pname = "hyperx-audio-switchd";
  version = "0.1.1";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ];
  };
  cargoLock.lockFile = ./Cargo.lock;
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ pulseaudio systemd ];

  meta = {
    description = "Switch audio output based on HyperX Cloud III S Wireless power state";
    homepage = "https://github.com/mny315/Hyperx-cloud-3-switchd";
    license = lib.licenses.mit;
    mainProgram = "hyperx-audio-switchd";
    platforms = lib.platforms.linux;
  };
}
