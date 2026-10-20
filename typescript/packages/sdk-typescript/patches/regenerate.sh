#!/usr/bin/env bash
# Regenerate the patch from the edited checkout in .build/ (see setup.sh),
# including files the edits added.
set -euo pipefail
cd "$(dirname "$0")"
sdk=../.build/sdk-typescript
paths=(packages/libs/restate-sdk/src packages/libs/restate-sdk/test)
git -C "$sdk" add -N -- "${paths[@]}"
git -C "$sdk" diff HEAD -- "${paths[@]}" > sdk-typescript.patch
grep '^diff --git' sdk-typescript.patch | awk '{print $3}'
