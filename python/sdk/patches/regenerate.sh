#!/usr/bin/env bash
# Regenerate the patches from the edited checkouts in .build/ (see setup.sh),
# including files the edits added. The lockfiles are left out: Cargo and uv
# rewrite them for the patched core.
set -euo pipefail
cd "$(dirname "$0")"
build=../.build

core=$build/sdk-shared-core
git -C "$core" add -N -- src
git -C "$core" diff -- src Cargo.toml > ../../../packages/sdk-shared-core/sdk-shared-core.patch

sdk=$build/sdk-python
git -C "$sdk" add -N -- src python tests
git -C "$sdk" diff -- src python tests Cargo.toml pyproject.toml > sdk-python.patch

grep -h '^diff --git' ../../../packages/sdk-shared-core/sdk-shared-core.patch sdk-python.patch | awk '{print $3}'
