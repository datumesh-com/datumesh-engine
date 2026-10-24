//! The [Datumesh](https://datumesh.com) engine: the value of a cell as of a time.
//!
//! [`Engine::get`] answers "the value of this cell as of A" with a receipt: for a stored
//! cell, the publisher's receipt from the store, unchanged; for a computed cell, a receipt the
//! engine signs with the mesh's key after evaluating the cell's function through a
//! [`Runtime`], from the receipts of the cells it reads. Answers are memoised; computed
//! versions are kept in the store (`put_computed`) and validated before they are reused. The
//! store is any [`datumesh_store::Store`] (in practice a `PgStore`); the API is synchronous
//! and [`Engine`] is `Send + Sync`.
//!
//! # Answering as of A
//!
//! A receipt answers as of A only when its `tx` and its `asof` are both at or before A. A
//! stored cell's answer is `stored_asof(cell, A)`. A computed cell's is its computed version
//! with the greatest `asof <= A` (`computed_asof`), once it is validated as of A; when none
//! validates, the function is evaluated as of A and, unless the value equals that version's
//! (which is then kept), a new receipt is signed and stored. No asof means now: the clock
//! reading taken once at the start of the request, which is also the `issued` of every
//! receipt the request signs.
//!
//! Function names resolve (`resolve_name`) as of A: the requested reference's name, and every
//! input name a function reads. [`Engine::get_cell`] takes a cell whose function hash is
//! already pinned (the core's [`CellRef`](datumesh_core::CellRef) carries no hash).
//!
//! # Computed receipts
//!
//! `computed_by` is `<mesh host>#<key id>`, `publisher` the cell's namespace, `inputs` the
//! hashes of the receipts read, `tx` the latest input `tx` (SPEC 8.2), `issued` the request's
//! clock reading, `valid` the cell's valid-time arguments, `licence` and `summary` from the
//! engine's configuration ([`EngineBuilder::licence`], [`EngineBuilder::summary`]): the
//! descriptor has neither, and adding them would change function hashes.
//!
//! `asof` is the latest input `asof` (SPEC 8.2 rule 2), with two refinements that keep every
//! receipt true as of its own `asof` (so that it recomputes there):
//!
//! * an input answered by a version that was kept or validated across a change of its own
//!   inputs counts from where its value was established again, not from its receipt's
//!   `asof`;
//! * if a function name the value depends on resolved differently at that instant than as
//!   of A (bound later, or rebound), the receipt's `asof` is A. The store does not say when a
//!   binding began, so no earlier instant is known.
//!
//! A function that reads nothing has `tx` = `asof` = `0001-01-01T00:00:00.000000Z`: its value
//! does not depend on time.
//!
//! # Memo, validation and early cutoff
//!
//! A version is valid as of A when every input receipt it lists, its name still resolving to
//! its function, gives as of A the same receipt or the same value (early cutoff). Validation
//! reads only the store, so it is always available, and correctness rests on it alone.
//!
//! The memo keeps each answer with the interval of asofs it holds for (a stored version from
//! its asof to the next version's; a computed answer the intersection of its inputs') and the
//! names it depends on, which are resolved again on every use. Adding a stored version must be
//! reported with [`Engine::stored_version_added`]: it drops the cell's answers and those of
//! every cell that read them (the dirty set, through the memo's own index of the cell-level
//! dependents of the answers it holds). Dropped answers are rechecked from memory before the
//! store is asked, so after a change that early cutoff stops, the unchanged part of a graph
//! costs no store query. Evaluations running across an invalidation cannot keep what they
//! read (revision marks). A process that writes versions without telling the engine leaves its
//! memo stale until [`Engine::clear_memo`]; [`EngineBuilder::memo_capacity`]`(0)` validates
//! every request through the store instead.
//!
//! Early-cutoff records need not be persisted: a version kept because its recomputed value is
//! the same stays the version in the store, so a fresh engine (after a restart) validates the
//! readers above it by receipt hash and recomputes only the cell whose inputs changed, as the
//! first engine did (`tests/early_cutoff.rs`).
//!
//! # Recursion and suspension
//!
//! A function may read its own name with other arguments (fib(n) reads fib(n - 1)).
//! Evaluation runs on an explicit stack of frames on the heap, never native recursion. When a
//! runtime reads a computed input not answered yet, [`Inputs::read`] returns
//! [`EvalError::Suspended`]; the engine pushes a frame for the input and, once it is answered,
//! runs the suspended function again from the start, its earlier reads now answered at once
//! from the request. A cell reappearing on the stack is [`EngineError::Cycle`]; more frames
//! than the depth limit (default 10,000) is [`EngineError::DepthExceeded`].
//!
//! The cost of the restart scheme: a function whose k computed inputs are all missing when it
//! starts runs up to k + 1 times and re-reads its earlier inputs each time (O(k^2) reads at
//! worst). A chain runs each link twice; fib(n) runs fib(2) three times and every other fib(k)
//! twice. Validating a stored version first makes its inputs available before the function
//! runs, so a recomputation after a change usually runs once.
//!
//! # Budget and single-flight
//!
//! Each request has a fuel ([`EngineBuilder::fuel`], default 10,000,000): runtime steps
//! ([`Budget::charge`]) plus input reads, validation's included. When it runs out the request
//! is [`EngineError::BudgetExceeded`] and nothing is stored: new receipts and versions are
//! written only after a request has succeeded.
//!
//! Evaluations run one at a time per engine, so concurrent requests for the same uncomputed
//! cell evaluate it once: the second finds the first's answer. Answers from the memo and of
//! stored cells do not wait.
//!
//! # For the HTTP layer
//!
//! | `Engine::get` result | Meaning | Envelope (SPEC 5.5), in spirit |
//! |---|---|---|
//! | `Ok(Answer::Receipt(bytes))` | the receipt answering as of A (`asof <= A`) | `ok`; `trust` `primary` when the host of `computed_by` is the host of `cell.ns` and `inputs` is empty, `verified` for a receipt the engine computed |
//! | `Ok(Answer::NotCovered)` | the name has no binding as of A, the function hash is unknown, a stored cell has no version answering as of A, or a computed cell read an input with none | `not_covered` |
//! | `Err(Invalid(e))` | a malformed request (asof, slice, arguments against the signature) | `bad_request`, `error` = `e.code()` |
//! | `Err(NameUnresolved { .. })` | an input's name has no binding as of A | `error`, `E_FN_UNAVAILABLE` |
//! | `Err(UndeclaredInput { .. })` | a function read a name it does not declare | `error`, `E_INPUT_UNDECLARED` |
//! | `Err(NoRuntime { .. })`, `Runtime`, `Cycle`, `DepthExceeded`, `BudgetExceeded` | the mesh could not evaluate the function | `error`, `E_EVAL_UNAVAILABLE` (or `stale` with an earlier receipt) |
//! | `Err(Sign(e))` | an input's `asof` is later than the clock (`E_TIME_ORDER`) | `error`, `E_TIME_ORDER` |
//! | `Err(Store(e))` | the store failed | `error`, `E_INPUT_UNAVAILABLE` (or `stale`) |
//!
//! [`Engine`] is also a time-aware [`datumesh_core::Resolver`], with the answering rule above:
//! `descriptor` and `receipt` from the store; `evaluate` looks a stored cell up and evaluates
//! a computed cell's function (never reading its own receipts back).
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use datumesh_core::{CellRef, SigningKey};
//! use datumesh_engine::{Answer, Engine};
//! use datumesh_store::{PgStore, Store};
//!
//! let store: Arc<dyn Store + Send + Sync> = Arc::new(PgStore::connect("postgres://localhost/datumesh").unwrap());
//! // The store must hold the mesh's key document: it verifies every receipt stored.
//! let engine = Engine::builder(store, "mesh.example.com").signing_key(SigningKey::from_seed(&[2; 32])).build().unwrap();
//! let cell = CellRef::from_json(br#"{"args":{},"fn":"total","ns":"shop.example.com"}"#).unwrap();
//! match engine.get(&cell, None).unwrap() {
//!     Answer::Receipt(bytes) => println!("{}", String::from_utf8_lossy(&bytes)),
//!     Answer::NotCovered => println!("not covered"),
//! }
//! ```
//!
//! Feature `native-test` adds `native_test::NativeTest`, a runtime of Rust closures keyed by
//! function hash, for tests.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod cache;
mod check;
mod clock;
mod engine;
mod error;
mod eval;
mod memo;
#[cfg(feature = "native-test")]
pub mod native_test;
mod runtime;
mod sign;

pub use clock::{Clock, SystemClock};
pub use engine::{
    Answer, Engine, EngineBuilder, MemoStats, SummaryFn, DEFAULT_DEPTH_LIMIT, DEFAULT_FUEL, DEFAULT_MEMO_CAPACITY,
};
pub use error::{ConfigError, EngineError, EvalError, StoreError};
pub use runtime::{Budget, Inputs, Runtime};
