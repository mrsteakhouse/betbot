{
  description = "betbot - tipping bot (Tippspiel) running as a Kubernetes CronJob";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, rust-overlay, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];

      perSystem =
        { system, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };

          # Single source of truth, shared with GitHub Actions.
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        in
        {
          devShells.default = pkgs.mkShell {
            packages = [
              rustToolchain # cargo, rustc, clippy, rustfmt, rust-src, rust-analyzer (see rust-toolchain.toml)
              pkgs.cargo-nextest
              pkgs.cargo-deny
              pkgs.kubernetes-helm
              pkgs.chart-testing
              pkgs.kubectl
            ];

            # rust-analyzer finds the std sources via the rust-src component.
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

            # TLS: reqwest with rustls (no openssl/pkg-config needed).
          };

          formatter = pkgs.nixfmt;
        };
    };
}
