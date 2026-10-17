#!/usr/bin/env bash
# Prepare the patched Restate Python SDK (see README.md).
#
# Clones the SDK and its shared core at the pinned revisions into .build/, and
# applies patches/ and the shared core's patch (../sdk-shared-core). The SDK's
# Cargo.toml takes the core from the clone next to it. The uv workspace (the
# repository's pyproject.toml) builds the SDK from .build/sdk-python with
# maturin and installs it (`make py`).
#
# Needs: git.
set -euo pipefail
cd "$(dirname "$0")"
here=$(pwd)
build=$here/.build

SHARED_CORE_REV=382649d0652aa6d971ec16e788d24b4b2c8c8b45 # as the TypeScript SDK's
SDK_PYTHON_REV=ad4ea7c0a60b5745681a809fe57656bec0939c38   # Release 1.0.5

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
  ! git -C "$dir" diff -- "$@" | cmp -s - "$patch"
}
if unsaved "$build/sdk-shared-core" ../sdk-shared-core/sdk-shared-core.patch src Cargo.toml ||
  unsaved "$build/sdk-python" patches/sdk-python.patch src python tests Cargo.toml pyproject.toml; then
  echo "$build has edits that the patches do not: run patches/regenerate.sh first, or remove .build/ to drop them" >&2
  exit 1
fi

mkdir -p "$build"
checkout https://github.com/restatedev/sdk-shared-core.git $SHARED_CORE_REV "$build/sdk-shared-core"
git -C "$build/sdk-shared-core" apply "$here/../sdk-shared-core/sdk-shared-core.patch"
checkout https://github.com/restatedev/sdk-python.git $SDK_PYTHON_REV "$build/sdk-python"
git -C "$build/sdk-python" apply "$here/patches/sdk-python.patch"
# What make takes as the SDK being prepared.
touch "$build/sdk-python/.patched"
