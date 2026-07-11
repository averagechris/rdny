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
    ciSmoke = system:
      mkToolApp system "ci-smoke" [(pkgsFor system).bash (pkgsFor system).coreutils (pkgsFor system).gnugrep] ''
        exec bash scripts/ci-smoke.sh "$@"
      '';
    ciSmokeGate = system:
      mkToolApp system "ci-smoke-gate" [(pkgsFor system).bash (pkgsFor system).coreutils] ''
        exec bash scripts/ci-smoke-gate.sh "$@"
      '';
    ciSmokeFlowTest = system:
      mkToolApp system "ci-smoke-flow-test" [(pkgsFor system).bash (pkgsFor system).coreutils (pkgsFor system).gnugrep] ''
        exec bash scripts/ci-smoke-flow-test.sh "$@"
      '';
    ciReleaseFacing = system:
      mkToolApp system "ci-release-facing" [(pkgsFor system).bash (pkgsFor system).coreutils (pkgsFor system).findutils (pkgsFor system).gawk (pkgsFor system).gnugrep (pkgsFor system).gnutar (pkgsFor system).nix] ''
        exec bash scripts/ci-release-facing.sh "$@"
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
        # Tests run as a dedicated mandatory CI task. Avoid rerunning the
        # process-heavy suite inside every package and wrapper build.
        doCheck = false;
        postInstall = ''
          install -Dm644 LICENSE "$out/share/licenses/rdny/LICENSE"
        '';

        meta = {
          description = package.description;
          license = lib.licenses.mit;
          mainProgram = "rdny";
        };
      };
      bundled = pkgs.symlinkJoin {
        name = "rdny-bundled-${package.version}";
        paths = [app];
        nativeBuildInputs = [pkgs.makeWrapper];
        postBuild = ''
          wrapProgram "$out/bin/rdny" \
            --set-default RDNY_CHROME "${pkgs.ungoogled-chromium}/bin/chromium" \
            --set-default RDNY_FFMPEG "${pkgs.ffmpeg-headless}/bin/ffmpeg"
        '';

        meta =
          app.meta
          // {
            description = "${package.description} (with ungoogled-chromium and ffmpeg)";
          };
      };
      ffmpeg = pkgs.symlinkJoin {
        name = "rdny-ffmpeg-${package.version}";
        paths = [app];
        nativeBuildInputs = [pkgs.makeWrapper];
        postBuild = ''
          wrapProgram "$out/bin/rdny" \
            --set-default RDNY_FFMPEG "${pkgs.ffmpeg-headless}/bin/ffmpeg"
        '';

        meta =
          app.meta
          // {
            description = "${package.description} (with ffmpeg)";
          };
      };
    in
      {
        default = app;
        rdny = app;
        rdny-ffmpeg = ffmpeg;
        ci-audit = ciAudit system;
        ci-deny = ciDeny system;
        ci-machete = ciMachete system;
        ci-release-facing = ciReleaseFacing system;
        ci-smoke = ciSmoke system;
        ci-smoke-flow-test = ciSmokeFlowTest system;
        ci-smoke-gate = ciSmokeGate system;
        ci-sort = ciSort system;
        release-artifact = (fleetApps system).releaseArtifact system;
      }
      // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        rdny-bundled = bundled;
      });

    apps = forAllSystems (system: let
      pkgs = pkgsFor system;
      lib = pkgs.lib;
    in
      {
        default = self.apps.${system}.rdny;
        rdny = {
          type = "app";
          program = "${self.packages.${system}.rdny}/bin/rdny";
        };
        rdny-ffmpeg = {
          type = "app";
          program = "${self.packages.${system}.rdny-ffmpeg}/bin/rdny";
        };
        ci-audit = {
          type = "app";
          program = "${self.packages.${system}.ci-audit}/bin/ci-audit";
        };
        ci-deny = {
          type = "app";
          program = "${self.packages.${system}.ci-deny}/bin/ci-deny";
        };
        ci-machete = {
          type = "app";
          program = "${self.packages.${system}.ci-machete}/bin/ci-machete";
        };
        ci-release-facing = {
          type = "app";
          program = "${self.packages.${system}.ci-release-facing}/bin/ci-release-facing";
        };
        ci-smoke = {
          type = "app";
          program = "${self.packages.${system}.ci-smoke}/bin/ci-smoke";
        };
        ci-smoke-flow-test = {
          type = "app";
          program = "${self.packages.${system}.ci-smoke-flow-test}/bin/ci-smoke-flow-test";
        };
        ci-smoke-gate = {
          type = "app";
          program = "${self.packages.${system}.ci-smoke-gate}/bin/ci-smoke-gate";
        };
        ci-sort = {
          type = "app";
          program = "${self.packages.${system}.ci-sort}/bin/ci-sort";
        };
        inherit ((fleetApps system).apps) prepare-release release-tag release ci-fmt ci-clippy static-checks ci-test;
      }
      // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        rdny-bundled = {
          type = "app";
          program = "${self.packages.${system}.rdny-bundled}/bin/rdny";
        };
      });

    checks = forAllSystems (system: {
      inherit (self.packages.${system}) rdny release-artifact;
    });

    devShells = forAllSystems (system: let
      pkgs = pkgsFor system;
    in {
      default = pkgs.mkShell {
        packages = with pkgs;
          [
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
          ]
          ++ [srht.packages.${system}.srht];
      };
    });

    formatter = forAllSystems nixFormatter;
  };
}
