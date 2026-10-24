//! One request's evaluation: an explicit stack of frames, never native recursion.
//!
//! A frame evaluates one computed cell. Its runtime reads inputs through [`Reader`]; a stored
//! input, or one already answered in this request, is answered at once. A computed input not
//! answered yet suspends the frame: `read` returns [`EvalError::Suspended`], the engine pushes
//! a frame for the input, and once that frame is done it runs the suspended frame's runtime
//! again from the start. Every cell answered is kept in the request, so a re-run reads it
//! at once. New receipts are written to the store only when the whole request has succeeded.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use datumesh_core::{lexical, Cell, Descriptor, Error as CoreError, ErrorCode, Receipt, ReceiptBuilder, Scalar};

use crate::cache::Parsed;
use crate::check::{check_args, valid_args};
use crate::engine::{Engine, STORED};
use crate::error::{EngineError, EvalError, StoreError};
use crate::runtime::{Budget, Inputs};
use crate::sign::receipt_json;

/// The earliest instant: the asof and tx of a computed receipt with no inputs.
pub(crate) const MIN_INSTANT: &str = "0001-01-01T00:00:00.000000Z";

/// An answer: the receipt that answers a cell, and over which asofs it is known to.
#[derive(Debug)]
pub(crate) struct Ans {
    /// The cell id.
    pub(crate) id: String,
    /// The receipt's hash, canonical bytes, value, tx and asof.
    pub(crate) hash: String,
    pub(crate) bytes: Arc<Vec<u8>>,
    pub(crate) value: Scalar,
    pub(crate) tx: String,
    pub(crate) asof: String,
    /// The answer holds for every asof in `[lo, hi)` (`hi` none: unbounded), given the store
    /// as it was read and the names in `names` resolving as recorded.
    pub(crate) lo: String,
    pub(crate) hi: Option<String>,
    /// Every function name the answer depends on (the cell's inputs, transitively), with the
    /// function hash it resolved to.
    pub(crate) names: Arc<BTreeMap<String, String>>,
    /// The cells read (direct inputs).
    pub(crate) deps: Vec<String>,
}

/// How a cell ended in a request.
#[derive(Clone, Debug)]
pub(crate) enum Done {
    Value(Arc<Ans>),
    /// No value as of the asof.
    NotCovered,
    /// The top cell evaluated without a receipt (`Resolver::evaluate`).
    Unsigned(Scalar),
}

/// What looking a cell up gives.
enum Lookup {
    Ready(Done),
    /// A computed cell not answered yet: it needs a frame.
    Eval(Arc<Descriptor>),
}

struct Fresh {
    cell_id: String,
    tx: String,
    hash: String,
    bytes: Arc<Vec<u8>>,
}

enum State {
    Eval,
}

struct Frame {
    cell: Cell,
    id: String,
    desc: Arc<Descriptor>,
    state: State,
    /// Evaluate without signing (the top cell of `Resolver::evaluate`).
    unsigned: bool,
}

/// One request: its asof, its clock reading, and what it has answered so far.
pub(crate) struct Req<'e> {
    eng: &'e Engine,
    asof: String,
    issued: String,
    names: HashMap<String, Option<String>>,
    done: HashMap<String, Done>,
    fresh: Vec<Fresh>,
    budget: Budget,
}

impl<'e> Req<'e> {
    pub(crate) fn new(eng: &'e Engine, asof: &str, issued: &str) -> Req<'e> {
        Req {
            eng,
            asof: asof.to_string(),
            issued: issued.to_string(),
            names: HashMap::new(),
            done: HashMap::new(),
            fresh: Vec::new(),
            budget: Budget::new(eng.fuel),
        }
    }

    /// The function hash `fn_name` resolves to as of the request's asof (once per request).
    fn resolve(&mut self, fn_name: &str) -> Result<Option<String>, EvalError> {
        if let Some(h) = self.names.get(fn_name) {
            return Ok(h.clone());
        }
        let h = self
            .eng
            .store
            .resolve_name(fn_name, &self.asof)
            .map_err(|e| EvalError::Store(StoreError::new("resolve_name", e)))?;
        self.names.insert(fn_name.to_string(), h.clone());
        Ok(h)
    }

    /// A cell's answer if it can be had without evaluating a function.
    fn lookup(&mut self, cell: &Cell, id: &str, desc: Arc<Descriptor>) -> Result<Lookup, EvalError> {
        if let Some(d) = self.done.get(id) {
            return Ok(Lookup::Ready(d.clone()));
        }
        if desc.lang == STORED {
            let d = self.stored(cell, id)?;
            self.done.insert(id.to_string(), d.clone());
            return Ok(Lookup::Ready(d));
        }
        Ok(Lookup::Eval(desc))
    }

    /// A stored cell's answer: the version answering as of the asof.
    pub(crate) fn stored(&mut self, _cell: &Cell, id: &str) -> Result<Done, EvalError> {
        let found = self
            .eng
            .store
            .stored_asof_with_receipt(id, &self.asof)
            .map_err(|e| EvalError::Store(StoreError::new("stored_asof", e)))?;
        let Some((v, bytes)) = found else { return Ok(Done::NotCovered) };
        let p = self.eng.parsed(&v.receipt_hash, &bytes).map_err(EvalError::Store)?;
        let r = &p.receipt;
        Ok(Done::Value(Arc::new(Ans {
            id: id.to_string(),
            hash: v.receipt_hash.clone(),
            bytes: p.bytes.clone(),
            value: r.value().clone(),
            tx: r.tx().to_string(),
            asof: r.asof().to_string(),
            lo: r.asof().to_string(),
            hi: None,
            names: Arc::new(BTreeMap::new()),
            deps: Vec::new(),
        })))
    }

    /// Answers `cell` (a computed cell with descriptor `desc`).
    pub(crate) fn run(&mut self, cell: Cell, desc: Arc<Descriptor>, unsigned: bool) -> Result<Done, EngineError> {
        let id = cell.id();
        let mut stack = vec![Frame { cell, id: id.clone(), desc, state: State::Eval, unsigned }];
        while let Some(top) = stack.last_mut() {
            let step = match top.state {
                State::Eval => self.eval_frame(top)?,
            };
            match step {
                Step::Done(d) => {
                    let f = stack.pop().expect("a frame");
                    self.done.insert(f.id, d);
                }
                Step::Push(cell, id, desc) => {
                    if stack.len() >= self.eng.depth_limit {
                        return Err(EngineError::DepthExceeded { limit: self.eng.depth_limit });
                    }
                    stack.push(Frame { cell, id, desc, state: State::Eval, unsigned: false });
                }
            }
        }
        self.done.get(&id).cloned().ok_or_else(|| EngineError::Runtime { cell_id: id, message: "no answer".into() })
    }

    /// Runs a frame's runtime once.
    fn eval_frame(&mut self, f: &mut Frame) -> Result<Step, EngineError> {
        let Some(rt) = self.eng.runtimes.get(&f.desc.lang).cloned() else {
            return Err(EngineError::NoRuntime { lang: f.desc.lang.clone(), fn_hash: f.cell.fn_hash.clone() });
        };
        let mut budget = self.budget.share();
        let mut reader = Reader {
            budget: self.budget.share(),
            req: self,
            desc: &f.desc,
            read: BTreeMap::new(),
            names: BTreeMap::new(),
            stop: None,
        };
        let result = rt.evaluate(&f.desc, &f.cell, &mut reader, &mut budget);
        let Reader { read, names, stop, .. } = reader;
        let fail = |e: EvalError| engine_error(e, &f.id);
        match stop {
            Some(Stop::Pending(cell, id, desc)) => return Ok(Step::Push(cell, id, desc)),
            Some(Stop::NotCovered) => return Ok(Step::Done(Done::NotCovered)),
            Some(Stop::Fatal(e)) => return Err(fail(e)),
            None => {}
        }
        let value = match result {
            Ok(v) => v,
            Err(EvalError::Suspended | EvalError::InputNotCovered { .. }) => {
                return Err(EngineError::Runtime {
                    cell_id: f.id.clone(),
                    message: "the runtime returned an input's error for an input it did not read".into(),
                })
            }
            Err(e) => return Err(fail(e)),
        };
        if value.ty() != f.desc.signature.returns.ty {
            return Err(EngineError::Runtime {
                cell_id: f.id.clone(),
                message: format!(
                    "returned a {} where the function declares {}",
                    value.ty().as_str(),
                    f.desc.signature.returns.ty.as_str()
                ),
            });
        }
        if f.unsigned {
            return Ok(Step::Done(Done::Unsigned(value)));
        }
        let ans = self.sign(f, value, read, names)?;
        Ok(Step::Done(Done::Value(ans)))
    }

    /// Signs a new receipt for frame `f`'s cell with `value`, read from `read`.
    fn sign(
        &mut self,
        f: &Frame,
        value: Scalar,
        read: BTreeMap<String, Arc<Ans>>,
        own_names: BTreeMap<String, String>,
    ) -> Result<Arc<Ans>, EngineError> {
        let tx = read.values().map(|a| a.tx.as_str()).max().unwrap_or(MIN_INSTANT).to_string();
        let asof = read.values().map(|a| a.asof.as_str()).max().unwrap_or(MIN_INSTANT).to_string();
        let lo = read.values().map(|a| a.lo.as_str()).max().unwrap_or(MIN_INSTANT).to_string();
        let hi = read.values().filter_map(|a| a.hi.clone()).min();
        let names = merge_names(read.values(), own_names);
        let summary = match &self.eng.summary {
            Some(s) => s(&f.cell),
            None => format!("{} computed by {}.", f.cell.function_name(), self.eng.host),
        };
        let receipt = ReceiptBuilder::new()
            .cell(f.cell.clone())
            .value(value.clone())
            .valid(valid_args(&f.cell, &f.desc))
            .asof(asof.clone())
            .tx(tx.clone())
            .issued(self.issued.clone())
            .publisher(f.cell.ns.clone())
            .signer_host(self.eng.host.clone())
            .inputs(read.keys().cloned())
            .licence(self.eng.licence.clone())
            .summary(summary)
            .sign(&self.eng.key)
            .map_err(EngineError::Sign)?;
        let bytes = Arc::new(receipt_json(&receipt));
        let hash = receipt.hash().to_string();
        self.fresh.push(Fresh { cell_id: f.id.clone(), tx: tx.clone(), hash: hash.clone(), bytes: bytes.clone() });
        Ok(Arc::new(Ans {
            id: f.id.clone(),
            hash,
            bytes,
            value,
            tx,
            asof,
            lo,
            hi,
            names,
            deps: read.values().map(|a| a.id.clone()).collect(),
        }))
    }

    /// Writes the request's new receipts and computed versions: receipts first (inputs before
    /// the receipts reading them), then the versions.
    pub(crate) fn flush(&mut self) -> Result<(), EngineError> {
        for f in &self.fresh {
            self.eng.store.put_receipt(&f.bytes).map_err(|e| StoreError::new("put_receipt", e))?;
        }
        for f in &self.fresh {
            match self.eng.store.put_computed(&f.cell_id, &f.tx, &f.hash) {
                Ok(()) => {}
                // Another receipt holds this (cell, tx, asof): the answer stands, as a receipt in
                // the store that is not a computed version.
                Err(datumesh_store::Error::Conflict { .. }) => {
                    tracing::warn!(target: "datumesh_engine", cell_id = %f.cell_id, receipt = %f.hash,
                        "another computed receipt is recorded for this cell at the same tx and asof; kept that one");
                }
                Err(e) => return Err(StoreError::new("put_computed", e).into()),
            }
        }
        self.fresh.clear();
        Ok(())
    }
}

/// What a frame's step leads to.
enum Step {
    Done(Done),
    Push(Cell, String, Arc<Descriptor>),
}

/// Why a frame's runtime cannot finish this run, recorded at the first read that says so; it
/// wins over whatever the runtime returns.
enum Stop {
    /// An input to evaluate first.
    Pending(Cell, String, Arc<Descriptor>),
    /// An input with no value: neither has this cell.
    NotCovered,
    Fatal(EvalError),
}

/// The [`Inputs`] of one run of a frame's runtime.
struct Reader<'r, 'e> {
    req: &'r mut Req<'e>,
    desc: &'r Descriptor,
    budget: Budget,
    /// The answers read, by receipt hash.
    read: BTreeMap<String, Arc<Ans>>,
    /// The input names resolved, with their hashes.
    names: BTreeMap<String, String>,
    stop: Option<Stop>,
}

impl Reader<'_, '_> {
    fn read_input(&mut self, fn_name: &str, args: &datumesh_core::Args) -> Result<Scalar, EvalError> {
        self.budget.charge_shared(1)?;
        if !self.desc.inputs.iter().any(|i| i == fn_name) {
            return Err(EvalError::UndeclaredInput { fn_name: fn_name.to_string() });
        }
        let Some(fn_hash) = self.req.resolve(fn_name)? else {
            return Err(EvalError::NameUnresolved { fn_name: fn_name.to_string(), asof: self.req.asof.clone() });
        };
        let Some((ns, f)) = fn_name.rsplit_once('/') else {
            return Err(EvalError::UndeclaredInput { fn_name: fn_name.to_string() });
        };
        let cell = Cell { ns: ns.to_string(), fn_name: f.to_string(), fn_hash: fn_hash.clone(), args: args.clone() };
        if let Some((k, _)) = cell.args.iter().find(|(_, a)| a.scalar().is_none()) {
            return Err(EvalError::InvalidInput(CoreError::new(
                ErrorCode::Slice,
                format!("$.args.{k}"),
                "an input is a cell, not a slice",
            )));
        }
        let desc = match self.req.eng.descriptor_of(&fn_hash) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return Err(EvalError::Store(StoreError::new(
                    "descriptor",
                    datumesh_store::Error::NotFound { what: "descriptor", key: fn_hash },
                )))
            }
            Err(EngineError::Store(e)) => return Err(EvalError::Store(e)),
            Err(EngineError::Invalid(e)) => return Err(EvalError::InvalidInput(e)),
            Err(e) => return Err(EvalError::Runtime(e.to_string())),
        };
        check_args(&cell, &desc).map_err(EvalError::InvalidInput)?;
        self.names.insert(fn_name.to_string(), fn_hash);
        let id = cell.id();
        match self.req.lookup(&cell, &id, desc)? {
            Lookup::Ready(Done::Value(a)) => {
                let v = a.value.clone();
                self.read.insert(a.hash.clone(), a);
                Ok(v)
            }
            Lookup::Ready(Done::NotCovered | Done::Unsigned(_)) => {
                self.stop = Some(Stop::NotCovered);
                Err(EvalError::InputNotCovered { cell_id: id })
            }
            Lookup::Eval(desc) => {
                self.stop = Some(Stop::Pending(cell, id, desc));
                Err(EvalError::Suspended)
            }
        }
    }
}

impl Inputs for Reader<'_, '_> {
    fn read(&mut self, fn_name: &str, args: &datumesh_core::Args) -> Result<Scalar, EvalError> {
        match &self.stop {
            Some(Stop::Pending(..)) => return Err(EvalError::Suspended),
            Some(Stop::NotCovered) => return Err(EvalError::InputNotCovered { cell_id: String::new() }),
            Some(Stop::Fatal(e)) => return Err(e.clone()),
            None => {}
        }
        let r = self.read_input(fn_name, args);
        if let Err(e) = &r {
            if !matches!(e, EvalError::Suspended | EvalError::InputNotCovered { .. }) {
                self.stop = Some(Stop::Fatal(e.clone()));
            }
        }
        r
    }
}

/// The names of `answers` and `own`, sharing an `Arc` when nothing is added.
fn merge_names<'a>(answers: impl Iterator<Item = &'a Arc<Ans>>, own: BTreeMap<String, String>) -> Arc<BTreeMap<String, String>> {
    let mut acc: Option<Arc<BTreeMap<String, String>>> = None;
    for a in answers {
        match &mut acc {
            None => acc = Some(a.names.clone()),
            Some(m) => {
                if a.names.iter().any(|(k, v)| m.get(k) != Some(v)) {
                    let m = Arc::make_mut(m);
                    for (k, v) in a.names.iter() {
                        m.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }
    let mut acc = acc.unwrap_or_default();
    if own.iter().any(|(k, v)| acc.get(k) != Some(v)) {
        Arc::make_mut(&mut acc).extend(own);
    }
    acc
}

/// An evaluation error as the request's error, for the cell being evaluated.
fn engine_error(e: EvalError, cell_id: &str) -> EngineError {
    match e {
        EvalError::NoRuntime { lang } => EngineError::NoRuntime { lang, fn_hash: String::new() },
        EvalError::UndeclaredInput { fn_name } => EngineError::UndeclaredInput { fn_name },
        EvalError::Cycle { cell_id } => EngineError::Cycle { cell_id },
        EvalError::DepthExceeded { limit } => EngineError::DepthExceeded { limit },
        EvalError::BudgetExceeded { fuel } => EngineError::BudgetExceeded { fuel },
        EvalError::NameUnresolved { fn_name, asof } => EngineError::NameUnresolved { fn_name, asof },
        EvalError::Store(e) => EngineError::Store(e),
        EvalError::InvalidInput(e) => EngineError::Invalid(e),
        other => EngineError::Runtime { cell_id: cell_id.to_string(), message: other.to_string() },
    }
}

/// Parses a receipt the store returned (its canonical bytes).
pub(crate) fn parse_stored(hash: &str, bytes: &[u8]) -> Result<Parsed, StoreError> {
    match Receipt::from_json(bytes) {
        Ok(r) if r.hash() == hash => Ok(Parsed { bytes: Arc::new(bytes.to_vec()), receipt: r }),
        Ok(_) | Err(_) => Err(StoreError::new(
            "receipt",
            datumesh_store::Error::Invalid(CoreError::new(
                ErrorCode::InputInvalid,
                "$",
                format!("the store's receipt {hash} does not parse to that hash"),
            )),
        )),
    }
}

/// Whether `s` is an instant (for the asof of `Resolver::evaluate`).
pub(crate) fn is_instant(s: &str) -> bool {
    lexical::check_instant(s, "$").is_ok()
}
