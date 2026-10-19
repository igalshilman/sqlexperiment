#!/usr/bin/env bash
# Build the Python engine: CPython for WASI, as a reactor the runtime drives
# (reactor.c), and its standard library with bytecode already compiled.
#
#   rust/quickjs-durable/python/build.sh     (or: make python-engine)
#
# Into $CARGO_TARGET_DIR/pywasm (by default target/pywasm): the toolchain
# (wasi-sdk, a Wasmtime CLI that CPython's configure runs), CPython's source
# and build, and out/ — python.wasm and lib/, the runtime's QJS_PYTHON.
# CPython's build takes some minutes, once; relinking the reactor, seconds.
set -euo pipefail

CPYTHON=3.15.0
WASI_SDK=33
WASMTIME=49.0.3

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
work=${CARGO_TARGET_DIR:-$root/target}/pywasm
mkdir -p "$work"
cd "$work"

arch=$(uname -m | sed 's/aarch64/arm64/')
sdk=$work/wasi-sdk-$WASI_SDK.0-$arch-linux
if [ ! -d "$sdk" ]; then
  curl -fsSL "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-$WASI_SDK/wasi-sdk-$WASI_SDK.0-$arch-linux.tar.gz" | tar xz
fi
wasmtime=$work/wasmtime-v$WASMTIME-$(uname -m)-linux
if [ ! -d "$wasmtime" ]; then
  curl -fsSL "https://github.com/bytecodealliance/wasmtime/releases/download/v$WASMTIME/wasmtime-v$WASMTIME-$(uname -m)-linux.tar.xz" | tar xJ
fi
source=$work/Python-$CPYTHON
if [ ! -d "$source" ]; then
  curl -fsSL "https://www.python.org/ftp/python/$CPYTHON/Python-$CPYTHON.tar.xz" | tar xJ
fi

host=$source/cross-build/wasm32-wasip1
build_python=$source/cross-build/$(uname -m)-pc-linux-gnu/python
if [ ! -f "$host/libpython3.15.a" ]; then
  (cd "$source" && PATH=$wasmtime:$PATH WASI_SDK_PATH=$sdk python3 Platforms/WASI build)
fi

# The reactor: CPython's library and modules, with reactor.c's exports instead
# of a main. A small stack, first in memory: it overflows into a trap, and
# the runtime saves only what is above its pointer.
out=$work/out
rm -rf "$out.tmp"
mkdir -p "$out.tmp"
"$sdk/bin/clang" --target=wasm32-wasip1 -O2 -I"$source/Include" -I"$host" \
  -c "$here/reactor.c" -o "$work/reactor.o"
"$sdk/bin/clang" --target=wasm32-wasip1 -mexec-model=reactor \
  -z stack-size=2097152 -Wl,--stack-first -Wl,--strip-debug \
  -Wl,--export=__heap_base -Wl,--export=__stack_pointer \
  -o "$out.tmp/python.wasm" "$work/reactor.o" "$host/libpython3.15.a" \
  "$host"/Modules/_hacl/libHacl_*.a "$host/Modules/expat/libexpat.a" \
  -ldl -lwasi-emulated-signal -lwasi-emulated-getpid -lwasi-emulated-process-clocks -lpthread -lm

# The standard library, without tests and GUI toolkits, with bytecode that
# imports check by its source's hash, never by file times.
mkdir -p "$out.tmp/lib"
(cd "$source/Lib" && tar cf - --exclude=__pycache__ --exclude=test --exclude=idlelib \
  --exclude=tkinter --exclude=turtledemo --exclude=ensurepip --exclude=site-packages .) |
  (cd "$out.tmp/lib" && tar xf -)
"$build_python" -E -m compileall -q -j 0 --invalidation-mode unchecked-hash "$out.tmp/lib" >/dev/null

rm -rf "$out"
mv "$out.tmp" "$out"
echo "Python engine in $out"
