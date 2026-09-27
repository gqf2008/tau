#!/usr/bin/env bash
# Python upper extension via componentize-py (pip install componentize-py).
# The runtime discovers exported-interface implementations by module
# attribute name (Tools / Hooks — see src/upper.py); the wit_world
# bindings are injected at componentize time, no local bindings needed.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p target
componentize-py -d ../../wit/tau.wit -w extension \
    componentize -p src upper -o target/py_upper.wasm
echo "built target/py_upper.wasm"
