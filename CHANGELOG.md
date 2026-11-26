# Changelog

All notable changes to `datumesh-engine`.

## 0.6.2

Guard patch, Part G (CONTRACT-guard-patch.md): member-set rows, a manifest policy, rebuilt manifests,
reuse of stored computed versions, and the fixes from the independent review. Requires datumesh-store
0.6.5 (migrations 17 / 13 before reads).

### Added

- `ManifestPolicy` (`Store`, `MemberSetOnly`, `StoreUpTo(usize)`, the default `Store`) and
  `EngineBuilder::manifests`: how a slice read's manifest is stored. A member-set row is written for
  every slice read, whatever the policy.
- `EngineBuilder::store_past_reads(bool)`: off, a read as of a past time stores only slice definitions.
- `Engine::manifest_asof(entry, asof)`: a manifest rebuilt from its slice as of `asof`, never stored.
  Over the fuel it is refused as the listing goes.
- `Engine::resolver_for(asof)` and `ReceiptResolver`: a per-receipt resolver serving a manifest as of
  the receipt's asof when none is held.
- `Engine::get_cell_current(cell, arrival)`: a current read as of a pinned arrival.

### Changed

- A stored version's memo entry starts at the later of its asof and its recorded time; its validity
  ends at the next version recorded after it (the store's `next_version_after`).
- Each request passes the store's barrier before its first store read.
- Reuse of a stored computed version's slice needs no member read while the slice's `seq` holds.

## 0.6.1

Local acceptance of restricted licences (CONTRACT-protocol-part1, E1; DECISIONS-round-protocol D4,
D4.1, D4.2). Nothing changes on a rooted host. On an unrooted host a refusal over restricted input
licences is now `EngineError::NotAccepted` where it was `Unlicensed` (the refusal itself is the same),
and the acceptance can compute it.

### Added

- `EngineBuilder::accepted_licences(impl IntoIterator<Item = String>)`: the restricted licences the
  operator has accepted. Replaces a set given before.
- `Engine::set_accepted_licences(&self, BTreeSet<String>)`: replaces the set for the next request,
  from any thread. It waits for a running evaluation, then drops the memo when the set changes, so
  a memoised answer is judged under the new set. A long-running `serve` or `mcp` picks up an
  acceptance made by another process this way.
- `pub const LOCAL_DERIVATION_LICENCE: &str = "LicenseRef-datumesh-local-derivation"`, re-exported
  at the crate root.
- `EngineError::NotAccepted { cell_id, licences }`: a refusal for a missing acceptance, with the
  restricted input licences not accepted, sorted and each once. Display names them.

### Rule

On an unrooted engine host (`*.invalid`) only, when SPEC 8.5 refuses with `not_declared`,
`declaration_not_open` or `restricted_conflict`, and every restricted input licence is accepted:
one distinct restricted licence gives that licence, two or more give `LOCAL_DERIVATION_LICENCE`.
Open inputs alongside do not change this; a share-alike conflict still refuses. The lineage keeps
every input receipt. A restricted licence not accepted gives `NotAccepted`; a rooted host keeps
`Unlicensed`. Applies to `get`, scoped evaluation (`whatif`) and memo reuse alike.

"Restricted" is anything outside SPEC 8.5's open tiers. Core keeps the tiers private, so the
engine carries its own list (`CC0-1.0`, `PDDL-1.0`, `MIT`, `Apache-2.0`, `CC-BY-4.0`, `OGL-UK-3.0`,
`CC-BY-SA-4.0`, `ODbL-1.0`, and `public:<https URL>`). A unit test checks that core gives each
open identifier, alone, a licence (not a refusal); a change to core's tiers needs this list to
follow.

### Notes

- A receipt signed under acceptance verifies at level `signed`, and recomputes at level
  `recomputed`, with the engine's own key.
- `EngineError` is non-exhaustive and gains a variant; `Unlicensed` keeps its shape. Code that
  matches `Unlicensed` on an unrooted host must also handle `NotAccepted`.

## 0.6.0

Withdrawals (CONTRACT-round-0.7 R1; SPEC 3.6, 8.1, 15.11-15.13 of spec-0.7.0): a stored cell whose
latest version as of the asof is a withdrawal (the null version, `Version.withdrawn`) has no value.
On `datumesh-store` 0.6.0 and `datumesh-core` 0.3.13.

### Added

- `Answer::Withdrawn { receipt, reason }`: `get` and `get_cell` of such a stored cell answer it.
  `receipt` is the withdrawal's canonical bytes (what `Resolver::receipt` gives for its hash),
  `reason` its `withdrawn` text, read with core's `Withdrawal` (never `Receipt::from_json`). The
  HTTP layer answers `not_covered` with the `withdrawn` member.
- Whatif members override (R3; SPEC 5.9): `Overrides::slice_members(slice, members)` replaces a
  stored slice's whole member set with exactly `members` (cells and values; possibly none), and
  `Overrides::slice_full(slice, members: Option<..>, add, remove)` is the general form.
  `members` with `add` or `remove` is `E_SLICE_MEMBER`, as is a member that is not a cell of the
  slice (by its function and arguments) or one given twice. `ScenarioOverride::Members { slice,
  members }` lists it, sorted by cell id and present even when empty. Nothing is written.
- A withdrawal that left a slice member out counts for time (SPEC 8.2, spec-cr 4196c5a): a
  computed receipt whose slice read left out a member because its latest version as of the
  asof is a withdrawal has the withdrawal's `tx` among its candidates for `tx`, and the
  withdrawal's `asof` among those for its `asof`. A read as of before that asof misses the
  memo and recomputes the earlier value (the slice's members, with the withdrawn cell in).
  Uses `Store::slice_withdrawn_latest` (store 0.6.0).

### Changed

- A formula's single-cell read of a withdrawn cell is `EvalError::InputNotCovered`, as for a cell
  with no receipt: the reader answers `NotCovered` (not `Withdrawn`: the withdrawal is the
  input's), `otherwise` does not catch it, and nothing is stored for the read.
- A slice leaves a withdrawn cell out of its members and its manifest (the store's
  `slice_values`); a formula's `at(...)` of it is `E_FORMULA_ABSENT`, which `otherwise` catches.
- Nothing is memoised for a withdrawn read. A withdrawal stored after a memoised answer
  supersedes it from its own asof (`stored_version_added` drops the cell's answers and every
  slice reader); a later valued version ends the withdrawal.
- `Resolver::evaluate` and `whatif` answer no value for a withdrawn cell.

### Dependencies

- `datumesh-store` 0.6.0 (was 0.5.0), `datumesh-core` 0.3.13 (was 0.3.8).

### Tests

- `tests/engine_060_withdrawal.rs`: the four signed withdrawal vectors (`tests/vectors/receipt/`)
  against valued siblings: the answer and its reason, supersession of a memoised answer, a later
  valued version, the single-cell read, `otherwise`, slices (count, sum, lineage) and `at`.

## 0.5.0

Build 2 Part 3 of the engine: scenarios (`whatif`), the solver, cycle paths, and deep recursion
that splits itself (Build 2 E1-E4). On `datumesh-store` 0.5.0 (was 0.4.1; the store adds only).
Breaking (`API-CHANGES-0.5.0.md`): `EngineError::Cycle` and `EvalError::Cycle` carry `path`
instead of `cell_id`.

### Added

- `Engine::whatif(cell, asof, &Overrides, Option<&Solve>) -> Result<ScenarioAnswer, EngineError>`
  (E1): the value of a cell as of an asof with value overrides and stored-slice overrides in
  place. The overridden cells answer their values; a cell that does not depend on the overrides
  answers from the memo and the store as `get` would; a dependent one is evaluated in the
  request's scope. Nothing is written: no receipt, computed version, global memo entry or
  signature. The answer is labelled a scenario (`ScenarioAnswer`, no receipt).
- `Overrides` (`value`, `slice`; at most 64, validated per SPEC 5.9), `ScenarioAnswer`,
  `ScenarioOverride` (`Value`, `Slice`, with `id()`).
- `Solve::new(vary, target, equals, lo, hi)` (E2): bisection of a number cell over `[lo, hi]`
  until `|target - equals|` or the interval is within 1e-12 relative. The value found is
  rounded half-even to 15 significant digits: solve values carry 15 significant digits. The
  bisection runs on binary doubles (the engine has no decimal type), and 15 digits are what a
  double holds deterministically. The answer is the requested cell's value under the solved
  `vary` (the target's, when the requested cell is the target).
  Fuel exhaustion or no sign change is `E_WHATIF_NO_SOLUTION`.
- `EngineError::Override(String)` (the code: `E_CANON_TYPE`, `E_CANON_ARG_DUPLICATE`,
  `E_SLICE_MEMBER`, `E_CANON_LIMIT`), `EngineError::NotCovered { fault }` (a scenario's cell
  with no value, or a fault in scope), `EngineError::WhatifNoSolution`.

### Changed

- A cycle names its path (E3): `EngineError::Cycle { path }`, the canonical calls in read order,
  from the cell that reappeared to its second occurrence (`<ns>/<fn>(<label>: <literal>, ...)`).
- Deep recursion splits itself (E4): an evaluation that would pass the depth limit (default
  10,000, unchanged) evaluates the deepest pending cell first as a root of its own, within the
  same request, and then runs the outer cell again, which finds the answers in the request. The
  answer and its receipts are those of a shallow evaluation. Fuel and work limits still bound it.
  `EngineError::DepthExceeded` is no longer raised by evaluation.

### Dependencies

- `datumesh-store` 0.5.0 (was 0.4.1). A local development `[patch]` of it is in the
  git-ignored `.cargo/config.toml`; the lock needs the registry release.

### Tests

- `tests/recursion.rs`: `a_cell_reappearing_on_the_stack_is_a_cycle` matches the path;
  `the_depth_limit_is_enforced` is `the_depth_limit_splits_a_deep_chain_with_the_same_answer`
  (success, the same receipt as the default limit); new `an_unbounded_recursion_is_stopped_by_its_fuel_and_work_limits`.
  Coordinator-approved, as the test writer noted.
- `tests/slices.rs`: the loop cycle asserts its path (`SUM(), LOOP(n: 2), SUM()`).
- `tests/whatif.rs` `a_scenario_writes_nothing`: the final assertion compared a warm engine with
  the cold snapshot (the test compared a warm engine with the cold snapshot); it now compares
  against the warm snapshot.
- `tests/solver.rs`: the rounding test pins 15 significant digits; new test for the requested
  cell's value under the solved `vary`.

## 0.4.2 (unreleased)

For spec-0.5.0: suspension (SPEC 9.9), faults memoised (SPEC 15.12, formula finding H9),
pinned reads by hash (SPEC 16.3 `super`, finding H8), methods as ordinary functions (SPEC
16.3, 16.7). On `datumesh-core` 0.3.8 (was 0.3.7) and `datumesh-store` 0.4.1 (was 0.4.0).
Additive (`API-CHANGES-0.4.2.md`): nothing removed or changed.

### Added

- `EngineError::NamespaceSuspended { host, suspended_at }` (a variant of a `#[non_exhaustive]`
  enum): a write the engine makes under a suspended namespace is refused by the store
  (`datumesh_store::Error::NamespaceSuspended`), and the engine says so. A request that must
  sign a receipt under the mesh's own suspended namespace fails with it before any write:
  nothing is signed or stored, and the cell's and its readers' answers are not memoised as
  answers. Reads, verification and the receipts already stored go on, served byte for byte;
  `Resolver::evaluate` gives no value where a new receipt would be needed. Other store errors
  stay `EngineError::Store`.
- `Inputs::read_pinned(fn_name, fn_hash, args)` (a provided method of the public `Inputs`
  trait, default `EvalError::Runtime`, so no implementor is affected): the cell of `fn_name`
  at the function hash `fn_hash`, `P/m@<fn_hash>(...)`, evaluated with the descriptor stored
  under that hash and relying on no binding of the name. Its receipt is an input of the reader
  as any read's is. `fn_name` must be declared by the reader's descriptor (the `super` of a
  method lists its parent method as an input). The engine's inputs override it.

### Changed

- **Faults are memoised** (SPEC 15.12 and D58: a fault is a property of the cell). A cell whose
  evaluation faulted is evaluated once while nothing it read has changed, for every fault code
  (`E_FORMULA_KEY` and `E_FORMULA_DISPATCH_KEY` included, as the others), and a reader of a
  faulted cell is answered from the memo. A memoised fault holds for the asofs an answer would
  hold for, and is dropped when a stored version it rests on is added. A fault that rests on an
  absent slice member or on an input with no value is not memoised. `MemoStats::entries` counts
  the faults held as well as the answers.
- The validation of a version's inputs checks a pinned input (`read_pinned`) by its hash alone,
  and records no name for it: a name that did not resolve to the hash at the version's asof was
  not read by name.

### Notes

- The suspension tests run on PostgreSQL only: the engine's test world has no SQLite backend.
  The `sqlite` feature adds no test of it.

## 0.4.1 (unreleased)

For spec-0.4.3: licence inheritance (SPEC 8.5). On `datumesh-core` 0.3.7 (was 0.3.5).
Additive (`API-CHANGES-0.4.1.md`).

### Added

- `EngineError::Unlicensed { cell_id, reason }` (a variant of a `#[non_exhaustive]` enum): the
  mesh may not compute the cell under licence inheritance (`share_alike_conflict`,
  `restricted_conflict`, `not_declared`, `declaration_not_open`, core's `LicenceReason`).
  Nothing is signed or stored for the request; a cell that reads a refused cell is refused
  too (`cell_id` names the refused cell). The HTTP layer answers `unlicensed` (403), with no
  new code.

### Changed

- **A computed receipt's `licence` is the result of SPEC 8.5** (core's `inherit_licence`), no
  longer the configured licence: over the licences of the input receipts it lists, one per
  receipt, every member of a slice entry included, at its `issued`. A restricted input
  licence needs a declaration for the function by its publisher, open at `issued`, read from
  the key document of the host of `cell.ns` in the store (`Store::namespace`; even when the
  mesh signing is another host). A host whose key document the store does not hold declares
  nothing (a restricted input is then `not_declared`). `EngineBuilder::licence` is the mesh's
  own licence: that of a receipt with no input receipt. Stored receipts are untouched.
- **Reuse.** A computed version (from the store or the memo) is reused only while its licence
  is allowed for the input licences read now: entitled at its own `issued` (as the
  entitlement check, SPEC 10.3, judges it: a declaration closed since does not matter), or
  the licence the rule gives now. Otherwise the function is evaluated again and the cell
  signed again with the rule's licence (a receipt signed before for exactly these inputs,
  with an allowed licence, is reused), or refused. This covers receipts signed before 0.4.1
  under the configured licence (they are signed again, or refused, on their next request;
  nothing needs migrating), and an input republished with the same value under another
  licence (early cutoff keeps a value, not a licence).
- Recomputation for a verifier (`Resolver::evaluation`) does not apply the rule to the cell it
  evaluates (SPEC 8.5: never in verification).

### Notes

- The `licence` vectors of spec-0.4.3 (50) run through the engine's own path
  (`tests/licence.rs`, copied into `tests/vectors/licence/`): all 50.
- Cost: the licences of a stored slice's members come from their receipts in the store
  (cached by hash), read when a receipt over the slice is signed or validated.
- Compatibility: a mesh that serves `declarations` is unverifiable by clients on core < 0.3.7
  or datumesh < 0.4.3; publishers should wait for their consumers to upgrade before declaring.
  The same holds for @datumesh/verify < 0.4.3.

## 0.4.0 (unreleased)

On `datumesh-store` 0.4.0 (was 0.3.2), so that an engine can run on its SQLite backend
(`SqliteStore`, feature `sqlite`) as well as on PostgreSQL. Nothing else changed: no code,
no behaviour (`API-CHANGES-0.4.0.md`).

### Changed

- `datumesh-store = "0.4.0"`. Breaking only because the public API names the store's sealed
  `Store` trait (`Engine::builder` takes an `Arc<dyn Store + Send + Sync>`) and its error
  (`StoreError::store_error`), which are now store 0.4's: users move to `datumesh-store` 0.4
  together with this release. Code written for 0.3.2 on store 0.3.2 builds unchanged on 0.4.0
  and store 0.4.0 (store 0.4.0 is additive for `PgStore` users).
- `datumesh-store` with `default-features = false`: the store's SQLite backend is not built
  unless asked for, through the engine's new feature `sqlite` (off by default; it enables
  `datumesh-store/sqlite`). `PgStore` is always built (the store has no feature for it), so
  PostgreSQL users need no feature, as in 0.3.

## 0.3.2 (2026-11-02)

For spec-0.4.2 (`text_of`, `E_NAME_UNBOUND`). On `datumesh-core` 0.3.5. Additive
(`API-CHANGES-0.3.2.md`).

### Added

- `Inputs::read_blob(sha256, length)` (provided: `EvalError::BlobUnavailable`): a blob's
  content. The engine reads it from the store and checks its length and SHA-256; content it
  cannot obtain or that does not match is `EvalError::BlobUnavailable`, which fails the
  request (`EngineError::BlobUnavailable`, an `error` with `E_EVAL_UNAVAILABLE`, never
  `not_covered`) whatever the runtime returns after it. No `inputs` entry; free (the runtime
  charges SPEC 15.12's `1 + bytes / 64`, knowing the length).
- `EvalError::BlobUnavailable`, `EngineError::BlobUnavailable`.
- `Engine::resolve(cell_ref, asof)`: the cell `get` would answer, or `None` when the name has
  no binding then (`E_NAME_UNBOUND`); `get` still answers `NotCovered` for it.

### Notes

- Recomputation through the engine as a resolver (`Resolver::evaluation`) runs the function
  through the engine, so it obtains blob content from the store as the evaluation did.
- Pinned requests (spec-0.4.2's `fn_hash=`) need nothing new: `get_cell` takes the pinned
  cell.

## 0.3.1 (2026-10-26)

The 1.3 follow-ups. Additive (`API-CHANGES-0.3.1.md`).

### Added

- `Inputs::binding_peek(fn_name, at)` (provided: `binding`): a binding looked up without
  being relied on: no asof candidate, no dependency of the answer on the name.
- `Inputs::asof_compare(t)` (provided: through `asof`): `t` against the evaluation asof
  without making the evaluation asof-dependent when the answer cannot change later: a `t` at
  or before the asof becomes a candidate for the receipt's `asof` (so every asof the receipt
  answers or is validated at gives the same answer, and it recomputes at its own); a `t`
  after it makes the evaluation asof-dependent.
- `FnBinding::new`, for a runtime's tests without a store.
- `examples/cutoff.rs`: the early-cutoff case of the benchmark, alone and repeated.

### Changed

- On `datumesh-store` 0.3.2: stored slices (and version slices) are read with
  `slice_values` (each member's value, receipt hash, `tx`, `asof` and cell from the store's
  columns and index: no receipt bytes) and `slice_changed_after` (whether the answer holds
  after A: one indexed query, where 0.3.0 read the slice again as of the end of time).

### Fixed

- A slice over a computed function evaluates its members in key order and stops at the first
  without a value (SPEC 15.12, 15.13): later members are neither evaluated nor charged (the
  vector `formula-eval/member-fault-caught` now has the spec's fuel, 7, through the engine;
  0.3.0 gave 16).
- A charge exceeding both the fuel and the work limit is `BudgetExceeded`, the normative
  error, not `WorkExceeded`.
- `E_FORMULA_FUTURE` is kept when an input has no value: a runtime's `Fault(Future)` stands
  over the absence when the engine saw a time after the asof (`read_asof`, `read_versions`,
  `asof_compare`) or the runtime read the asof; times are checked by `read_asof` and
  `read_versions` even after an absence. (The formula crate's engine fuzz, 100,000 bodies: 0
  such cases, 12 with 0.3.0.)
- The early-cutoff slowdown of 0.3.0: whether a function is asof-dependent is cached per
  function hash (a runtime may take long to say: the native test runtime hashed the
  descriptor on every frame, 15% of the case); answers, names and bindings are found without
  allocating keys; name maps are shared when nothing is added.

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
