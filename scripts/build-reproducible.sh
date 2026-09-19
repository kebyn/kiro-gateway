#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0}
export CARGO_INCREMENTAL=0
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$ROOT=/kiro-gateway"
cd "$ROOT"
cargo build --release --locked
