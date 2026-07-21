{
  description = "Sigil reproducible Linux host proof environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/d407951447dcd00442e97087bf374aad70c04cea";

  outputs = { nixpkgs, ... }:
    let
      pkgs = import nixpkgs { system = "x86_64-linux"; };
    in
    {
      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          clippy
          ffmpeg-headless
          pkg-config
          rustc
          rustfmt
        ];

        shellHook = ''
          export CARGO_BUILD_JOBS="''${CARGO_BUILD_JOBS:-4}"
          export RUST_BACKTRACE="''${RUST_BACKTRACE:-1}"
        '';
      };
    };
}
