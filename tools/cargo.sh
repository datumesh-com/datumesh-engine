#!/bin/sh
# Run cargo in a toolchain container on the `datumesh` Docker network, where the
# development database (a postgres:16 container named datumesh-pg) answers:
#   tools/cargo.sh <image-tag> <cargo args...>, e.g. tools/cargo.sh 1.99 test --all-features
# Tags: a Rust version (image rust:<tag>), nightly-tools (cargo-public-api), lint.
set -e
tag="$1"; shift
case "$tag" in nightly) image=rustlang/rust:nightly ;; nightly-tools) image=datumesh-nightly-tools ;; lint) image=datumesh-rust-1.99-lint ;; *) image="rust:$tag" ;; esac
root="$(cd "$(dirname "$0")/.." && pwd)"
exec docker run --rm -i --network datumesh -v "$HOME/.cache/datumesh-cargo:/usr/local/cargo/registry" -v "$root:/w" -w /w \
  -e CARGO_TARGET_DIR="/w/target-$tag" -e CARGO_TERM_COLOR=never \
  -e DATABASE_URL="${DATABASE_URL:-postgres://postgres:postgres@datumesh-pg:5432/postgres}" \
  -e RUST_LOG -e DATUMESH_ENGINE_DUMP -e RUST_TEST_THREADS "$image" cargo "$@"
