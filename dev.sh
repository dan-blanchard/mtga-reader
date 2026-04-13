#!/bin/bash
# Quick dev loop: check, build, sync to VM, run test
set -e
. "$HOME/.cargo/env"

echo "=== Checking macOS compile ==="
cargo check --lib 2>&1 | grep "^error" && exit 1 || echo "OK"

echo "=== Syncing to VM ==="
scp src/mono/scanner.rs danblanchard@10.211.55.3:C:/mtga-reader/src/mono/scanner.rs
scp Cargo.toml danblanchard@10.211.55.3:C:/mtga-reader/Cargo.toml

echo "=== Building on VM ==="
ssh danblanchard@10.211.55.3 "cd C:\mtga-reader && npm run build 2>&1" | tail -3

echo "=== Running test ==="
ssh danblanchard@10.211.55.3 "cd C:\mtga-reader && powershell -Command \"\\\$env:MTGA_DEBUG_MONO='1'; node -e \\\"const r = require('./index.js'); const t=Date.now(); const c = r.readMtgaCardsMono('MTGA.exe'); console.log('Time: '+(Date.now()-t)+'ms'); console.log(JSON.stringify(c).substring(0,500))\\\" 2>&1\"" | tail -20

echo "=== Done ==="
