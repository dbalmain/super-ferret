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
      # The CLI and ferretd. Tests are left to the gates, which need the
      # oracles above.
      ferret = pkgs.rustPlatform.buildRustPackage {
        pname = "ferret";
        version = "0.0.0";
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./crates ];
        };
        cargoLock = {
          lockFile = ./Cargo.lock;
          # intpack is a git dependency pinned by rev in Cargo.lock.
          allowBuiltinFetchGit = true;
        };
        cargoBuildFlags = [ "--package" "ferret" ];
        doCheck = false;
      };
      # The stand-in rg for agents' PATH (D118): contrib/agents with the real
      # rg, ferret and the shadow's tools fixed to this flake's builds.
      agent-rg = pkgs.runCommand "ferret-agent-rg" { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
        mkdir -p $out/bin
        install -m755 ${./contrib/agents/rg} $out/bin/rg
        install -m755 ${./contrib/agents/ferret-shadow} $out/bin/ferret-shadow
        install -m755 ${./contrib/agents/ferret-shadow-report} $out/bin/ferret-shadow-report
        patchShebangs $out/bin
        # The launcher runs these before the shadow's PATH exists, and rg
        # itself should see the caller's PATH, so name them directly.
        substituteInPlace $out/bin/rg \
          --replace-fail 'readlink -f' '${pkgs.coreutils}/bin/readlink -f' \
          --replace-fail 'exec setsid' 'exec ${pkgs.util-linux}/bin/setsid'
        wrapProgram $out/bin/rg --set-default FERRET_REAL_RG ${pkgs.ripgrep}/bin/rg \
          --set-default FERRET_BIN ${ferret}/bin/ferret
        wrapProgram $out/bin/ferret-shadow \
          --set-default FERRET_BIN ${ferret}/bin/ferret \
          --prefix PATH : ${pkgs.lib.makeBinPath (with pkgs; [ coreutils diffutils findutils gnugrep gnused util-linux ])}
        wrapProgram $out/bin/ferret-shadow-report \
          --prefix PATH : ${pkgs.lib.makeBinPath (with pkgs; [ coreutils gawk ])}
      '';
    in {
      packages.${system} = { inherit rustfmt fmt findutils ferret agent-rg; default = ferret; };
      devShells.${system}.default = pkgs.mkShell {
        packages = [ findutils pkgs.python3 ];
        FERRET_PYTHON = "${pkgs.python3}/bin/python3";
        FERRET_RG = "${ripgrep}/bin/rg";
        FERRET_GNU_FIND = "${findutils}/bin/find";
      };
      apps.${system}.fmt = { type = "app"; program = "${fmt}/bin/fmt"; };
    };
}
