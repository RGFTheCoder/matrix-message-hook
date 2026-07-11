{
  description = "Build a cargo workspace";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    flake-utils.url = "github:numtide/flake-utils";

    # Pinned Rust toolchain provider. The default nixpkgs rustc bundles an LLVM
    # that miscompiles surrealdb's `diskann-*` AVX-512 code
    # (`llvm.x86.avx512.vpdpwssd.512`); an older stable toolchain's LLVM handles
    # it. rustc uses its OWN bundled LLVM for codegen (the system clang is only
    # the linker), so pinning rustc is what changes the codegen backend.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      advisory-db,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        inherit (pkgs) lib;

        # Pinned stable toolchain that compiles the surrealdb 3.x dependency
        # tree. It must be new enough that (a) edition-2024 + AVX-512 std::arch
        # intrinsics are stable (diskann-wide uses them; stabilized in 1.89) and
        # (b) it satisfies the tree's MSRV (tonic needs 1.88); yet it bundles a
        # real LLVM (20) that can lower `llvm.x86.avx512.vpdpwssd.512` — unlike
        # this environment's default nixpkgs rustc/LLVM, which cannot. rustc uses
        # its OWN bundled LLVM for codegen (system clang is only the linker), so
        # pinning rustc is what fixes the backend.
        #
        # The exact version lives in `rust-toolchain.toml` (crane's recommended
        # pin), so rustup/cargo/rust-analyzer and this nix build all agree. We
        # extend only crane's toolchain scope (the closure form of
        # `overrideToolchain`), not the whole `pkgs`.
        craneLib = (crane.mkLib pkgs).overrideToolchain (
          p: p.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
        );
        src = craneLib.cleanCargoSource ./.;

        # Common arguments can be set here to avoid repeating them later
        commonArgs = {
          inherit src;
          strictDeps = true;

          nativeBuildInputs = [
            pkgs.pkg-config
          ];

          buildInputs = [
            # Add additional build inputs here
          ]
          ++ lib.optionals pkgs.stdenv.isDarwin [
            # Additional darwin specific inputs can be set here
            pkgs.libiconv
          ];

          # Additional environment variables can be set directly
          # MY_CUSTOM_VAR = "some value";
        };

        # Build *just* the cargo dependencies (of the entire workspace),
        # so we can reuse all of that work (e.g. via cachix) when running in CI
        # It is *highly* recommended to use something like cargo-hakari to avoid
        # cache misses when building individual top-level-crates
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        individualCrateArgs = commonArgs // {
          inherit cargoArtifacts;
          inherit (craneLib.crateNameFromCargoToml { inherit src; }) version;
          # NB: we disable tests since we'll run them all via cargo-nextest
          doCheck = false;
        };

        fileSetForCrate =
          crate:
          lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              (craneLib.fileset.commonCargoSources ./crates/hook-core)
              (craneLib.fileset.commonCargoSources ./crates/my-workspace-hack)
              (craneLib.fileset.commonCargoSources crate)
            ];
          };

        # Build the top-level crates of the workspace as individual derivations.
        # This allows consumers to only depend on (and build) only what they need.
        # Though it is possible to build the entire workspace as a single derivation,
        # so this is left up to you on how to organize things
        #
        # Note that the cargo workspace must define `workspace.members` using wildcards,
        # otherwise, omitting a crate (like we do below) will result in errors since
        # cargo won't be able to find the sources for all members.
        hookd = craneLib.buildPackage (
          individualCrateArgs
          // {
            pname = "hookd";
            cargoExtraArgs = "-p hookd";
            src = fileSetForCrate ./crates/hookd;
          }
        );
        hook-admin = craneLib.buildPackage (
          individualCrateArgs
          // {
            pname = "hook-admin";
            cargoExtraArgs = "-p hook-admin";
            src = fileSetForCrate ./crates/hook-admin;
          }
        );
        fake-human = craneLib.buildPackage (
          individualCrateArgs
          // {
            pname = "fake-human";
            cargoExtraArgs = "-p fake-human";
            src = fileSetForCrate ./crates/fake-human;
          }
        );
      in
      {
        checks = {
          # Build the crates as part of `nix flake check` for convenience
          inherit hookd hook-admin fake-human;

          # Run clippy (and deny all warnings) on the workspace source,
          # again, reusing the dependency artifacts from above.
          #
          # Note that this is done as a separate derivation so that
          # we can block the CI if there are issues here, but not
          # prevent downstream consumers from building our crate by itself.
          my-workspace-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          my-workspace-doc = craneLib.cargoDoc (
            commonArgs
            // {
              inherit cargoArtifacts;
              # This can be commented out or tweaked as necessary, e.g. set to
              # `--deny rustdoc::broken-intra-doc-links` to only enforce that lint
              env.RUSTDOCFLAGS = "--deny warnings";
            }
          );

          # Check formatting
          my-workspace-fmt = craneLib.cargoFmt {
            inherit src;
          };

          my-workspace-toml-fmt = craneLib.taploFmt {
            src = pkgs.lib.sources.sourceFilesBySuffices src [ ".toml" ];
            # taplo arguments can be further customized below as needed
            # taploExtraArgs = "--config ./taplo.toml";
          };

          # Audit dependencies
          my-workspace-audit = craneLib.cargoAudit {
            inherit src advisory-db;
          };

          # Audit licenses
          my-workspace-deny = craneLib.cargoDeny {
            inherit src;
          };

          # Run tests with cargo-nextest
          # Consider setting `doCheck = false` on other crate derivations
          # if you do not want the tests to run twice
          my-workspace-nextest = craneLib.cargoNextest (
            commonArgs
            // {
              inherit cargoArtifacts;
              partitions = 1;
              partitionType = "count";
              cargoNextestPartitionsExtraArgs = "--no-tests=pass";
            }
          );

          # Ensure that cargo-hakari is up to date
          my-workspace-hakari = craneLib.mkCargoDerivation {
            inherit src;
            pname = "my-workspace-hakari";
            cargoArtifacts = null;
            doInstallCargoArtifacts = false;

            buildPhaseCargoCommand = ''
              cargo hakari generate --diff  # workspace-hack Cargo.toml is up-to-date
              cargo hakari manage-deps --dry-run  # all workspace crates depend on workspace-hack
              cargo hakari verify
            '';

            nativeBuildInputs = [
              pkgs.cargo-hakari
            ];
          };
        };

        packages = {
          inherit hookd hook-admin fake-human;
        };

        apps = {
          hookd = flake-utils.lib.mkApp {
            drv = hookd;
          };
          hook-admin = flake-utils.lib.mkApp {
            drv = hook-admin;
          };
          fake-human = flake-utils.lib.mkApp {
            drv = fake-human;
          };
        };

        devShells.default = craneLib.devShell {
          # Inherit inputs from checks.
          checks = self.checks.${system};

          # Additional dev-shell environment variables can be set directly
          # MY_CUSTOM_DEVELOPMENT_VAR = "something else";

          # Extra inputs can be added here; cargo and rustc are provided by default.
          packages = [
            pkgs.cargo-hakari
            pkgs.pkg-config
          ];
        };
      }
    );
}
