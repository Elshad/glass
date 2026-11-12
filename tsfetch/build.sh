#!/bin/sh
# Builds dist/tsfetch.wasm and copies Go's wasm_exec.js beside it.
# Needs Go (the version tailscale.com asks for; Go fetches it if yours is older).
set -eu
cd "$(dirname "$0")"
mkdir -p dist

(
  cd wasm
  go test .
  tags=$(go run ./tagsgen)
  GOOS=js GOARCH=wasm go build -trimpath -ldflags "-s -w" \
    -tags "$tags" -o ../dist/tsfetch.wasm .
)
cp "$(go env GOROOT)/lib/wasm/wasm_exec.js" dist/wasm_exec.js

if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --enable-bulk-memory dist/tsfetch.wasm -o dist/tsfetch.wasm
fi
if command -v brotli >/dev/null 2>&1; then
  brotli -f -q 9 -k dist/tsfetch.wasm
fi
ls -l dist
