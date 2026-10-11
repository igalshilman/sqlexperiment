#!/usr/bin/env bash
# Regenerate the patches from the edited checkouts in .build/ (see setup.sh),
# including files the edits added. Generated files are left out: the inlined
# WebAssembly, which setup.sh rebuilds, and lockfiles.
set -euo pipefail
cd "$(dirname "$0")"
build=../.build

core=$build/sdk-shared-core
git -C "$core" add -N -- src
git -C "$core" diff -- src Cargo.toml > sdk-shared-core.patch

sdk=$build/sdk-typescript
git -C "$sdk" add -N -- packages/libs/restate-sdk/src sdk-shared-core-wasm-bindings/src
git -C "$sdk" diff -- packages/libs/restate-sdk/src sdk-shared-core-wasm-bindings/Cargo.toml \
  sdk-shared-core-wasm-bindings/src ':!packages/libs/restate-sdk/src/endpoint/handlers/vm' \
  > sdk-typescript.patch

grep -h '^diff --git' sdk-shared-core.patch sdk-typescript.patch | awk '{print $3}'
