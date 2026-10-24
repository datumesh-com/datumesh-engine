# Changelog

All notable changes to `datumesh-engine`.

## 0.1.0 (unreleased)

The first release: the engine of step 1.2b, on `datumesh-store` 0.2.0 and `datumesh-core`
0.3.2 (spec-0.3.0).

- `Engine::get(&CellRef, asof)` and `Engine::get_cell(&Cell, asof)`: the receipt answering as
  of A (its `tx` and its `asof` both at or before A), or `Answer::NotCovered`. Stored cells:
  the publisher's receipt, unchanged. Computed cells: the computed version with the greatest
  `asof <= A`, validated (each input gives the same receipt or the same value, its name
  resolving to the same function); else evaluated through a `Runtime` and signed by the mesh
  (`computed_by` mesh host#key id; `tx` the latest input tx; `asof` the latest input asof,
  later where needed for the receipt to hold as of it), or the version kept when the value
  is the same. New receipts and versions (`put_computed`) are written only when the request
  succeeded.
- `Runtime`, `Inputs`, `Budget`: the contract for function languages. Evaluation on an
  explicit stack; a read of an input not computed yet suspends the function, which is run
  again once the input is answered. `Cycle`, a depth limit (default 10,000), fuel per request
  (default 10,000,000).
- The memo: answers with the interval of asofs they hold for and the names they depend on;
  `stored_version_added` drops what rests on a cell (dirty set, revision marks for
  evaluations running meanwhile); dropped answers are rechecked from memory. Single-flight:
  evaluations run one at a time per engine.
- `Engine` is a time-aware `datumesh_core::Resolver` (the engine's answering rule; computed
  cells are evaluated).
- `EngineBuilder`: store, mesh host, signing key (or a seed file), runtimes, depth limit,
  fuel, memo capacity, licence and summary of computed receipts, clock. It checks that the
  store holds the mesh's key document with the signing key.
- Feature `native-test`: `NativeTest`, Rust closures keyed by function hash.
- Errors: `EvalError`, `EngineError` (the brief's variants, plus `Suspended`,
  `InputNotCovered`, `InvalidInput` / `Invalid`, `Sign`), `ConfigError`, `StoreError` (the
  store's error in the engine's words).
