{
  description = "SQL in Restate virtual objects: the sqlite-kv crate, its Node.js addon, the patched Restate TypeScript SDK, the agent integrations and the examples";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
    in
    {
      # `nix develop`, then `make`, `make test`, `make crash-test`... (see README.md).
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages =
            with pkgs;
            [
              # The crate and the addon; the shared core, to WebAssembly.
              (rust-bin.stable."1.98.1".default.override {
                extensions = [
                  "rust-src"
                  "clippy"
                  "rustfmt"
                ];
                targets = [ "wasm32-unknown-unknown" ];
              })
              # setup.sh needs exactly this version.
              wasm-bindgen-cli_0_2_121
              # The pnpm workspace: node >= 24 runs its TypeScript, and
              # OpenCode needs it.
              nodejs_24
              pnpm_11
              git
              curl
              gnumake

              # The TLA+ check (sqlite-kv/spec/check.sh) and the benchmark scripts.
              jdk_headless
              python3
            ]
            ++ lib.optionals stdenv.hostPlatform.isDarwin [
              libiconv
              # ld64.lld, for a cargo config that links with -fuse-ld=lld.
              lld
            ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
