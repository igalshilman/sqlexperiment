{
  description = "SQL in Restate virtual objects: the state and SQL crates, their Node.js addon, restate-fns, the agent integrations and the examples";

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
              # The crates and the addon.
              (rust-bin.stable."1.98.1".default.override {
                extensions = [
                  "rust-src"
                  "clippy"
                  "rustfmt"
                ];
              })
              # The pnpm workspace: node >= 24 runs its TypeScript, and
              # OpenCode needs it.
              nodejs_24
              pnpm_11
              # scripts/restate-server-from-image.py.
              python3
              git
              curl
              gnumake
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
