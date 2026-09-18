#!/bin/sh
set -eu
exec "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/scripts/verify-reproducible.sh"

