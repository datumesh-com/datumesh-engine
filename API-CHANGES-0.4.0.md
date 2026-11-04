# API changes in datumesh-engine 0.4.0

Generated with `cargo public-api -ss --color=never diff 0.3.2` (cargo-public-api 0.52.0,
nightly-tools image) against the 0.3.2 release on crates.io, with default features and with
`--all-features`; the full output is at the end. **No item removed, changed or added.**
`public-api.txt` and `public-api-all-features.txt` are regenerated (unchanged).

The release is breaking all the same: the listing names `datumesh_store::Store` (in
`Engine::builder`) and `datumesh_store::error::Error` (from `StoreError::store_error`) without
their version, and those are now `datumesh-store` 0.4.0's, a semver-incompatible release of
the store. A `PgStore` (or `SqliteStore`) from store 0.3 is not a store 0.4 `Store`, so users
move to `datumesh-store` 0.4 together with this release; nothing else is asked of them.

Behaviour: none changed. Code: none changed. Dependency: `datumesh-store` 0.4.0 (was 0.3.2),
with `default-features = false`. Feature `sqlite` (new, off) enables the store's SQLite
backend; it adds nothing to the engine's API (`--all-features` lists the same items with it).

## Full diff

Default features and `--all-features` give the same output:

```text
Removed items from the public API
=================================
(none)

Changed items in the public API
===============================
(none)

Added items to the public API
=============================
(none)

```
