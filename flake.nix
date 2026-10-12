{
  description = "SQL in Restate virtual objects: the sqlite-kv crate, its Node.js addon, the patched Restate TypeScript SDK and the examples";

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
              # The crate and the addon; the shared core, to WebAssembly. The
              # version restate pins too, for building restate-server.
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
              # The SDK (node >= 22, pnpm 11) and the examples.
              nodejs_22
              pnpm_11
              git
              curl
              gnumake

              # restate-server from source (the Makefile's default on macOS):
              # protobuf, RocksDB, librdkafka, OpenSSL.
              protobuf
              cmake
              pkg-config
              openssl
              perl
              zlib

              # The TLA+ check (sqlite-kv/spec/check.sh) and the benchmark scripts.
              jdk_headless
              python3
            ]
            ++ lib.optionals stdenv.hostPlatform.isDarwin [ libiconv ];

          # restate-server's C dependencies probe the compiler at -O0, which
          # _FORTIFY_SOURCE turns into errors.
          hardeningDisable = [ "fortify" ];
          # bindgen, for RocksDB's bindings.
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
