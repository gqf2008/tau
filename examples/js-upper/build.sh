#!/usr/bin/env bash
# JavaScript upper extension: jco componentize (StarlingMonkey backend).
# --disable http fetch-event: the StarlingMonkey runtime links wasi:http
# by default, which tau's ambient-WASI linker does not provide (extensions
# needing network go through tau's `http` capability instead).
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p target
npm install --no-fund --no-audit --silent
npx @bytecodealliance/jco componentize src/upper.js \
    --wit ../../wit/tau.wit --world-name extension \
    --disable http fetch-event \
    -o target/js_upper.wasm
echo "built target/js_upper.wasm"
