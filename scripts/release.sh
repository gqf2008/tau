#!/usr/bin/env bash
# Release packaging: test, build wasm examples, build the release binary,
# assemble dist/tau-<version>-<target>.zip with binary + docs + examples.
#
# Usage: scripts/release.sh            (full run)
#        SKIP_TESTS=1 scripts/release.sh
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(cargo metadata --no-deps --format-version 1 \
    | python -c "import json,sys; print(json.load(sys.stdin)['workspace_package']['version'])" 2>/dev/null \
    || grep -m1 '^version' Cargo.toml | sed 's/[^0-9.]//g')
TARGET=$(rustc -vV | sed -n 's/^host: //p')
NAME="tau-${VERSION}-${TARGET}"
DIST="dist/${NAME}"

if [ "${SKIP_TESTS:-0}" != "1" ]; then
    echo "== cargo test --workspace"
    cargo test --workspace
fi

echo "== wasm examples (wasm32-wasip2, release)"
for ex in upper echo-provider mcp-bridge http-provider guard; do
    cargo build --manifest-path "examples/${ex}/Cargo.toml" \
        --target wasm32-wasip2 --release --quiet
done

echo "== cargo build --release -p tau-cli"
cargo build --release -p tau-cli --quiet

echo "== assemble ${DIST}"
rm -rf "${DIST}" "${DIST}.zip"
mkdir -p "${DIST}/docs" "${DIST}/examples"

EXE=target/release/tau
[ -f target/release/tau.exe ] && EXE=target/release/tau.exe
cp "${EXE}" "${DIST}/"
cp README.md LICENSE "${DIST}/"
cp docs/*.md "${DIST}/docs/"
for ex in upper echo_provider mcp_bridge http_provider guard; do
    wasm="examples/${ex//_/-}/target/wasm32-wasip2/release/${ex}.wasm"
    cp "${wasm}" "${DIST}/examples/"
done
cp examples/README.md "${DIST}/examples/"

python - "$PWD/dist" "${NAME}" <<'PY'
import shutil, sys
shutil.make_archive(f"{sys.argv[1]}/{sys.argv[2]}", "zip", sys.argv[1], sys.argv[2])
PY
echo "== dist/${NAME}.zip"
ls -la "${DIST}.zip"
