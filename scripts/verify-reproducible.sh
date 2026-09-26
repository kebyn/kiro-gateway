#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TMP1=$(mktemp -d)
TMP2=$(mktemp -d)
cleanup() { rm -rf "$TMP1" "$TMP2"; }
trap cleanup EXIT INT TERM
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}
export CARGO_INCREMENTAL=0
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$ROOT=/kiro-gateway"
build() {
  CARGO_TARGET_DIR="$1" cargo build --manifest-path "$ROOT/Cargo.toml" --release --locked >/dev/null
}
build "$TMP1/target"
build "$TMP2/target"
sha256sum "$TMP1/target/release/kiro-gateway" "$TMP2/target/release/kiro-gateway"
test "$(sha256sum "$TMP1/target/release/kiro-gateway" | awk '{print $1}')" = "$(sha256sum "$TMP2/target/release/kiro-gateway" | awk '{print $1}')"
for BUILD_PATH in "$ROOT" "$TMP1" "$TMP2"; do
  if strings "$TMP1/target/release/kiro-gateway" | grep -F "$BUILD_PATH" >/dev/null; then
    echo "absolute build path found in binary: $BUILD_PATH" >&2
    exit 1
  fi
done
