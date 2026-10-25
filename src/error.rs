//! The engine's errors: [`EvalError`] (what a runtime and [`Inputs`](crate::Inputs) see),
//! [`EngineError`] (what [`Engine::get`](crate::Engine::get) returns), [`ConfigError`]
//! (building an engine) and [`StoreError`] (a store failure, in the engine's words).

use std::fmt;
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
    /// The cell reappeared on the evaluation stack: it depends on itself.
    Cycle {
        /// The cell's id.
        cell_id: String,
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
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::NoRuntime { lang } => write!(f, "no runtime for language {lang}"),
            EvalError::UndeclaredInput { fn_name } => write!(f, "{fn_name} is not a declared input"),
            EvalError::Cycle { cell_id } => write!(f, "cycle: cell {cell_id} depends on itself"),
            EvalError::DepthExceeded { limit } => write!(f, "evaluation deeper than the limit of {limit}"),
            EvalError::BudgetExceeded { fuel } => write!(f, "evaluation needs more than its fuel of {fuel}"),
            EvalError::NameUnresolved { fn_name, asof } => write!(f, "{fn_name} has no binding as of {asof}"),
            EvalError::Store(e) => e.fmt(f),
            EvalError::Runtime(m) => write!(f, "runtime: {m}"),
            EvalError::Suspended => f.write_str("evaluation suspended until an input is available"),
            EvalError::InputNotCovered { cell_id } => write!(f, "input cell {cell_id} has no value as of the asof"),
            EvalError::InvalidInput(e) => write!(f, "invalid input cell: {e}"),
            EvalError::Fault(c) => write!(f, "evaluation fault {c}"),
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
    /// A cell depends on itself.
    Cycle {
        /// The cell that reappeared on the evaluation stack.
        cell_id: String,
    },
    /// The evaluation stack reached the depth limit.
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
    /// The request is malformed: an asof that is not an instant, a slice, arguments that do
    /// not match the function's signature; or a runtime read such an input. The core's error
    /// carries the code (`E_CANON_INSTANT`, `E_SLICE`, `E_ARG_UNKNOWN`, ...). Added to the
    /// brief's list.
    Invalid(datumesh_core::Error),
    /// A computed receipt could not be signed: the core refused its members. In practice an
    /// input whose `asof` is later than the engine's clock (`E_TIME_ORDER`: `asof <= issued`).
    /// Added to the brief's list.
    Sign(datumesh_core::Error),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::NoRuntime { lang, fn_hash } => {
                write!(f, "no runtime for language {lang} (function {fn_hash})")
            }
            EngineError::UndeclaredInput { fn_name } => write!(f, "{fn_name} is not a declared input"),
            EngineError::Cycle { cell_id } => write!(f, "cycle: cell {cell_id} depends on itself"),
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
            EngineError::Invalid(e) => write!(f, "invalid request: {e}"),
            EngineError::Sign(e) => write!(f, "cannot sign the computed receipt: {e}"),
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
    fn from(e: StoreError) -> EngineError {
        EngineError::Store(e)
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
