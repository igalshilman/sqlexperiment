#!/usr/bin/env bash
# Build the patched Restate TypeScript SDK (see ../README.md).
#
# Clones the SDK and its shared core at the pinned revisions into .build/,
# applies patches/, builds the core to WebAssembly and inlines it into the SDK,
# and packs the SDK as .build/restatedev-restate-sdk-1.17.3.tgz, which
# examples installs (`make examples-js`).
#
# Needs: git, cargo with the wasm32-unknown-unknown target, wasm-bindgen 0.2.121
# (cargo install wasm-bindgen-cli --version 0.2.121), node and pnpm.
set -euo pipefail
cd "$(dirname "$0")"
here=$(pwd)
build=$here/.build

SHARED_CORE_REV=382649d0652aa6d971ec16e788d24b4b2c8c8b45 # what sdk-typescript 1.17.3 uses
SDK_TYPESCRIPT_REV=02a12acb879fcf46448d88d028304d3a3113e7e2 # Release 1.17.3

checkout() { # url rev dir
  rm -rf "$3"
  git init -q "$3"
  git -C "$3" fetch -q --depth 1 "$1" "$2"
  git -C "$3" checkout -q FETCH_HEAD
}

mkdir -p "$build"
checkout https://github.com/restatedev/sdk-shared-core.git $SHARED_CORE_REV "$build/sdk-shared-core"
git -C "$build/sdk-shared-core" apply "$here/patches/sdk-shared-core.patch"
checkout https://github.com/restatedev/sdk-typescript.git $SDK_TYPESCRIPT_REV "$build/sdk-typescript"
git -C "$build/sdk-typescript" apply "$here/patches/sdk-typescript.patch"

# The core, as WebAssembly inlined into the SDK.
bindings=$build/sdk-typescript/sdk-shared-core-wasm-bindings
# Its own target directory, whatever CARGO_TARGET_DIR says: the path below reads from it.
(cd "$bindings" && CARGO_TARGET_DIR="$bindings/target" RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-stable} \
  cargo build --release --target wasm32-unknown-unknown)
wasm-bindgen --target web --out-dir "$bindings/pkg" --out-name sdk_shared_core_wasm_bindings \
  "$bindings/target/wasm32-unknown-unknown/release/sdk_shared_core_wasm_bindings.wasm"
node "$here/inline-wasm.mjs" "$bindings/pkg" sdk_shared_core_wasm_bindings \
  "$build/sdk-typescript/packages/libs/restate-sdk/src/endpoint/handlers/vm"

# The SDK package; ctx.sql loads the addon, an optional peer dependency (added
# after installing, which would otherwise fetch it).
(cd "$build/sdk-typescript" && pnpm install --frozen-lockfile && pnpm --filter @restatedev/restate-sdk run build)
(cd "$build/sdk-typescript/packages/libs/restate-sdk" &&
  npm pkg set 'peerDependencies.sqlite-kv="*"' 'peerDependenciesMeta.sqlite-kv.optional=true' --json &&
  pnpm pack --pack-destination "$build")
