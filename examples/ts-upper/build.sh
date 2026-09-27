#!/usr/bin/env bash
# TypeScript upper extension: jco componentize bundles TS automatically.
# --disable http fetch-event: see the JavaScript example.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p target
npm install --no-fund --no-audit --silent
npx jco componentize src/upper.ts \
    --wit ../../wit/tau.wit --world-name extension \
    --disable http fetch-event \
    -o target/ts_upper.wasm
echo "built target/ts_upper.wasm"
