{
  # Pins the nightly rustfmt the fmt gate needs: rustfmt.toml sets
  # wrap_comments, which stable rustfmt ignores. The build itself stays on
  # stable. flake.lock holds the pin; `nix flake update fenix` moves it.
  description = "Super Ferret tooling";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, fenix, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      rustfmt = fenix.packages.${system}.default.rustfmt;
      fmt = pkgs.writeShellApplication {
        name = "fmt";
        text = ''
          # cargo fmt runs the first cargo-fmt/rustfmt on PATH.
          PATH="${rustfmt}/bin:$PATH" exec cargo fmt --all "$@"
        '';
      };
    in {
      packages.${system} = { inherit rustfmt fmt; };
      apps.${system}.fmt = { type = "app"; program = "${fmt}/bin/fmt"; };
    };
}
