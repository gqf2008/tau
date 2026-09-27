#!/usr/bin/env bash
# Go upper extension: wit-bindgen go bindings + TinyGo (core module) +
# wasm-tools componentize (embed world + preview1 reactor adapter).
#
# Env: TINYGO=<tinygo binary>, WASMOPT=<wasm-opt binary>,
#      ADAPTER=<wasi_snapshot_preview1.reactor.wasm path>
#
# -buildmode=c-shared (reactor): its _initialize runs initHeap BEFORE
# initRand (scheduler_none.go/command modules do the reverse), and initRand
# reaches cabi_realloc through the adapter's stack allocation, so a command
# module can never survive that call. The asyncify scheduler is required:
# scheduler=none refuses the `go initAll()` in the reactor entry.
# patch_tinygo.py fixes TinyGo-incompatible bits in the generated code
# (runtime.Pinner / AddCleanup / sbrk linkname / pre-heap GC allocs —
# see the script).
set -euo pipefail
cd "$(dirname "$0")"
TINYGO="${TINYGO:-tinygo}"
ADAPTER="${ADAPTER:-wasi_snapshot_preview1.reactor.wasm}"
mkdir -p target
wit-bindgen go --world extension --out-dir . ../../wit/tau.wit
go mod edit -require=go.bytecodealliance.org/pkg@v0.2.3
go mod edit -go=1.25
go mod vendor >/dev/null 2>&1
python patch_tinygo.py
GOFLAGS=-mod=vendor "$TINYGO" build -buildmode=c-shared -target=wasi -opt=0 \
    -o target/go_upper.core.wasm .
wasm-tools component embed ../../wit/tau.wit --world extension target/go_upper.core.wasm \
    -o target/go_upper.embed.wasm
wasm-tools component new target/go_upper.embed.wasm \
    --adapt wasi_snapshot_preview1="$ADAPTER" -o target/go_upper.wasm
echo "built target/go_upper.wasm"
