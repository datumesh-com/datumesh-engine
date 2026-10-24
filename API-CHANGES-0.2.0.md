# API changes in datumesh-engine 0.2.0

Generated with `cargo public-api -ss --color=never diff 0.1.0` and the same with
`--all-features` (cargo-public-api 0.52.0, nightly-tools image) against the 0.1.0 release on
crates.io. Both listings are unchanged (`public-api.txt`, `public-api-all-features.txt`):

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

## Why 0.2.0 (breaking) all the same

The listing names `datumesh_store` paths, and those now mean `datumesh-store` 0.3 types:

| Item | Change |
|---|---|
| `Engine::builder(Arc<dyn datumesh_store::Store + Send + Sync>, ..)` | The store 0.3 trait (sealed: only `PgStore` implements it). A store from `datumesh-store` 0.2 no longer fits. |
| `StoreError::store_error(&self) -> &datumesh_store::Error` | The store 0.3 error type. |

## Behaviour

- A computed receipt's `asof` is the latest of its input asofs and the start of every name
  binding it relied on (`Store::resolve_binding`), instead of the request's asof when a
  name was bound after the inputs' asof.
- The engine's `Resolver` passes the core 0.3.3 test kit under the default
  `AnswerRule::TxAndAsof` (unchanged behaviour: the kit changed).
