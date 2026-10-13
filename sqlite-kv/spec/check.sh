#!/usr/bin/env bash
# Model-check the protocol spec with TLC.
# Usage: spec/check.sh [config ...]; by default the quick configurations.
# MCDeep.cfg (three transactions) takes about half an hour on four cores.
set -euo pipefail
cd "$(dirname "$0")"
JAR="${TLA2TOOLS:-$HOME/.cache/tla2tools-1.8.0.jar}"
if [ ! -f "$JAR" ]; then
    mkdir -p "$(dirname "$JAR")"
    # Into place only once whole: an interrupted download is not taken as the jar.
    curl -fsSL -o "$JAR.part" https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar
    mv "$JAR.part" "$JAR"
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
configs=("$@")
[ ${#configs[@]} -gt 0 ] || configs=(MC.cfg MCNoCache.cfg MCStrong.cfg)
for config in "${configs[@]}"; do
    echo "== $config"
    status=0
    java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto \
        -metadir "$work/states" -config "$config" MC.tla > "$work/tlc.log" 2>&1 || status=$?
    rm -rf "$work/states"
    if [ "$status" -ne 0 ]; then
        # A violation (with its trace) or an error in the spec: TLC's whole output.
        cat "$work/tlc.log"
        exit "$status"
    fi
    if ! grep -q "No error has been found" "$work/tlc.log"; then
        # TLC succeeded without saying the check passed: show what it said.
        cat "$work/tlc.log"
        echo "TLC did not report that no error was found" >&2
        exit 1
    fi
    grep -E "No error has been found|distinct states found|Finished in" "$work/tlc.log"
done
