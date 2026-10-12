#!/usr/bin/env bash
# Model-check the protocol spec with TLC.
# Usage: spec/check.sh [config ...]; by default the quick configurations.
# MCDeep.cfg (three transactions) takes about an hour on four cores.
set -euo pipefail
cd "$(dirname "$0")"
JAR="${TLA2TOOLS:-$HOME/.cache/tla2tools-1.8.0.jar}"
if [ ! -f "$JAR" ]; then
    mkdir -p "$(dirname "$JAR")"
    curl -sSL -o "$JAR" https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar
fi
STATES="${TMPDIR:-/tmp}/tlc-states"
configs=("$@")
[ ${#configs[@]} -gt 0 ] || configs=(MC.cfg MCNoCache.cfg MCStrong.cfg)
for config in "${configs[@]}"; do
    echo "== $config"
    java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto \
        -metadir "$STATES" -config "$config" MC.tla |
        grep -E "violated|No error has been found|distinct states found|Finished in"
    rm -rf "$STATES"
done
