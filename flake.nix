{
  description = "HyperX Cloud III S audio switching daemon";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      nixosModules.default = import ./hyperx.nix;

      devShells = forAllSystems (system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              cargo
              clippy
              pkg-config
              rustc
              rustfmt
            ];

            buildInputs = with pkgs; [
              pulseaudio
              systemd
            ];

            RUST_BACKTRACE = "1";
          };
        });

      formatter = forAllSystems (system: (pkgsFor system).nixfmt-rfc-style);
    };
}
