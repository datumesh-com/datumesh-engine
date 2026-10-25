# Changelog

All notable changes to `datumesh-engine`.

## 0.3.0 (2026-10-25)

On `datumesh-store` 0.3.1 and `datumesh-core` 0.3.4 (spec-0.4.0). Breaking
(`API-CHANGES-0.3.0.md`): `Runtime::evaluate` takes a `FunctionDescriptor`; `Inputs` has
new required methods; `Answer` is `#[non_exhaustive]` and `NotCovered` carries a fault.

### Added

- Slice reads (SPEC 15.13): `Inputs::read_slice(fn_name, pattern)` with `Pattern` (`Value`,
  `Open`, `Range`) and `SlicePattern`, giving `Member`s in key order. A stored function's
  members come from `Store::slice_members`; a computed function's from its declared `extent`
  (or, for a range over a `date` or integer `number` argument without one, every day or
  integer), each member evaluated, memoised and checked for cycles like any input, every
  missing member evaluated before the reader runs again (one restart per slice); a member
  without a value makes the read the fault `E_FORMULA_ABSENT`. One `inputs` entry
  `slice:<slice id>:<members hash>` per slice; its manifest is stored with `put_manifest`
  when the request succeeds.
- `Inputs::read_cell` (value, receipt hash, `tx`, `asof`), `Inputs::read_asof(fn, args, T)`
  (the name resolved and the cell answered as of `T`; a computed cell evaluated as of `T`),
  `Inputs::read_versions(fn, args, from, to)` (a version slice), `Inputs::binding(fn, at)`
  (`FnBinding`: hash, start, descriptor), `Inputs::asof()`; `E_FORMULA_FUTURE` for a `T` or
  `to` after the evaluation asof. `Input`, `Member` (`#[non_exhaustive]`, with `new` for a
  runtime's tests).
- `EvalError::Fault(datumesh_core::Fault)`: a runtime's evaluation fault gives
  `Answer::NotCovered { fault: Some(code) }`, no receipt, nothing stored for the cell; a
  reader of a faulted cell gets the fault and may catch it.
- `Runtime::asof_dependent(descriptor)` (provided: `false`) and
  `NativeTest::set_asof_dependent`: an asof-dependent function's receipts have the evaluation
  asof as `asof`, one per requested asof, and are never reused for another asof (memo or
  store). A function that reads `Inputs::asof` is asof-dependent for that evaluation.
- Normative fuel: `Budget::used` (the attempt's fuel); each attempt has its own meter, so a
  cell's fuel is that of its completed attempt whatever the restarts, and the request's fuel
  bounds the sum of the completed attempts. `EngineBuilder::work_limit` (default
  `DEFAULT_WORK_FACTOR` = 16 times the fuel) bounds all work, discarded attempts and
  validation reads included: `EngineError::WorkExceeded`.
- `Resolver::manifest` (from the store) and `Resolver::evaluation` with the lineage of a
  computed cell's evaluation, so that a verifier reports `E_INPUT_SUPERSEDED`.
- `MemoStats::slice_patterns`.

### Changed

- **Breaking:** `Runtime::evaluate` receives the `FunctionDescriptor` (with `order` and
  `extent`); descriptors are stored and read as `FunctionDescriptor`.
- **Breaking:** `Answer` is `#[non_exhaustive]`; `Answer::NotCovered { fault }`
  (`Answer::NOT_COVERED` is the one without a fault).
- A computed receipt's `asof` also counts slice members, every `T` of `read_asof` and every
  `to` of `read_versions`; for an asof-dependent function it is the evaluation asof. Its
  `tx` counts members; with no input receipt (only empty slices) it is the earliest instant.
- Validation compares slice entries by members hash, then by the members' values (early
  cutoff); `stored_version_added` also drops the answers whose slice pattern the new version
  falls into (it reads the version's receipt from the store when slices are memoised).
- Validation reads are work, not normative fuel. A request that 0.2.0 ran within its fuel
  runs within it now (each evaluation is charged once, not once per restart).
- Versions resting on a caught fault, on a read at a transaction time, or on an undeclared
  `asof()` read are signed and stored as receipts but not kept as computed versions.
- A backdated correction of an input: recomputing the old receipt through the engine now
  reports `E_INPUT_SUPERSEDED` (with the lineage), at level signed, where 0.2.0 gave
  `E_VALUE_MISMATCH`.

## 0.2.0 (2026-10-24)

On `datumesh-store` 0.3.0 and `datumesh-core` 0.3.3.

- **Breaking:** the public API names `datumesh-store` 0.3 types (`Engine::builder` takes an
  `Arc<dyn datumesh_store::Store + Send + Sync>`; `StoreError::store_error` returns a
  `datumesh_store::Error`), so engine 0.1 does not take a store 0.3. The listing of the API is unchanged
  (`API-CHANGES-0.2.0.md`). `Store` is sealed
  since store 0.3.0: the store is a `PgStore`.
- A computed receipt's `asof` is the latest of its input asofs and the start of every name
  binding it relied on (the names read, transitively, through `Store::resolve_binding`). In
  0.1.0 a name bound after the inputs' asof made the receipt's asof the request's: a
  receipt per requested asof. Now one receipt answers from the binding's start (fib above
  its base cases: from when `fib` was bound). An input kept across a change of its own
  inputs still counts from where its value is known again.
- One query fewer per name for such receipts: a name and its binding's start come together.
- The core's test kit passes every check under its default answering rule `TxAndAsof`.
- Tests run on `PgStore` only (the in-memory and counting test stores could not implement
  the sealed trait): the 100,000-deep chain on PostgreSQL; statement counts on the wire.

## 0.1.0 (2026-10-24)

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
- Receipt identity is per computed version (cell, `tx`, `asof`); two valid receipts for the
  same value may exist with different lineage. After a backdated correction, the old receipt
  stays signature-valid but no longer recomputes (documented in the crate docs).
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
