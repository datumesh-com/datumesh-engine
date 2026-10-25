#!/bin/sh
# Cross-language check: the test suite runs with DATUMESH_ENGINE_DUMP set, so every test
# that checks the receipts the engine signed (each at level recomputed, through Engine as
# the resolver) also writes them, with the key documents of the mesh and the publishers, as
# JSON lines, with the manifest of every slice entry. Then the Python library datumesh 0.4.0 (PyPI, in python:3.13-slim) and the
# JavaScript library @datumesh/verify 0.4.0 (npm, in node:22-slim) verify each at level
# signed, check its receipt hash, and check that each manifest gives its slice entry.
#
#   tools/crosscheck.sh [toolchain]   cargo in a rust:<toolchain> container (tools/cargo.sh)
#   tools/crosscheck.sh --native      the local cargo and DATABASE_URL (CI)
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
dump="$root/target-crosscheck"
rm -rf "$dump"
mkdir -p "$dump"
if [ "$1" = "--native" ]; then
  (cd "$root" && DATUMESH_ENGINE_DUMP="$dump" cargo test --all-features --locked -q)
else
  DATUMESH_ENGINE_DUMP=/w/target-crosscheck "$root/tools/cargo.sh" "${1:-1.99}" test --all-features --locked -q
fi
cat "$dump"/*.jsonl > "$dump/all.txt"
echo "$(wc -l < "$dump/all.txt") receipt lines from $(ls "$dump"/*.jsonl | wc -l) tests"
docker run --rm -v "$dump:/d:ro" -v "$root/tools/crosscheck:/s:ro" python:3.13-slim \
  sh -c 'pip install -q --disable-pip-version-check --root-user-action=ignore datumesh==0.4.0 && python -I /s/verify.py /d/all.txt'
docker run --rm -v "$dump:/d:ro" -v "$root/tools/crosscheck:/s:ro" node:22-slim \
  sh -c 'mkdir -p /tmp/x && cp /s/verify.mjs /tmp/x/ && cd /tmp/x && npm install -s --no-fund --no-audit @datumesh/verify@0.4.0 && node verify.mjs /d/all.txt'
