{
  description = "Chrome automation CLI in Rust, deeply inspired by simonw/rodney";

  nixConfig = {
    extra-substituters = ["https://averagechris-dotfiles.cachix.org"];
    extra-trusted-public-keys = ["averagechris-dotfiles.cachix.org-1:VwJkl5dG1+xGDY5x884mH/kVwwpgwBAdBKIF3BZiia4="];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fleet.url = "git+https://git.sr.ht/~averagechris/averagechris.srht.site";
    srht.url = "git+https://git.sr.ht/~averagechris/srht";
  };

  outputs = {
    self,
    nixpkgs,
    fleet,
    srht,
  }: let
    systems = [
      "aarch64-darwin"
      "aarch64-linux"
      "x86_64-darwin"
      "x86_64-linux"
    ];

    forAllSystems = nixpkgs.lib.genAttrs systems;
    pkgsFor = system: import nixpkgs {inherit system;};
    cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    package = cargoToml.package;
    fleetApps = system:
      fleet.lib.fleet.presets.rust {
        pkgs = pkgsFor system;
        inherit self;
        pname = "rdny";
        binaries = ["rdny"];
        subdir = "rdny";
        srhtRepo = "rdny";
        versionMode = "package";
        versionFile = "Cargo.toml";
        lockPackages = ["rdny"];
      };
    mkToolApp = system: name: runtimeInputs: text: let
      pkgs = pkgsFor system;
    in
      pkgs.writeShellApplication {
        inherit name runtimeInputs text;
      };
    ciAudit = system:
      mkToolApp system "ci-audit" [(pkgsFor system).cargo (pkgsFor system).cargo-audit] ''
        cargo audit --deny warnings
      '';
    ciDeny = system:
      mkToolApp system "ci-deny" [(pkgsFor system).cargo (pkgsFor system).cargo-deny] ''
        cargo deny check
      '';
    ciMachete = system:
      mkToolApp system "ci-machete" [(pkgsFor system).cargo (pkgsFor system).cargo-machete] ''
        cargo machete
      '';
    ciSort = system:
      mkToolApp system "ci-sort" [(pkgsFor system).cargo (pkgsFor system).cargo-sort] ''
        cargo sort --workspace --check
      '';
    nixFormatter = system: let
      pkgs = pkgsFor system;
    in
      pkgs.writeShellApplication {
        name = "alejandra";
        runtimeInputs = [pkgs.alejandra];
        text = ''
          if [[ $# -eq 0 ]]; then
            exec alejandra -q .
          fi

          exec alejandra -q "$@"
        '';
      };
  in {
    packages = forAllSystems (system: let
      pkgs = pkgsFor system;
      lib = pkgs.lib;
      app = pkgs.rustPlatform.buildRustPackage {
        pname = "rdny";
        version = package.version;
        src = lib.cleanSource ./.;
        cargoLock.lockFile = ./Cargo.lock;

        meta = {
          description = package.description;
          license = lib.licenses.mit;
          mainProgram = "rdny";
        };
      };
    in {
      default = app;
      rdny = app;
      ci-audit = ciAudit system;
      ci-deny = ciDeny system;
      ci-machete = ciMachete system;
      ci-sort = ciSort system;
      release-artifact = (fleetApps system).releaseArtifact system;
    });

    apps = forAllSystems (system: {
      default = self.apps.${system}.rdny;
      rdny = {
        type = "app";
        program = "${self.packages.${system}.rdny}/bin/rdny";
      };
      ci-audit = {type = "app"; program = "${self.packages.${system}.ci-audit}/bin/ci-audit";};
      ci-deny = {type = "app"; program = "${self.packages.${system}.ci-deny}/bin/ci-deny";};
      ci-machete = {type = "app"; program = "${self.packages.${system}.ci-machete}/bin/ci-machete";};
      ci-sort = {type = "app"; program = "${self.packages.${system}.ci-sort}/bin/ci-sort";};
      inherit ((fleetApps system).apps) prepare-release release-tag release ci-fmt ci-clippy static-checks ci-test;
    });

    checks = forAllSystems (system: {
      inherit (self.packages.${system}) rdny release-artifact;
    });

    devShells = forAllSystems (system: let
      pkgs = pkgsFor system;
    in {
      default = pkgs.mkShell {
        packages = with pkgs; [
          alejandra
          cargo
          cargo-audit
          cargo-deny
          cargo-machete
          cargo-outdated
          cargo-sort
          clippy
          direnv
          jujutsu
          nixd
          rust-analyzer
          rustc
          rustfmt
          sccache
        ] ++ [srht.packages.${system}.srht];
      };
    });

    formatter = forAllSystems nixFormatter;
  };
}
