{
  description = "Rust ERTS rewrite — development environment (toolchain, fast linkers, oracle runtimes).";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Rust toolchain is pinned by ./rust-toolchain.toml (single source of truth).
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # Native build inputs needed by the CRANELIFT JIT tier and the C-ABI shims.
        nativeDeps = with pkgs; [
          clang          # linker driver used by .cargo/config.toml
          mold           # fast linker (iteration speed)
          lld            # portable linker fallback
          pkg-config
          cmake
          zlib
          openssl
          linuxHeaders   # for libc bindings / io_uring syscalls
        ];

        # Runtimes used as the differential-test ORACLE and as the benchmark TARGETS.
        # These are the *real* Erlang/Elixir installations the rewrite must match and beat.
        oracleDeps = with pkgs; [
          erlang         # system ERTS baseline (differential oracle, differential harness)
          elixir         # Elixir toolchain (compiler-throughput benchmark target)
          rebar3         # build tool for khepri / emqx targets
        ];

        toolDeps = with pkgs; [
          rustToolchain
          rust-analyzer
          cargo-nextest
          cargo-criterion
          hyperfine      # wall-clock benchmarking of the CLI
          jq
          gnuplot
        ];
      in {
        devShells.default = pkgs.mkShell {
          packages = toolDeps ++ nativeDeps ++ oracleDeps;

          # Keep binaries out of the workspace target dir; expose the oracle explicitly.
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          ERTS_ORACLE = "${pkgs.erlang}/bin/erl";

          shellHook = ''
            echo "Rust ERTS rewrite dev shell"
            echo "  rustc:  $(rustc --version)"
            echo "  oracle: $(erl -noshell -eval 'io:format("~s~n",[erlang:system_info(otp_release)]),halt().' 2>/dev/null || echo 'erl n/a')"
            echo "  linker: mold (configured in .cargo/config.toml)"
          '';
        };
      });
}
