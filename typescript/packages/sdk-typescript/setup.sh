#!/usr/bin/env bash
# Build the patched Restate TypeScript SDK (see README.md).
#
# Clones the SDK at the pinned release into .build/, applies patches/, and
# packs the SDK as .build/restatedev-restate-sdk-1.17.4.tgz, which the pnpm
# workspace installs (`make js`). The release's shared core (8.1.0, compiled
# to WebAssembly and checked in) is used as it is.
#
# Needs: git, node and pnpm.
set -euo pipefail
cd "$(dirname "$0")"
here=$(pwd)
build=$here/.build

SDK_TYPESCRIPT_REV=b1080a11984b35419e0e825a0e82bc17c656a22d # Release 1.17.4

checkout() { # url rev dir
  rm -rf "$3"
  git init -q "$3"
  git -C "$3" fetch -q --depth 1 "$1" "$2"
  git -C "$3" checkout -q FETCH_HEAD
}

mkdir -p "$build"
checkout https://github.com/restatedev/sdk-typescript.git $SDK_TYPESCRIPT_REV "$build/sdk-typescript"
git -C "$build/sdk-typescript" apply "$here/patches/sdk-typescript.patch"

(cd "$build/sdk-typescript" && pnpm install --frozen-lockfile && pnpm --filter @restatedev/restate-sdk run build)
(cd "$build/sdk-typescript/packages/libs/restate-sdk" && pnpm pack --pack-destination "$build")
