{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, fenix, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system}.extend fenix.overlays.default;
    in
    {
      # Library crate — consumers take it as a path/git cargo dependency.
      # `nix develop --impure` for the frozen machine-wide toolchain.
      devShells.${system}.default =
        let
          dev-shells-src = builtins.path {
            name = "dev-shells";
            path = /home/dev/dev-shells;
          };
        in
        pkgs.mkShell {
          inputsFrom = [
            (import "${dev-shells-src}/shells/rust-stable.nix" { inherit pkgs; })
          ];
          env = {
            CARGO_TARGET_DIR = "/home/dev/.cache/stand-render-target";
          };
        };
    };
}
