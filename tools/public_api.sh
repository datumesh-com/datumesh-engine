#!/bin/sh
# The library's public API (`cargo public-api -ss`, which needs a nightly toolchain and
# cargo-public-api 0.52): public-api.txt with default features, public-api-all-features.txt
# with --all-features (native-test adds the module native_test); each compared with the
# file, or written with --write. Plain output whatever CARGO_TERM_COLOR says (CI sets
# it to always): --color=never.
#
#   tools/public_api.sh [--write]            in the datumesh-nightly-tools image (tools/cargo.sh)
#   tools/public_api.sh --native [--write]   with the local cargo (CI: nightly-2026-10-22)
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
native=false
write=false
for a in "$@"; do
  case "$a" in --native) native=true ;; --write) write=true ;; *) echo "unknown option $a" >&2; exit 2 ;; esac
done
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

api() {
  if $native; then
    (cd "$root" && cargo public-api -ss --color=never "$@")
  else
    "$root/tools/cargo.sh" nightly-tools public-api -ss --color=never "$@"
  fi
}
api > "$work/default.txt"
api --all-features > "$work/all.txt"
# Without native-test, nothing of it; with it, only additions.
if grep -q native_test "$work/default.txt"; then
  echo "native_test in the default API" >&2
  exit 1
fi
if [ -n "$(diff "$work/default.txt" "$work/all.txt" | grep '^<' || true)" ]; then
  echo "--all-features removes items:" >&2
  diff "$work/default.txt" "$work/all.txt" >&2
  exit 1
fi

if $write; then
  cp "$work/default.txt" "$root/public-api.txt"
  cp "$work/all.txt" "$root/public-api-all-features.txt"
  echo "public-api.txt and public-api-all-features.txt written"
else
  diff -u "$root/public-api.txt" "$work/default.txt"
  diff -u "$root/public-api-all-features.txt" "$work/all.txt"
  echo "public-api.txt and public-api-all-features.txt are current"
fi
