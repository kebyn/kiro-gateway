#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
FIXTURE=$(mktemp -d)
cleanup() { rm -rf "$FIXTURE"; }
trap cleanup EXIT INT TERM

git -C "$FIXTURE" init -q
printf '%s\n' 'client-key-change-me' >"$FIXTURE/safe-placeholder.txt"
git -C "$FIXTURE" add safe-placeholder.txt
"$ROOT/scripts/check-no-secrets.sh" "$FIXTURE"

printf '%s\n' 'do-not-track-secrets' >"$FIXTURE/.env"
git -C "$FIXTURE" add .env
if "$ROOT/scripts/check-no-secrets.sh" "$FIXTURE" >/dev/null 2>&1; then
  echo "secret check accepted a tracked .env file" >&2
  exit 1
fi
git -C "$FIXTURE" rm -q --cached .env
rm "$FIXTURE/.env"

printf '%s\n' '-----BEGIN PRIVATE KEY-----' >"$FIXTURE/unsafe.txt"
git -C "$FIXTURE" add unsafe.txt
if "$ROOT/scripts/check-no-secrets.sh" "$FIXTURE" >/dev/null 2>&1; then
  echo "secret check accepted a private-key marker" >&2
  exit 1
fi
