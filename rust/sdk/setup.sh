#!/usr/bin/env bash
# Prepare the patched Restate Rust SDK (see README.md).
#
# Clones the SDK and its shared core at the pinned revisions into .build/, and
# applies patches/ and the shared core's patch (core/sdk-shared-core). The
# SDK's Cargo.toml takes the core from the clone next to it; so must every
# crate that depends on the SDK (a [patch.crates-io] of its own).
#
# Needs: git.
set -euo pipefail
cd "$(dirname "$0")"
here=$(pwd)
build=$here/.build

SHARED_CORE_REV=382649d0652aa6d971ec16e788d24b4b2c8c8b45 # as the TypeScript and Python SDKs'
SDK_RUST_REV=5cd9a3d24d7ddc21f29c8c43074d3d67ca67c163    # Release 0.12.1

checkout() { # url rev dir
  rm -rf "$3"
  git init -q "$3"
  git -C "$3" fetch -q --depth 1 "$1" "$2"
  git -C "$3" checkout -q FETCH_HEAD
}

# Whether the clone in $1 has edits that the patch $2 does not (over the paths
# regenerate.sh writes it from): re-cloning would lose them.
unsaved() { # dir patch paths...
  local dir=$1 patch=$2
  shift 2
  [ -d "$dir/.git" ] || return 1
  git -C "$dir" add -N -- "$@" 2> /dev/null
  ! git -C "$dir" diff HEAD -- "$@" | cmp -s - "$patch"
}
if unsaved "$build/sdk-shared-core" ../../core/sdk-shared-core/sdk-shared-core.patch src Cargo.toml ||
  unsaved "$build/sdk-rust" patches/sdk-rust.patch src tests examples Cargo.toml rust-toolchain.toml; then
  echo "$build has edits that the patches do not: run patches/regenerate.sh first, or remove .build/ to drop them" >&2
  exit 1
fi

mkdir -p "$build"
checkout https://github.com/restatedev/sdk-shared-core.git $SHARED_CORE_REV "$build/sdk-shared-core"
git -C "$build/sdk-shared-core" apply "$here/../../core/sdk-shared-core/sdk-shared-core.patch"
checkout https://github.com/restatedev/sdk-rust.git $SDK_RUST_REV "$build/sdk-rust"
if [ -s patches/sdk-rust.patch ]; then
  git -C "$build/sdk-rust" apply "$here/patches/sdk-rust.patch"
fi
# What make takes as the SDK being prepared.
touch "$build/sdk-rust/.patched"
