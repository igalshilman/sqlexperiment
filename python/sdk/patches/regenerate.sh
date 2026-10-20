#!/usr/bin/env bash
# Regenerate the patch from the edited checkout in .build/ (see setup.sh),
# including files the edits added. The lockfiles are left out: Cargo and uv
# rewrite them.
set -euo pipefail
cd "$(dirname "$0")"
build=../.build

sdk=$build/sdk-python
git -C "$sdk" add -N -- src python tests
git -C "$sdk" diff -- src python tests Cargo.toml pyproject.toml > sdk-python.patch

grep '^diff --git' sdk-python.patch | awk '{print $3}'
