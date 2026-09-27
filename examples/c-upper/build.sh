#!/usr/bin/env bash
# Build the freestanding C upper extension. Zero downloads: wit-bindgen
# (bindings) + clang's wasm32 target (compile) + wasm-tools (componentize).
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p gen target
wit-bindgen c --world extension --out-dir gen ../../wit/tau.wit
clang --target=wasm32-unknown-unknown -nostdlib -fno-builtin -O2 \
    -I src/shim -I gen \
    -mexec-model=reactor -Wl,--no-entry -Wl,--export-memory \
    -o target/c_upper.core.wasm \
    src/upper.c src/shim.c gen/extension.c gen/extension_component_type.o
wasm-tools component new target/c_upper.core.wasm -o target/c_upper.wasm
echo "built target/c_upper.wasm"
