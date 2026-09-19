#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}
"$ROOT/scripts/build-reproducible.sh"
OUT="$ROOT/dist"
rm -rf "$OUT"
mkdir -p "$OUT/kiro-gateway"
cp "$ROOT/target/release/kiro-gateway" "$OUT/kiro-gateway/"
cp "$ROOT/README.md" "$ROOT/config.example.json" "$OUT/kiro-gateway/"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$SOURCE_DATE_EPOCH" -czf "$OUT/kiro-gateway.tar.gz" -C "$OUT" kiro-gateway
(cd "$OUT" && sha256sum kiro-gateway.tar.gz > SHA256SUMS)
