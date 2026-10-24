#!/bin/sh
# Packaging test: `cargo package`, unpack the .crate in a fresh directory, build a new
# project that depends on the packaged datumesh-engine and on datumesh-store 0.3.0 and
# datumesh-core 0.3.3 from crates.io, and run the README example against PostgreSQL 16 in
# a database of its own (empty: the example makes the schema), twice (it is idempotent).
# The packaged crate must carry no test material.
#
#   tools/package_test.sh [toolchain]   in fresh rust:<toolchain> containers on the
#                                       `datumesh` network, against datumesh-pg
#   tools/package_test.sh --native      with the local cargo and DATABASE_URL (CI)
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
work="$(mktemp -d)"
db="dm_engine_package_$$"

if [ "$1" = "--native" ]; then
  (cd "$root" && cargo package --locked)
  pkg="$root/target/package"
  admin="$DATABASE_URL"
  in_fresh() { (cd "$work" && sh -c "$1"); }
  psql_admin() { psql "$admin" -qc "$1"; }
else
  tag="${1:-1.99}"
  "$root/tools/cargo.sh" "$tag" package --locked
  pkg="$root/target-$tag/package"
  admin="postgres://postgres:postgres@datumesh-pg:5432/postgres"
  in_fresh() {
    docker run --rm --network datumesh -v "$work:/p" -w /p -e DATABASE_URL="$DATABASE_URL" "rust:$tag" sh -c "$1"
  }
  psql_admin() { docker exec datumesh-pg psql -U postgres -qc "$1"; }
fi

cp "$pkg/datumesh-engine-$version.crate" "$work/"
(cd "$work" && tar xzf "datumesh-engine-$version.crate")
if tar tzf "$work/datumesh-engine-$version.crate" | grep -E '/tests/|/tools/|/bench/|/examples/'; then
  echo "test material in the packaged crate" >&2
  exit 1
fi
mkdir -p "$work/app/src"
# The README's first ```rust block is the example.
awk '/^```rust$/{f=1;next} /^```$/{if(f){exit}} f' "$root/README.md" > "$work/app/src/main.rs"
printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\ndatumesh-engine = { path = "../datumesh-engine-%s" }\ndatumesh-store = "0.3.0"\ndatumesh-core = { version = "0.3.3", features = ["sign"] }\n' "$version" > "$work/app/Cargo.toml"

psql_admin "CREATE DATABASE $db"
trap 'psql_admin "DROP DATABASE IF EXISTS $db WITH (FORCE)"; rm -rf "$work"' EXIT
DATABASE_URL="${admin%/*}/$db"
export DATABASE_URL
in_fresh "cd app && cargo run -q && cargo run -q"
echo "package test passed ($version)"
