#!/usr/bin/env bash
# Freestanding C++ upper extension: wit-bindgen cpp + clang++ wasm32 +
# wasm-tools. No libc++ (src/cxxshim), no WASI. Zero downloads.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p gen target
wit-bindgen cpp --world extension --out-dir gen ../../wit/tau.wit
CFLAGS="--target=wasm32-unknown-unknown -nostdlib -fno-builtin -O2"
# -std=c++23: host-channel result<> returns map to std::expected (0.2.0)
CXXFLAGS="$CFLAGS -fno-exceptions -fno-rtti -std=c++23"
INCLUDES="-I src/cxxshim -I ../c-upper/src/shim -I gen"
clang -c $CFLAGS -I ../c-upper/src/shim -o target/shim.o ../c-upper/src/shim.c
clang++ -c $CXXFLAGS $INCLUDES -o target/upper.o src/upper.cpp
clang++ -c $CXXFLAGS $INCLUDES -o target/extension.o gen/extension.cpp
clang++ -c $CXXFLAGS $INCLUDES -o target/cxxshim.o src/cxxshim.cpp
clang++ --target=wasm32-unknown-unknown -nostdlib \
    -mexec-model=reactor -Wl,--no-entry -Wl,--export-memory \
    -o target/cpp_upper.core.wasm \
    target/upper.o target/extension.o target/cxxshim.o target/shim.o \
    gen/extension_component_type.o
wasm-tools component new target/cpp_upper.core.wasm -o target/cpp_upper.wasm
echo "built target/cpp_upper.wasm"
