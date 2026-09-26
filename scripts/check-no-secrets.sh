#!/bin/sh
set -eu

ROOT=${1:-$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)}

if ! git -C "$ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "not a Git worktree: $ROOT" >&2
  exit 2
fi

failed=0

if ! git -C "$ROOT" ls-files | while IFS= read -r path; do
  case "/$path" in
    */.env|*/config.json|*.sqlite|*.sqlite3|*.sqlite3-*|*.db|*.pem|*.p12|*.pfx)
      echo "tracked sensitive file name: $path" >&2
      exit 1
      ;;
  esac

  if [ -f "$ROOT/$path" ] && LC_ALL=C head -c 16 "$ROOT/$path" | grep -q '^SQLite format 3'; then
    echo "tracked SQLite database: $path" >&2
    exit 1
  fi
done
then
  failed=1
fi

# The scanner and its self-test contain the detection expressions themselves,
# so exclude only those two implementation files from content matching.
if git -C "$ROOT" grep -n -I -E -e \
  '-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9_]{30,}|eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}' \
  -- . \
  ':(exclude)scripts/check-no-secrets.sh' \
  ':(exclude)tests/check-no-secrets.sh'
then
  echo "tracked content resembles a private key or access token" >&2
  failed=1
fi

test "$failed" -eq 0
