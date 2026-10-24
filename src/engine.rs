//! [`Engine`], [`EngineBuilder`] and [`Answer`].

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use datumesh_core::{lexical, Cell, CellRef, Descriptor, KeyDoc, Resolver, Scalar, SigningKey};
use datumesh_store::Store;

use crate::cache::{Cache, Parsed};
use crate::check::{check_args, check_no_slice};
use crate::clock::{Clock, SystemClock};
use crate::error::{ConfigError, EngineError, StoreError};
use crate::memo::Memo;
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
        lexical::check_dns_name(&self.host, "$.host").map_err(ConfigError::Host)?;
        let key = match (self.key, &self.key_file) {
            (Some(k), _) => k,
            (None, Some(path)) => read_seed(path)?,
            (None, None) => return Err(ConfigError::NoSigningKey),
        };
        lexical::check_licence(&self.licence, "$.licence").map_err(ConfigError::Licence)?;
        let mut runtimes: HashMap<String, Arc<dyn Runtime>> = HashMap::new();
        for rt in self.runtimes {
            let lang = rt.lang().to_string();
            if lang == STORED {
                return Err(ConfigError::Runtime("stored is answered by the engine, not a runtime".into()));
            }
            if runtimes.insert(lang.clone(), rt).is_some() {
                return Err(ConfigError::Runtime(format!("two runtimes for {lang}")));
            }
        }
        let published = match self.store.namespace(&self.host) {
            Ok(Some(ns)) => KeyDoc::from_json(&self.host, &ns.keydoc)
                .ok()
                .and_then(|doc| doc.key(key.key_id()).map(|k| k.public == key.public_key()))
                .unwrap_or(false),
            Ok(None) => false,
            Err(e) => return Err(ConfigError::Store(StoreError::new("namespace", e))),
        };
        if !published {
            return Err(ConfigError::KeyNotPublished { host: self.host, key_id: key.key_id().to_string() });
        }
        Ok(Engine {
            store: self.store,
            host: self.host,
            key,
            runtimes,
            depth_limit: self.depth_limit,
            fuel: self.fuel,
            licence: self.licence,
            summary: self.summary,
            clock: self.clock,
            memo: Memo::new(self.memo_capacity),
            descriptors: Cache::new(DESCRIPTOR_CACHE),
            receipts: Cache::new(RECEIPT_CACHE),
            evaluator: Mutex::new(()),
        })
    }
}

impl fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineBuilder").field("host", &self.host).finish_non_exhaustive()
    }
}

/// The engine: answers the value of a cell as of a time. See the crate documentation.
pub struct Engine {
    pub(crate) store: Arc<dyn Store + Send + Sync>,
    pub(crate) host: String,
    pub(crate) key: SigningKey,
    pub(crate) runtimes: HashMap<String, Arc<dyn Runtime>>,
    pub(crate) depth_limit: usize,
    pub(crate) fuel: u64,
    pub(crate) licence: String,
    pub(crate) summary: Option<Arc<SummaryFn>>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) memo: Memo,
    /// Parsed descriptors by function hash (immutable: content-addressed).
    pub(crate) descriptors: Cache<Arc<Descriptor>>,
    /// Parsed receipts by hash (immutable: content-addressed).
    pub(crate) receipts: Cache<Arc<Parsed>>,
    /// Evaluations run one at a time (single-flight); answers from the memo do not wait.
    pub(crate) evaluator: Mutex<()>,
}

/// The language of stored functions, answered by the engine itself.
pub(crate) const STORED: &str = "stored";
const DESCRIPTOR_CACHE: usize = 10_000;
const RECEIPT_CACHE: usize = 100_000;

/// The seed in a key file: 32 bytes, or 64 lowercase hex digits and an optional newline.
fn read_seed(path: &std::path::Path) -> Result<SigningKey, ConfigError> {
    let bytes = std::fs::read(path).map_err(|e| ConfigError::KeyFile(format!("{}: {e}", path.display())))?;
    if let Ok(seed) = <[u8; 32]>::try_from(bytes.as_slice()) {
        return Ok(SigningKey::from_seed(&seed));
    }
    let text = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let hex = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    if text.len() == 64 {
        let mut seed = [0u8; 32];
        let mut ok = true;
        for (i, pair) in text.chunks(2).enumerate() {
            match (hex(pair[0]), hex(pair[1])) {
                (Some(h), Some(l)) => seed[i] = h << 4 | l,
                _ => ok = false,
            }
        }
        if ok {
            return Ok(SigningKey::from_seed(&seed));
        }
    }
    Err(ConfigError::KeyFile(format!("{}: not a seed (32 bytes, or 64 lowercase hex digits)", path.display())))
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
    ///
    /// The function name resolves as of the asof; with no binding then, the answer is
    /// [`Answer::NotCovered`]. See the crate documentation for the answers and errors.
    pub fn get(&self, cell: &CellRef, asof: Option<&str>) -> Result<Answer, EngineError> {
        let now = self.clock.now();
        let a = asof.or(cell.asof.as_deref()).unwrap_or(&now).to_string();
        lexical::check_instant(&a, "$.asof").map_err(EngineError::Invalid)?;
        check_no_slice(&cell.args)?;
        let fn_name = format!("{}/{}", cell.ns, cell.fn_name);
        let Some(fn_hash) = self.store.resolve_name(&fn_name, &a).map_err(|e| StoreError::new("resolve_name", e))?
        else {
            return Ok(Answer::NotCovered);
        };
        let resolved = cell.resolve(&fn_hash).map_err(EngineError::Invalid)?;
        self.answer(&resolved, &a, &now)
    }

    /// The value of `cell` (its function pinned by `fn_hash`), as of `asof` (else now). The
    /// form for a reference that already carries the function hash. An unknown function
    /// hash is [`Answer::NotCovered`].
    pub fn get_cell(&self, cell: &Cell, asof: Option<&str>) -> Result<Answer, EngineError> {
        let now = self.clock.now();
        let a = asof.unwrap_or(&now).to_string();
        lexical::check_instant(&a, "$.asof").map_err(EngineError::Invalid)?;
        check_no_slice(&cell.args)?;
        self.answer(cell, &a, &now)
    }

    /// The answer for a resolved cell as of `a`; `now` is the request's clock reading.
    fn answer(&self, cell: &Cell, a: &str, now: &str) -> Result<Answer, EngineError> {
        let Some(desc) = self.descriptor_of(&cell.fn_hash)? else {
            return Ok(Answer::NotCovered);
        };
        check_args(cell, &desc).map_err(EngineError::Invalid)?;
        let _ = now;
        if desc.lang == STORED {
            return match self.store.stored_asof_with_receipt(&cell.id(), a).map_err(|e| StoreError::new("stored_asof", e))? {
                Some((_, bytes)) => Ok(Answer::Receipt(bytes.into_owned())),
                None => Ok(Answer::NotCovered),
            };
        }
        Err(EngineError::NoRuntime { lang: desc.lang.clone(), fn_hash: cell.fn_hash.clone() })
    }

    /// The parsed descriptor of `fn_hash`, cached.
    pub(crate) fn descriptor_of(&self, fn_hash: &str) -> Result<Option<Arc<Descriptor>>, EngineError> {
        if let Some(d) = self.descriptors.get(fn_hash) {
            return Ok(Some(d));
        }
        let Some(bytes) = self.store.descriptor(fn_hash) else { return Ok(None) };
        let d = Arc::new(Descriptor::from_json(&bytes).map_err(EngineError::Invalid)?);
        self.descriptors.insert(fn_hash.to_string(), d.clone());
        Ok(Some(d))
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
    fn descriptor(&self, fn_hash: &str) -> Option<Cow<'_, [u8]>> {
        self.store.descriptor(fn_hash)
    }

    fn receipt(&self, hash: &str) -> Option<Cow<'_, [u8]>> {
        self.store.receipt(hash)
    }

    fn evaluate(&self, _cell: &Cell, _asof: &str) -> Option<Scalar> {
        None
    }
}
