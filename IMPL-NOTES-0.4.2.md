# Implementation notes: datumesh-engine 0.4.2 (spec-0.5.0)

For the implementer of the commit after the failing-tests commit. Contract:
`/root/datumesh-libs/CONTRACT-engine-0.4.2.md`. Nothing is removed or changed; the API
grows by one error variant and one provided trait method.

## Build

- `Cargo.toml` already names version 0.4.2, `datumesh-core` 0.3.8 and `datumesh-store` 0.4.1.
- Development patches both, uncommitted: `.cargo/config.toml` (excluded in `.git/info/exclude`)
  patches `datumesh-core` to `/core/crates/datumesh-core` (0d53fa8) and `datumesh-store` to
  `/store/crates/datumesh-store` (branch `step-1.5-namespace-validation`, db69efc); run cargo in
  `rust:1.99` on the `datumesh` network with those two checkouts mounted read-only at `/core` and
  `/store` (the committed `tools/cargo.sh` does not mount them: use a copy).
- `Cargo.lock` is not touched by this commit (fixed at release). Run
  `cargo test --all-features --no-fail-fast`; cargo stops after the first failing test binary
  without `--no-fail-fast`.

## The API sketch (stubs are in this commit)

1. `EngineError::NamespaceSuspended { host: String, suspended_at: String }` (`src/error.rs`).
   `EngineError` is `#[non_exhaustive]`, so a variant is additive. Display and `Clone`/`Debug` are
   done; the stub is never produced. To do: wherever the engine turns a
   `datumesh_store::Error::NamespaceSuspended` into an error, produce this variant instead of
   `EngineError::Store`. Today it comes out as `Store(StoreError { op: "put_receipt", .. })`,
   from `Req::flush` (`put_receipt`, `put_computed`, and `put_manifest` if the store refuses
   it) and from `store_frame` (`computed_asof`). Cover every store call that writes, and the
   `EvalError::Store` path (`engine_error`) in case a write is reached through it. Keep the
   `StoreError` conversion for every other store error. Nothing may be signed or stored: the
   tests compare the row counts of `receipts`, `computed_versions`, `versions`, `manifests`.
2. `Inputs::read_pinned(&mut self, fn_name: &str, fn_hash: &str, args: &Args) -> Result<Input, EvalError>`
   (`src/runtime.rs`), a **provided** method of the public `Inputs` trait, default
   `Err(EvalError::Runtime("these inputs offer no pinned reads"))`. It stays additive because a
   provided method adds no obligation to code that implements `Inputs` (runtimes' test doubles;
   `tests/pinned.rs` has one that does not override it), exactly as `binding_peek`, `read_blob`
   and `asof_compare` were added (0.3.1, 0.3.2). `Inputs` is not sealed; making it so would be
   a break, so the method must stay provided. The engine's `Reader` (`src/eval.rs`) overrides
   it; the stub there is `todo!()`.

## What read_pinned must do (item 5; tests/pinned.rs)

It is `read_at` with the function hash given instead of resolved:

- charge 1 and check `declared(fn_name)` (`UndeclaredInput` if `fn_name` is not in the
  descriptor's `inputs`), exactly as for any read; check the args as `read_at` does;
- take the descriptor from `descriptor_of(fn_hash)` (a hash with no stored descriptor is
  `EvalError::Store(NotFound { what: "descriptor", key: fn_hash })`, as in `function`);
- the cell is `Cell { ns, fn_name, fn_hash, args }` from `fn_name`'s namespace and name and
  the given hash; go through `lookup` as `read_at` does, with the evaluation's asof;
- **do not resolve the name and do not insert into `self.names`**, so no binding is relied on:
  no binding start is a candidate for the receipt's `asof` (the reader's and the pinned
  cell's `asof` are those of the stored versions read), and a later rebinding of the name
  neither invalidates nor recomputes the reader (tests: `a_pinned_read_relies_on_no_binding`,
  also across a cold engine). Check the validation path (`recheck_frame`, `check_use`,
  `names_hold`) does not demand a name for a read recorded as pinned; a pinned read is
  recorded like a cell read (`Rd::Cell`, `at` none).
- the receipt is an `inputs` entry of the reader; the fault is `EvalError::Fault` (catchable by
  the runtime, memoised as any fault); a pinned read of a stored function reads that hash's
  stored cells; `P/m@h1` and `P/m@h2` are distinct cells (the hash is in the cell id).
- a verifier recomputes through `Resolver::evaluate` on the cell with its `fn_hash`, which
  already works for a hash the name does not mean (`verify_all_issued` in the tests).

## Suspension (item 3; tests/suspension.rs)

Only the mesh's own namespace matters for the engine's writes (it signs as `MESH`). The tests
suspend it with `record_namespace_failure(MESH, at, 1)`. Required:

- a request that needs a receipt signed refuses with the typed error (host, `suspended_at` as
  the store gives them), also for a reader of such a cell, every time it is asked; it must not
  memoise an unsigned answer in a way that serves it later, and after
  `record_namespace_success` the same request signs (`reinstating_...`);
- reads go on: receipts already stored are served byte for byte by the same engine and by a
  cold one, stored cells and faults answer, `Resolver::evaluate` gives no value for what
  cannot be signed, verification at the recomputed level of what was signed passes. A cold
  engine finding an existing computed version must not try a write the store refuses (check
  that the idempotent `put_receipt` is allowed and that the existing version needs no
  `put_computed`; if a re-put is needed, it must not be attempted);
- another host's suspension (the publisher's, `RATES`) does not stop the mesh signing: this
  test passes already and guards against over-refusing.

The tests run on `PgStore` only (the `Store` trait is sealed and the engine's `World` has no
SQLite backend), so only that backend is tested; the `sqlite` feature adds none.

## Faults memoised (items 2 and 6; tests/fault_memo.rs)

`Fault::Key` and `Fault::DispatchKey` already pass through as `not_covered` with `fault`
(`the_key_and_dispatch_key_faults_are_not_covered_with_their_codes` passes today). What fails
is memoisation: today a faulted cell is evaluated again on every request (a native function's
call count grows; for `READER` over a faulting `INV` the counts were 3 and 4 after three and
four requests). Tests count the calls of the native function: once per cell at one asof for
every `Fault::ALL` code, once for a reader's faulting input, and invalidation follows the
inputs (after the stored input changes, a later asof is answered with the value, not the
memoised fault; the old asof still gives the fault).

Conflict to resolve, flagged: `tests/faults.rs` (old, must keep passing) asserts
`e.memo_stats().entries == 1` after faults ("only the stored input's answer") and its comment
says faults are not memoised as answers. Memoise faults so that this stays true (a separate
fault cache not counted in `entries`, or counted apart), or, if the coordinator prefers the
entries to count faults, change that one assertion in the implementation commit and say so.
The new tests do not look at `memo_stats`. The contract does not say how long a memoised
fault lives: the new tests only require once per unchanged world and correct answers after a
change, not precise invalidation (clearing on any stored version is enough).

## Methods (item 4; tests/methods.rs): regressions, pass today

- a class made by `define_class`, put and bound in the store (`put_class`, `bind_class`),
  with the method `rate` implemented natively under its hash: a cell of `N/m` is computed,
  signed by the mesh, memoised (one call), read by another function as its input, and
  follows its stored input. No engine change is expected.
- the asof-dependent default: a function reading `asof()` that reads
  `daily(day = day of asof)`; at two asofs two receipts (different hashes and inputs,
  `asof` equal to the requested asofs). "Two cells" is read as two receipts of the one
  function with different inputs; it passes today.

## Unclear

- `read_pinned` takes `fn_name` and `fn_hash` (the pinned cell's namespace and name come from
  `fn_name`, the descriptor from the hash); the contract writes `P/m@<fn_hash>(...)`. The name
  is checked against `inputs` as a read's is. If the formula team's `super` does not declare
  the parent method as an input, the declared check would need relaxing; the tests expect
  `UndeclaredInput` (`a_pinned_read_needs_a_stored_descriptor_and_a_declared_input`).
- Item 3's "both backends": see above, one backend exists in the engine's tests.
