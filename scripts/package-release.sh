#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}
"$ROOT/scripts/build-reproducible.sh"
OUT="$ROOT/dist"
rm -rf "$OUT"
mkdir -p "$OUT/kiro-gateway-rs"
cp "$ROOT/target/release/kiro-gateway-rs" "$OUT/kiro-gateway-rs/"
cp "$ROOT/README.md" "$ROOT/config.example.json" "$OUT/kiro-gateway-rs/"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$SOURCE_DATE_EPOCH" -czf "$OUT/kiro-gateway-rs.tar.gz" -C "$OUT" kiro-gateway-rs
(cd "$OUT" && sha256sum kiro-gateway-rs.tar.gz > SHA256SUMS)

