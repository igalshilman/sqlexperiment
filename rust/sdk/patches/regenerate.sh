#!/usr/bin/env bash
# Regenerate the patch from the edited checkout in .build/ (see setup.sh),
# including files the edits added. The lockfile is left out: Cargo rewrites it.
set -euo pipefail
cd "$(dirname "$0")"
build=../.build

sdk=$build/sdk-rust
git -C "$sdk" add -N -- src tests examples
git -C "$sdk" diff HEAD -- src tests examples Cargo.toml rust-toolchain.toml endpoint_manifest_schema.json > sdk-rust.patch

grep '^diff --git' sdk-rust.patch | awk '{print $3}'
