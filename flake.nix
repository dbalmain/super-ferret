{
  # Pins the nightly rustfmt the fmt gate needs: rustfmt.toml sets
  # wrap_comments, which stable rustfmt ignores. The build itself stays on
  # stable. flake.lock holds the pin; `nix flake update fenix` moves it.
  description = "Super Ferret tooling";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    grep-oracle.url = "github:NixOS/nixpkgs/f45c6f04c2f013f004bf94e284e95d72898d9393";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, fenix, grep-oracle, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      ripgrep = assert grep-oracle.legacyPackages.${system}.ripgrep.version == "15.2.0"; grep-oracle.legacyPackages.${system}.ripgrep;
      findutils = assert pkgs.findutils.version == "4.11.0"; pkgs.findutils;
      rustfmt = fenix.packages.${system}.default.rustfmt;
      fmt = pkgs.writeShellApplication {
        name = "fmt";
        text = ''
          # cargo fmt runs the first cargo-fmt/rustfmt on PATH.
          PATH="${rustfmt}/bin:$PATH" exec cargo fmt --all "$@"
        '';
      };
    in {
      packages.${system} = { inherit rustfmt fmt findutils; };
      devShells.${system}.default = pkgs.mkShell {
        packages = [ findutils ];
        FERRET_RG = "${ripgrep}/bin/rg";
        FERRET_GNU_FIND = "${findutils}/bin/find";
      };
      apps.${system}.fmt = { type = "app"; program = "${fmt}/bin/fmt"; };
    };
}
