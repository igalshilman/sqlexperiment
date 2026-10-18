#!/usr/bin/env bash
# Regenerate the patches from the edited checkouts in .build/ (see setup.sh),
# including files the edits added. The lockfile is left out: Cargo rewrites it
# for the patched core.
set -euo pipefail
cd "$(dirname "$0")"
build=../.build

core=$build/sdk-shared-core
git -C "$core" add -N -- src
git -C "$core" diff HEAD -- src Cargo.toml > ../../../core/sdk-shared-core/sdk-shared-core.patch

sdk=$build/sdk-rust
git -C "$sdk" add -N -- src tests examples
git -C "$sdk" diff HEAD -- src tests examples Cargo.toml rust-toolchain.toml > sdk-rust.patch

grep -h '^diff --git' ../../../core/sdk-shared-core/sdk-shared-core.patch sdk-rust.patch | awk '{print $3}'
