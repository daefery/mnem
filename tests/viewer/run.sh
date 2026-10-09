#!/bin/sh
# Run the viewer checks against a demo record, in a throwaway home: nothing in ~/.ravnori is
# read or changed. From the repository root:
#
#   cargo build --release && (cd tests/viewer && npm ci && sh run.sh)
#
# RAVNORI_BIN picks the ravnori binary (default: target/release/rvn), RAVNORI_CHROME a browser
# binary (default: the installed Chrome), PORT the port (default 37790). Extra arguments
# go to features.js, e.g. a fingerprint path: sh run.sh /tmp/fingerprint.json
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin=${RAVNORI_BIN:-$root/target/release/rvn}
port=${PORT:-37790}
[ -x "$bin" ] || { echo "no ravnori binary at $bin: run cargo build --release first" >&2; exit 2; }
[ -d "$here/node_modules/playwright-core" ] || { echo "run npm ci in tests/viewer first" >&2; exit 2; }

home=$(mktemp -d "${TMPDIR:-/tmp}/ravnori-viewer-check.XXXXXX")
pid=""
cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$home"
}
trap cleanup EXIT INT TERM

export HOME="$home" RAVNORI_HOME="$home/.ravnori"
db="$RAVNORI_HOME/ravnori.db"
mkdir -p "$RAVNORI_HOME"
"$bin" --db "$db" doctor >/dev/null 2>&1 || true # creates the schema
# A fixed "now", so every run builds the same record and fingerprints compare.
RAVNORI_DEMO_NOW=${RAVNORI_DEMO_NOW:-1790000000000} python3 "$root/docs/demo/make-demo.py" "$db" >/dev/null
"$bin" --db "$db" backup >/dev/null # one backup, so the backup list has a row

"$bin" --db "$db" ui --port "$port" >"$home/ui.log" 2>&1 &
pid=$!
i=0
until curl -fs "http://127.0.0.1:$port/" >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -le 50 ] || { echo "viewer did not start:" >&2; cat "$home/ui.log" >&2; exit 1; }
  sleep 0.2
done

node "$here/features.js" "http://127.0.0.1:$port" "$@"
