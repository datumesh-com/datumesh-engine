//! The engine's errors: [`EvalError`] (what a runtime and [`Inputs`](crate::Inputs) see),
//! [`EngineError`] (what [`Engine::get`](crate::Engine::get) returns), [`ConfigError`]
//! (building an engine) and [`StoreError`] (a store failure, in the engine's words).

use std::fmt;

use datumesh_core::Fault;
use std::sync::Arc;

/// A store failure, as the engine reports it.
///
/// It wraps the store's error (its [`source`](std::error::Error::source)), with the engine's
/// own wording: the store's `Conflict` speaks of signatures, which is not what a conflict
/// means to the engine (another receipt is already recorded for a version).
#[derive(Clone, Debug)]
pub struct StoreError {
    op: &'static str,
    inner: Arc<datumesh_store::Error>,
}

impl StoreError {
    pub(crate) fn new(op: &'static str, e: datumesh_store::Error) -> StoreError {
        StoreError { op, inner: Arc::new(e) }
    }

    /// The store operation that failed (`stored_asof`, `put_receipt`, ...).
    pub fn operation(&self) -> &str {
        self.op
    }

    /// The store's error.
    pub fn store_error(&self) -> &datumesh_store::Error {
        &self.inner
    }

    /// Whether the store refused a write because another receipt is recorded under the same
    /// key.
    pub fn is_conflict(&self) -> bool {
        matches!(*self.inner, datumesh_store::Error::Conflict { .. })
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.inner {
            datumesh_store::Error::Conflict { hash } => write!(
                f,
                "store {}: another receipt is already recorded where receipt {hash} was to go; the recorded one is kept",
                self.op
            ),
            other => write!(f, "store {}: {other}", self.op),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.inner)
    }
}

/// What evaluating a function can fail with: returned by [`Inputs::read`](crate::Inputs::read)
/// and [`Budget::charge`](crate::Budget::charge), and by a [`Runtime`](crate::Runtime).
///
/// A runtime propagates every error it gets from `Inputs::read` or `Budget::charge`
/// unchanged (with `?`). The engine does not depend on it: it records each such error when it
/// happens, and what the runtime returns after it is ignored.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum EvalError {
    /// No runtime is configured for the function's `code.lang`.
    NoRuntime {
        /// The language.
        lang: String,
    },
    /// The function read a function its descriptor does not declare in `inputs`.
    UndeclaredInput {
        /// The fully qualified name read.
        fn_name: String,
    },
    /// The cell reappeared on the evaluation stack: it depends on itself. The path holds the
    /// canonical calls in read order, from the cell that reappeared to its second occurrence.
    Cycle {
        /// The canonical calls of the cycle (SPEC 5.8), the first and last equal.
        path: Vec<String>,
    },
    /// The evaluation stack reached the engine's depth limit.
    DepthExceeded {
        /// The limit.
        limit: usize,
    },
    /// The request's fuel (runtime steps plus input reads) ran out.
    BudgetExceeded {
        /// The fuel the request had.
        fuel: u64,
    },
    /// A function name has no binding as of the evaluation's asof.
    NameUnresolved {
        /// The fully qualified name.
        fn_name: String,
        /// The asof.
        asof: String,
    },
    /// The store failed.
    Store(StoreError),
    /// The runtime failed: its own message.
    Runtime(String),
    /// The input read is not available yet: the engine evaluates it, then runs the reading
    /// function again from the start (see [`Inputs`](crate::Inputs)). Added to the brief's
    /// list: it is how evaluation suspends without native recursion.
    Suspended,
    /// The input has no value as of the evaluation's asof (a stored cell with no version
    /// answering then, or a computed cell one of whose inputs has none). The reading cell has
    /// no value either ([`Answer::NotCovered`](crate::Answer::NotCovered)), even if the
    /// runtime catches it: a receipt lists the inputs read, and cannot list an absence, so a
    /// value computed from one could be neither checked against its inputs nor recomputed.
    /// Added to the brief's list.
    InputNotCovered {
        /// The input cell's id.
        cell_id: String,
    },
    /// The arguments given to `Inputs::read` do not make a cell of the input function (a
    /// slice, an unknown or missing argument, a type that differs from the signature). Added to
    /// the brief's list: it is the runtime's mistake, not the engine's or the store's.
    InvalidInput(datumesh_core::Error),
    /// A formula evaluation fault (SPEC 15.11, 15.12): the cell has no value, deterministically
    /// (`E_FORMULA_DIV_ZERO`, `E_FORMULA_DOMAIN`, `E_FORMULA_RANGE`, `E_FORMULA_EMPTY`,
    /// `E_FORMULA_ABSENT`, `E_FORMULA_FUTURE`). Returned by a runtime, it makes the answer
    /// [`Answer::NotCovered`](crate::Answer::NotCovered) with the fault, and nothing is
    /// stored for the cell. Returned by a read (an input that faulted, a computed slice with a
    /// member without a value, a transaction time after the asof), the runtime may catch it
    /// where its language allows. Since 0.3.0.
    Fault(datumesh_core::Fault),
    /// A blob's content could not be obtained ([`Inputs::read_blob`](crate::Inputs::read_blob)):
    /// the store has none for its SHA-256, or what it has is not the blob's (another length or
    /// hash). The evaluator's failure, not a fault of the cell (SPEC 15.12,
    /// `E_FORMULA_BLOB_UNAVAILABLE`): the request fails with
    /// [`EngineError::BlobUnavailable`] whatever the runtime returns after it. Since 0.3.2.
    BlobUnavailable {
        /// The blob's SHA-256.
        sha256: String,
        /// Why: no content, or content of another length or hash.
        reason: String,
    },
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::NoRuntime { lang } => write!(f, "no runtime for language {lang}"),
            EvalError::UndeclaredInput { fn_name } => write!(f, "{fn_name} is not a declared input"),
            EvalError::Cycle { path } => write!(f, "cycle: {}", path.join(" -> ")),
            EvalError::DepthExceeded { limit } => write!(f, "evaluation deeper than the limit of {limit}"),
            EvalError::BudgetExceeded { fuel } => write!(f, "evaluation needs more than its fuel of {fuel}"),
            EvalError::NameUnresolved { fn_name, asof } => write!(f, "{fn_name} has no binding as of {asof}"),
            EvalError::Store(e) => e.fmt(f),
            EvalError::Runtime(m) => write!(f, "runtime: {m}"),
            EvalError::Suspended => f.write_str("evaluation suspended until an input is available"),
            EvalError::InputNotCovered { cell_id } => write!(f, "input cell {cell_id} has no value as of the asof"),
            EvalError::InvalidInput(e) => write!(f, "invalid input cell: {e}"),
            EvalError::Fault(c) => write!(f, "evaluation fault {c}"),
            EvalError::BlobUnavailable { sha256, reason } => {
                write!(f, "the content of blob {sha256} is unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for EvalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EvalError::Store(e) => Some(e),
            EvalError::InvalidInput(e) => Some(e),
            _ => None,
        }
    }
}

/// What [`Engine::get`](crate::Engine::get) can fail with. For how each maps to a response
/// envelope, see the crate documentation.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum EngineError {
    /// No runtime is configured for a function's `code.lang`.
    NoRuntime {
        /// The language.
        lang: String,
        /// The function's hash.
        fn_hash: String,
    },
    /// A function read a function its descriptor does not declare in `inputs`.
    UndeclaredInput {
        /// The fully qualified name read.
        fn_name: String,
    },
    /// A cell depends on itself. `path` holds the canonical calls of the cycle in read order,
    /// starting at the cell that reappeared and ending with it again (Build 2 E3: since 0.5.0).
    Cycle {
        /// The canonical calls of the cycle (SPEC 5.8).
        path: Vec<String>,
    },
    /// The evaluation stack reached the depth limit. Since 0.5.0 a deeper evaluation splits
    /// itself instead (Build 2 E4), so this is not raised by the engine's evaluation.
    DepthExceeded {
        /// The limit.
        limit: usize,
    },
    /// The request's fuel ran out: its normative fuel (SPEC 15.12) exceeds the limit. Nothing
    /// was stored for the request.
    BudgetExceeded {
        /// The fuel the request had.
        fuel: u64,
    },
    /// The request's work ran out: every evaluation attempt (those a restart discarded
    /// included) and the engine's validation reads, which bound its total cost beyond the
    /// normative fuel ([`EngineBuilder::work_limit`](crate::EngineBuilder::work_limit)).
    /// Nothing was stored for the request. Since 0.3.0.
    WorkExceeded {
        /// The work limit.
        limit: u64,
    },
    /// An input's function name has no binding as of the request's asof.
    NameUnresolved {
        /// The fully qualified name.
        fn_name: String,
        /// The asof.
        asof: String,
    },
    /// The store failed.
    Store(StoreError),
    /// A runtime failed, or broke its contract (returned a value of another type than the
    /// function declares, or `Suspended` without a pending input).
    Runtime {
        /// The cell being evaluated.
        cell_id: String,
        /// What happened.
        message: String,
    },
    /// A function read a blob whose content the engine could not obtain or check (SPEC 15.7,
    /// 15.12: `E_FORMULA_BLOB_UNAVAILABLE`, the evaluator's failure). Nothing was stored for
    /// the request. Since 0.3.2.
    BlobUnavailable {
        /// The blob's SHA-256.
        sha256: String,
        /// Why: no content, or content of another length or hash.
        reason: String,
    },
    /// The request is malformed: an asof that is not an instant, a slice, arguments that do
    /// not match the function's signature; or a runtime read such an input. The core's error
    /// carries the code (`E_CANON_INSTANT`, `E_SLICE`, `E_ARG_UNKNOWN`, ...). Added to the
    /// brief's list.
    Invalid(datumesh_core::Error),
    /// The mesh may not compute the cell under licence inheritance (SPEC 8.5, spec-0.4.3): its
    /// input licences have no licence in common (`share_alike_conflict`,
    /// `restricted_conflict`), or a restricted input licence is not declared for the function
    /// by its publisher, open at the receipt's `issued` (`not_declared`,
    /// `declaration_not_open`). Nothing was signed or stored for the request. `cell_id` is
    /// the refused cell: the requested one, or a cell it reads (a reader of a refused cell is
    /// refused too). The HTTP layer answers `unlicensed` (403, no new code). Since 0.4.1.
    Unlicensed {
        /// The cell the rule refused.
        cell_id: String,
        /// Why (never one of the reasons that give a licence).
        reason: datumesh_core::LicenceReason,
    },
    /// On an unrooted host, licence inheritance would compute the cell over restricted input
    /// licences, but some of them are not in the engine's accepted set (engine 0.6.1, E1; see
    /// [`EngineBuilder::accepted_licences`](crate::EngineBuilder::accepted_licences)). Nothing
    /// was signed or stored. `licences` are the restricted input licences not accepted, sorted,
    /// each once. Only where core's refusal would be overridden by acceptance (`not_declared`,
    /// `declaration_not_open`, `restricted_conflict`); a rooted host gives `Unlicensed` as
    /// before. Since 0.6.1.
    NotAccepted {
        /// The cell the rule refused: as for `Unlicensed`.
        cell_id: String,
        /// The restricted licences not accepted.
        licences: Vec<String>,
    },
    /// A write the engine makes is under a suspended namespace (SPEC 9.9, store 0.4.1): the
    /// store refused it with [`datumesh_store::Error::NamespaceSuspended`], as the mesh's own
    /// namespace (the signer of its computed receipts) is suspended. Nothing was signed or
    /// stored for the request; reads, verification and the receipts already stored go on.
    /// Since 0.4.2.
    NamespaceSuspended {
        /// The suspended host.
        host: String,
        /// When it was suspended (an instant).
        suspended_at: String,
    },
    /// The store did not answer within its time bound: a read waited behind a writer (the barrier
    /// of a request, or a read of a store that has a timeout). Nothing was signed or stored for the
    /// request; the mesh answers 503 `E_INPUT_UNAVAILABLE` (CONTRACT-guard-patch, second review B2).
    /// Since 0.6.2.
    InputUnavailable {
        /// The store operation that timed out.
        what: String,
    },
    /// A computed receipt could not be signed: the core refused its members. In practice an
    /// input whose `asof` is later than the engine's clock (`E_TIME_ORDER`: `asof <= issued`).
    /// Added to the brief's list.
    Sign(datumesh_core::Error),
    /// A scenario's overrides are not valid (SPEC 5.9, Build 2 E1): the code is `E_CANON_TYPE`
    /// (a value of another type than the cell's), `E_CANON_ARG_DUPLICATE` (a cell or slice
    /// overridden twice), `E_SLICE_MEMBER` (a removed cell that is not in the slice, a cell
    /// both added and removed, an added cell outside the slice or already in it) or
    /// `E_CANON_LIMIT` (more than 64 overrides). Since 0.5.0.
    Override(String),
    /// A scenario's cell has no value, or a fault in its scope (`not_covered`): the fault if
    /// any, as [`Answer::NotCovered`](crate::Answer::NotCovered). Since 0.5.0.
    NotCovered {
        /// The evaluation fault, if the cell has no value because of one.
        fault: Option<Fault>,
    },
    /// A solve found no solution (`E_WHATIF_NO_SOLUTION`): no sign change over the interval,
    /// or the request's fuel ran out before the bisection converged (never a partial answer).
    /// Since 0.5.0.
    WhatifNoSolution,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::NoRuntime { lang, fn_hash } => {
                write!(f, "no runtime for language {lang} (function {fn_hash})")
            }
            EngineError::UndeclaredInput { fn_name } => write!(f, "{fn_name} is not a declared input"),
            EngineError::Cycle { path } => write!(f, "cycle: {}", path.join(" -> ")),
            EngineError::DepthExceeded { limit } => write!(f, "evaluation deeper than the limit of {limit}"),
            EngineError::BudgetExceeded { fuel } => {
                write!(f, "evaluation needs more than its fuel of {fuel}; nothing was stored")
            }
            EngineError::WorkExceeded { limit } => {
                write!(f, "evaluation needs more work than its limit of {limit}; nothing was stored")
            }
            EngineError::NameUnresolved { fn_name, asof } => write!(f, "{fn_name} has no binding as of {asof}"),
            EngineError::Store(e) => e.fmt(f),
            EngineError::Runtime { cell_id, message } => write!(f, "runtime, evaluating cell {cell_id}: {message}"),
            EngineError::BlobUnavailable { sha256, reason } => {
                write!(f, "the content of blob {sha256} is unavailable: {reason}; nothing was stored")
            }
            EngineError::Unlicensed { cell_id, reason } => {
                write!(f, "licence inheritance refuses to compute cell {cell_id}: {reason}; nothing was stored")
            }
            EngineError::NotAccepted { cell_id, licences } => write!(
                f,
                "licence inheritance refuses to compute cell {cell_id}: not accepted: {}; nothing was stored",
                licences.join(", ")
            ),
            EngineError::NamespaceSuspended { host, suspended_at } => {
                write!(f, "the namespace {host} is suspended since {suspended_at}; nothing was signed or stored")
            }
            EngineError::InputUnavailable { what } => {
                write!(f, "the store did not answer in time ({what}); nothing was signed or stored")
            }
            EngineError::Invalid(e) => write!(f, "invalid request: {e}"),
            EngineError::Sign(e) => write!(f, "cannot sign the computed receipt: {e}"),
            EngineError::Override(code) => write!(f, "scenario override refused: {code}; nothing was computed"),
            EngineError::NotCovered { fault: Some(fault) } => write!(f, "not covered in the scenario: {fault}"),
            EngineError::NotCovered { fault: None } => f.write_str("not covered in the scenario"),
            EngineError::WhatifNoSolution => {
                f.write_str("E_WHATIF_NO_SOLUTION: no solution in the interval, or the fuel ran out")
            }
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EngineError::Store(e) => Some(e),
            EngineError::Invalid(e) | EngineError::Sign(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for EngineError {
    /// A store's refusal of a write under a suspended namespace (SPEC 9.9) is
    /// [`EngineError::NamespaceSuspended`]; any other store failure is [`EngineError::Store`].
    fn from(e: StoreError) -> EngineError {
        match &*e.inner {
            datumesh_store::Error::NamespaceSuspended { host, suspended_at } => {
                EngineError::NamespaceSuspended { host: host.clone(), suspended_at: suspended_at.clone() }
            }
            datumesh_store::Error::Timeout(_) => EngineError::InputUnavailable { what: e.op.to_string() },
            _ => EngineError::Store(e),
        }
    }
}

/// Why an engine could not be built ([`EngineBuilder::build`](crate::EngineBuilder::build)).
#[non_exhaustive]
#[derive(Debug)]
pub enum ConfigError {
    /// The mesh host is not a DNS name.
    Host(datumesh_core::Error),
    /// No signing key was given.
    NoSigningKey,
    /// The signing key's seed file could not be read, or does not hold a seed (32 bytes, or
    /// 64 hex digits with an optional final newline).
    KeyFile(String),
    /// The licence for computed receipts is not an SPDX identifier or an https URL.
    Licence(datumesh_core::Error),
    /// The store holds no key document for the mesh host, or the document has no key with the
    /// signing key's id and public key: the store would refuse every computed receipt.
    KeyNotPublished {
        /// The mesh host.
        host: String,
        /// The signing key's id.
        key_id: String,
    },
    /// Reading the mesh host's key document from the store failed.
    Store(StoreError),
    /// Two runtimes for one language, or a runtime for `stored`, which the engine answers
    /// itself.
    Runtime(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Host(e) => write!(f, "mesh host: {e}"),
            ConfigError::NoSigningKey => f.write_str("no signing key"),
            ConfigError::KeyFile(m) => write!(f, "signing key file: {m}"),
            ConfigError::Licence(e) => write!(f, "licence: {e}"),
            ConfigError::KeyNotPublished { host, key_id } => {
                write!(f, "the store holds no key document of {host} publishing key {key_id}")
            }
            ConfigError::Store(e) => e.fmt(f),
            ConfigError::Runtime(m) => write!(f, "runtimes: {m}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Host(e) | ConfigError::Licence(e) => Some(e),
            ConfigError::Store(e) => Some(e),
            _ => None,
        }
    }
}
