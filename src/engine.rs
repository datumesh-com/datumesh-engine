//! [`Engine`], [`EngineBuilder`] and [`Answer`].

use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use datumesh_core::{Cell, CellRef, Resolver, Scalar, SigningKey};
use datumesh_store::Store;

use crate::clock::{Clock, SystemClock};
use crate::error::{ConfigError, EngineError};
use crate::runtime::Runtime;

/// The default depth limit: frames on the evaluation stack.
pub const DEFAULT_DEPTH_LIMIT: usize = 10_000;
/// The default fuel of a request: runtime steps plus input reads.
pub const DEFAULT_FUEL: u64 = 10_000_000;
/// The default memo capacity, in entries.
pub const DEFAULT_MEMO_CAPACITY: usize = 1_000_000;

/// The answer to "the value of this cell as of A".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The receipt that answers, canonical JSON text: for a stored cell the publisher's
    /// receipt as the store holds it; for a computed cell one signed by the engine.
    Receipt(Vec<u8>),
    /// Nothing answers as of A: the function is unknown then, a stored cell has no version
    /// answering then, or a computed cell reads such an input (and its runtime did not handle
    /// it).
    NotCovered,
}

/// The function computing a computed receipt's `summary`.
pub type SummaryFn = dyn Fn(&Cell) -> String + Send + Sync;

/// Builds an [`Engine`]: [`Engine::builder`].
pub struct EngineBuilder {
    pub(crate) store: Arc<dyn Store + Send + Sync>,
    pub(crate) host: String,
    pub(crate) key: Option<SigningKey>,
    pub(crate) key_file: Option<PathBuf>,
    pub(crate) runtimes: Vec<Arc<dyn Runtime>>,
    pub(crate) depth_limit: usize,
    pub(crate) fuel: u64,
    pub(crate) memo_capacity: usize,
    pub(crate) licence: String,
    pub(crate) summary: Option<Arc<SummaryFn>>,
    pub(crate) clock: Arc<dyn Clock>,
}

impl EngineBuilder {
    /// The mesh's signing key.
    pub fn signing_key(mut self, key: SigningKey) -> Self {
        self.key = Some(key);
        self.key_file = None;
        self
    }

    /// The mesh's signing key, from a file holding its 32-byte seed: the 32 bytes, or 64 hex
    /// digits (an optional final newline). The only file the engine reads.
    pub fn signing_key_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.key_file = Some(path.into());
        self.key = None;
        self
    }

    /// Adds a runtime, for the language its [`Runtime::lang`] names.
    pub fn runtime(mut self, runtime: Arc<dyn Runtime>) -> Self {
        self.runtimes.push(runtime);
        self
    }

    /// The depth limit: frames on the evaluation stack (default 10,000).
    pub fn depth_limit(mut self, limit: usize) -> Self {
        self.depth_limit = limit;
        self
    }

    /// The fuel of each request: runtime steps plus input reads (default 10,000,000).
    pub fn fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    /// How many answers the memo keeps (default 1,000,000); when full it starts afresh. 0
    /// turns the memo off: every request then validates through the store.
    pub fn memo_capacity(mut self, entries: usize) -> Self {
        self.memo_capacity = entries;
        self
    }

    /// The `licence` of computed receipts: an SPDX identifier or an https URL (default
    /// `CC-BY-4.0`).
    pub fn licence(mut self, licence: impl Into<String>) -> Self {
        self.licence = licence.into();
        self
    }

    /// The `summary` of a computed receipt, from its cell. The default says which function
    /// the mesh computed: `"<namespace>/<function> computed by <mesh host>."`.
    pub fn summary(mut self, f: impl Fn(&Cell) -> String + Send + Sync + 'static) -> Self {
        self.summary = Some(Arc::new(f));
        self
    }

    /// The clock (default [`SystemClock`]).
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Checks the configuration and builds the engine.
    pub fn build(self) -> Result<Engine, ConfigError> {
        Err(ConfigError::NoSigningKey)
    }
}

impl fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineBuilder").field("host", &self.host).finish_non_exhaustive()
    }
}

/// The engine: answers the value of a cell as of a time. See the crate documentation.
pub struct Engine {
    _private: (),
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

/// Sizes of the memo and of the dirty set ([`Engine::memo_stats`]).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoStats {
    /// Answers held.
    pub entries: usize,
    /// Cells with at least one answer held.
    pub cells: usize,
    /// Edges of the in-memory dependents index (input cell to reading cell).
    pub dependent_edges: usize,
    /// Cells marked dirty and kept to refuse answers of evaluations already running.
    pub dirty: usize,
    /// An estimate of the bytes these take.
    pub approx_bytes: usize,
}

impl Engine {
    /// A builder for an engine on `store`, signing as the mesh `host`.
    pub fn builder(store: Arc<dyn Store + Send + Sync>, host: impl Into<String>) -> EngineBuilder {
        EngineBuilder {
            store,
            host: host.into(),
            key: None,
            key_file: None,
            runtimes: Vec::new(),
            depth_limit: DEFAULT_DEPTH_LIMIT,
            fuel: DEFAULT_FUEL,
            memo_capacity: DEFAULT_MEMO_CAPACITY,
            licence: "CC-BY-4.0".into(),
            summary: None,
            clock: Arc::new(SystemClock),
        }
    }

    /// The value of the cell `cell` denotes, as of `asof` (else `cell.asof`, else now).
    pub fn get(&self, cell: &CellRef, asof: Option<&str>) -> Result<Answer, EngineError> {
        let _ = (cell, asof);
        Ok(Answer::NotCovered)
    }

    /// The value of `cell` (its function pinned by `fn_hash`), as of `asof` (else now).
    pub fn get_cell(&self, cell: &Cell, asof: Option<&str>) -> Result<Answer, EngineError> {
        let _ = (cell, asof);
        Ok(Answer::NotCovered)
    }

    /// Tells the engine that a stored version of `cell_id` was added: the memoised answers
    /// that depend on the cell are dropped.
    pub fn stored_version_added(&self, cell_id: &str) {
        let _ = cell_id;
    }

    /// Drops every memoised answer.
    pub fn clear_memo(&self) {}

    /// Sizes of the memo and of the dirty set.
    pub fn memo_stats(&self) -> MemoStats {
        MemoStats::default()
    }
}

impl Resolver for Engine {
    fn descriptor(&self, _fn_hash: &str) -> Option<Cow<'_, [u8]>> {
        None
    }

    fn receipt(&self, _hash: &str) -> Option<Cow<'_, [u8]>> {
        None
    }

    fn evaluate(&self, _cell: &Cell, _asof: &str) -> Option<Scalar> {
        None
    }
}
