//! One request's evaluation: an explicit stack of frames, never native recursion.
//!
//! A frame evaluates one computed cell as of one asof (the request's, or the transaction time
//! `T` of an `asof(c, T)` read). Its runtime reads inputs through [`Reader`]; a stored input,
//! or one already answered in this request, is answered at once. A computed input not
//! answered yet suspends the frame: the read returns [`EvalError::Suspended`], the engine
//! pushes a frame for the input (for a slice over a computed function, for each missing
//! member in turn), and once they are done it runs the suspended frame's runtime again from
//! the start. Every cell answered is kept in the request, so a re-run reads it at once. New
//! receipts, manifests and versions are written to the store only when the whole request has
//! succeeded.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use datumesh_core::{
    inherit_licence, is_unrooted, lexical, Arg, Args, Cell, Declarations, Error as CoreError, ErrorCode, Fault,
    FunctionDescriptor, KeyDocument, LicenceReason, Manifest, Receipt, ReceiptBuilder, Scalar, Slice, Withdrawal,
};

use crate::acceptance::{decide, is_open, overridable, Rule, Verdict};
use crate::cache::Parsed;
use crate::check::{check_args, valid_args};
use crate::engine::{Engine, ManifestPolicy, STORED};
use crate::error::{EngineError, EvalError, StoreError};
use crate::memo::Entry;
use crate::runtime::{Budget, FnBinding, Input, Inputs, Member, SlicePattern};
use crate::sign::receipt_json;
use crate::slice::{cells_slice, extent_cells, spec_of, versions_slice, Spec};
use crate::whatif::{scoped_hash, Scope};

/// The earliest instant: the asof and tx of a computed receipt with no inputs.
pub(crate) const MIN_INSTANT: &str = "0001-01-01T00:00:00.000000Z";
/// The latest instant.
pub(crate) const MAX_INSTANT: &str = "9999-12-31T23:59:59.999999Z";

/// A string just after the instant `a` and before every later instant (instants have a fixed
/// width, so `a` followed by any character is): the exclusive end of an interval holding at
/// `a` only.
pub(crate) fn just_after(a: &str) -> String {
    format!("{a}~")
}

/// The function names an answer relies on: (name, the transaction time it was resolved at,
/// none for the answer's own asof) to the function hash it resolved to.
pub(crate) type Names = BTreeMap<(String, Option<String>), String>;

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
    /// The receipt's licence (an input's, for licence inheritance: SPEC 8.5).
    pub(crate) licence: String,
    /// The answer holds for every asof in `[lo, hi)` (`hi` none: unbounded), given the store
    /// as it was read and the names in `names` resolving as recorded.
    pub(crate) lo: String,
    pub(crate) hi: Option<String>,
    /// Every function name the answer depends on (the cell's inputs, transitively).
    pub(crate) names: Arc<Names>,
    /// The cell.
    pub(crate) cell: Cell,
    /// The inputs this answer rests on, with the receipt and value each gave (for a version
    /// kept or validated across a change, the inputs as of then, not its receipt's).
    pub(crate) inputs: Vec<Use>,
    /// Whether the memo may keep it: not when its value rests on a fault a runtime caught
    /// (which no receipt lists), or on such an answer.
    pub(crate) memo: bool,
}

/// An input an answer rests on.
#[derive(Clone, Debug)]
pub(crate) enum Use {
    /// A cell, read as of the answer's asof, or as of the transaction time `at`.
    Cell { id: String, cell: Cell, hash: String, value: Scalar, at: Option<String> },
    /// A slice.
    Slice(Arc<SliceRead>),
}

/// The ids of the cells read whose changes reach an answer through the dependents index:
/// single cells and the members of slices over computed functions. (A slice over a stored
/// function is matched by its pattern instead: [`input_patterns`].)
fn input_deps(inputs: &[Use]) -> impl Iterator<Item = &String> {
    inputs.iter().flat_map(|u| -> Box<dyn Iterator<Item = &String> + '_> {
        match u {
            Use::Cell { id, .. } => Box::new(std::iter::once(id)),
            Use::Slice(s) if s.computed => Box::new(s.members.iter().map(|m| &m.id)),
            Use::Slice(_) => Box::new(std::iter::empty()),
        }
    })
}

/// The slices over stored functions (and version slices) read.
fn input_patterns(inputs: &[Use]) -> impl Iterator<Item = &Arc<Slice>> {
    inputs.iter().filter_map(|u| match u {
        Use::Slice(s) if !s.computed => Some(&s.slice),
        _ => None,
    })
}

impl Ans {
    /// See [`input_deps`].
    pub(crate) fn deps(&self) -> impl Iterator<Item = &String> {
        input_deps(&self.inputs)
    }

    /// See [`input_patterns`].
    pub(crate) fn patterns(&self) -> impl Iterator<Item = &Arc<Slice>> {
        input_patterns(&self.inputs)
    }
}

/// A cell with no value because its evaluation faulted (SPEC 15.12). A fault is memoised as an
/// answer is: it holds for the asofs `[lo, hi)`, and is dropped when a stored version it rests
/// on is added. It has no receipt, so it is never stored.
#[derive(Debug)]
pub(crate) struct FaultAns {
    /// The cell id.
    pub(crate) id: String,
    pub(crate) fault: Fault,
    /// The instant the fault holds from: the latest of what the evaluation read and relied
    /// on (as of the asof, for an asof-dependent fault).
    pub(crate) known: String,
    /// The fault holds for every asof in `[lo, hi)`, given the store as it was read and the
    /// names in `names` resolving as recorded.
    pub(crate) lo: String,
    pub(crate) hi: Option<String>,
    /// Every function name the fault depends on, transitively.
    pub(crate) names: Arc<Names>,
    /// The inputs that gave a value, as an answer's are.
    pub(crate) inputs: Vec<Use>,
    /// The inputs that faulted (their cell ids; their own dependencies are theirs).
    pub(crate) faulted: Vec<String>,
    /// Whether the memo may keep it.
    pub(crate) memo: bool,
}

impl FaultAns {
    /// The cell ids whose changes reach this fault through the dependents index (see
    /// [`Ans::deps`]).
    pub(crate) fn deps(&self) -> impl Iterator<Item = &String> {
        self.faulted.iter().chain(input_deps(&self.inputs))
    }

    /// The slices over stored functions it read (see [`Ans::patterns`]).
    pub(crate) fn patterns(&self) -> impl Iterator<Item = &Arc<Slice>> {
        input_patterns(&self.inputs)
    }
}

/// A slice read as of an asof: its entry and manifest, and its members.
#[derive(Debug)]
pub(crate) struct SliceRead {
    pub(crate) entry: String,
    pub(crate) manifest: Arc<Vec<u8>>,
    /// The manifest object the bytes are of (for the member-set row); none for a slice read from
    /// a stored row (G3), which is never written again.
    pub(crate) set: Option<Manifest>,
    /// The slice's seq read before its members (G3, decided 26 Nov). 0 when a version of the slice
    /// was recorded after the read's asof: a changed slice has a version, so its seq is at least 1,
    /// and a row of 0 is never reused (`reuse_entry`). Stores refuse a negative seq.
    pub(crate) seen_seq: i64,
    /// The members' licences from the stored row (G3), when the read is from it: the members are
    /// not read, so these stand for them.
    pub(crate) row_licences: Option<Vec<String>>,
    /// The as-of the read was made at (the member-set row's `asof`).
    pub(crate) asof: String,
    pub(crate) slice: Arc<Slice>,
    pub(crate) fn_name: String,
    pub(crate) fn_hash: String,
    /// Over a computed function (members evaluated by the engine).
    pub(crate) computed: bool,
    /// The members in key order (ascending tx for a version slice).
    pub(crate) members: Vec<MemberRec>,
    /// The members' answers, for a slice over a computed function.
    member_ans: Vec<Arc<Ans>>,
    /// Where the read holds from (the latest member asof, and `to`), and until.
    lo: Option<String>,
    hi: Option<String>,
    /// The latest member tx (by recorded time, CONTRACT-guard-patch G9).
    tx: Option<String>,
    memo: bool,
}

/// A member of a slice read.
#[derive(Clone, Debug)]
pub(crate) struct MemberRec {
    pub(crate) id: String,
    pub(crate) hash: String,
    pub(crate) value: Scalar,
    args: Args,
    tx: String,
    asof: String,
    /// When the store recorded the member's version (G9): its answer is not before it.
    recorded: String,
    /// A member a scenario changed: its hash is a scenario's, not a receipt's.
    scoped: bool,
}

impl MemberRec {
    fn input(&self) -> Input {
        Input::new(self.value.clone(), self.hash.clone(), self.tx.clone(), self.asof.clone())
    }
}

/// How a cell ended in a request.
#[derive(Clone, Debug)]
pub(crate) enum Done {
    Value(Arc<Ans>),
    /// No value as of the asof.
    NotCovered,
    /// No value: the evaluation faulted (SPEC 15.12), as a memoised answer: the fault and the
    /// instant it holds from (the latest asof of what the evaluation read, and of the bindings
    /// it relied on).
    Fault(Arc<FaultAns>),
    /// The top cell evaluated without a receipt (`Resolver::evaluation`), with the entries
    /// it read.
    Unsigned(Scalar, Vec<String>),
    /// A stored cell withdrawn as of the asof (8.1): no value, with the withdrawal.
    Withdrawn(Arc<Withdrawn>),
}

/// A withdrawal that answers a stored cell (store 0.6.0): its canonical bytes and reason.
#[derive(Debug)]
pub(crate) struct Withdrawn {
    pub(crate) bytes: Arc<Vec<u8>>,
    pub(crate) reason: String,
}

/// What looking a cell up gives.
enum Lookup {
    Ready(Done),
    /// A computed cell not answered yet: it needs a frame.
    Eval(Arc<FunctionDescriptor>),
}

/// A cell to evaluate as of an asof.
#[derive(Clone, Debug)]
pub(crate) struct Pend {
    cell: Cell,
    id: String,
    desc: Arc<FunctionDescriptor>,
    asof: String,
}

/// What a runtime read: a cell (as of the asof, or of `at`), or a slice.
#[derive(Clone, Debug)]
enum Rd {
    Cell { ans: Arc<Ans>, at: Option<String> },
    Slice(Arc<SliceRead>),
}

impl Rd {
    fn entry(&self) -> &str {
        match self {
            Rd::Cell { ans, .. } => &ans.hash,
            Rd::Slice(s) => &s.entry,
        }
    }
}

struct Fresh {
    cell_id: String,
    tx: String,
    hash: String,
    bytes: Arc<Vec<u8>>,
    /// Kept as a computed version (validated by later requests).
    computed: bool,
}

enum State {
    /// Not looked at yet: an answer the memo dropped (a stored version it rested on changed)
    /// is rechecked first; else the store's computed version answering as of the asof.
    Start,
    /// Rechecking a dropped answer: inputs before `idx` gave the same receipt or value.
    Recheck { ans: Arc<Ans>, idx: usize, read: Vec<Rd>, names: Names },
    /// The store's computed version answering as of the asof, to validate; else evaluate.
    FromStore,
    /// Validating that version: inputs before `idx` gave the same receipt or value.
    Validate { cand: Arc<Parsed>, idx: usize, read: Vec<Rd>, names: Names },
    /// Evaluating the function; `cand` is the version that did not validate, if any.
    Eval { cand: Option<Arc<Parsed>> },
}

struct Frame {
    cell: Cell,
    id: String,
    asof: String,
    desc: Arc<FunctionDescriptor>,
    state: State,
    /// Evaluate without signing (the top cell of `Resolver::evaluation`).
    unsigned: bool,
    /// Cells to evaluate before the frame steps again, last first.
    pending: Vec<Pend>,
    /// The fuel of the frame's last suspended attempt: its next one charges at least as much.
    floor: u64,
}

/// One request: its asof, its clock reading, and what it has answered so far.
pub(crate) struct Req<'e> {
    eng: &'e Engine,
    asof: String,
    issued: String,
    /// Names resolved, by instant then name: the function hash and the start of the binding.
    names: HashMap<String, HashMap<String, Option<(String, String)>>>,
    /// Cells answered, by asof then cell id.
    done: HashMap<String, HashMap<String, Done>>,
    /// Slices read, by (asof, slice id).
    slices: HashMap<(String, String), Arc<SliceRead>>,
    fresh: Vec<Fresh>,
    /// The slices the request read fresh, by entry: their manifests and member-set rows are
    /// written by [`Req::flush`] under the engine's manifest policy.
    slice_reads: BTreeMap<String, Arc<SliceRead>>,
    /// The declarations of each publisher host (SPEC 9.2), read from the store once per
    /// request: `None` when the store has no key document for the host, or one without.
    declarations: HashMap<String, Option<Arc<Declarations>>>,
    /// The answers this request made (not taken from the memo), inputs first: for the memo.
    pub(crate) learned: Vec<Entry>,
    budget: Budget,
    /// The overrides of a scenario ([`Engine::whatif`](crate::Engine::whatif)): the request
    /// evaluates in its scope, and writes nothing.
    scope: Option<Arc<Scope>>,
    /// The function names each function name reaches through declared inputs, itself
    /// included, as of the request's asof (see [`Req::dependent`]).
    closures: HashMap<String, Arc<HashSet<String>>>,
    /// A current read (CONTRACT-guard-patch G10): stores as one, whatever its asof.
    current: bool,
}

/// How an input of a version compares as of the asof.
enum Check {
    /// The same: the read, the name it relied on (none for a pinned read) and its function hash.
    Same(Rd, Option<(String, Option<String>)>, String),
    Pending(Vec<Pend>),
    Differs,
}

/// What reading a slice as of an asof gives.
enum SliceOutcome {
    Ready(Arc<SliceRead>),
    Pending(Vec<Pend>),
    /// A member over a computed function has no value (since the instant given).
    Absent(String),
}

/// What a frame's step leads to.
enum Step {
    /// The frame changed state: step it again.
    Again,
    Done(Done),
    Push(Vec<Pend>),
}

/// The facts an answer derives from what it read.
struct Facts {
    tx: Option<String>,
    /// The latest input asof (members, `T` and `to` included).
    latest: String,
    lo: String,
    hi: Option<String>,
    names: Arc<Names>,
    inputs: Vec<Use>,
    memo: bool,
    entries: BTreeSet<String>,
}

/// How an evaluated answer is kept.
struct How {
    /// Asof-dependent: its receipt's asof is the evaluation asof, and it holds there only.
    dependent: bool,
    /// Kept as a computed version, for later requests to validate.
    computed: bool,
    /// Kept in the memo.
    memo: bool,
    /// A further asof candidate: where a fault a read returned holds from, and the latest
    /// time `asof_compare` found at or before the asof.
    known: Option<String>,
}

fn store_err(op: &'static str) -> impl Fn(datumesh_store::Error) -> EvalError {
    move |e| EvalError::Store(StoreError::new(op, e))
}

impl<'e> Req<'e> {
    /// Marks the request a current read (CONTRACT-guard-patch G10): it stores as one, whatever its
    /// asof against `issued`.
    pub(crate) fn with_current(mut self, current: bool) -> Self {
        self.current = current;
        self
    }

    pub(crate) fn new(eng: &'e Engine, asof: &str, issued: &str) -> Req<'e> {
        Req {
            eng,
            asof: asof.to_string(),
            issued: issued.to_string(),
            names: HashMap::new(),
            done: HashMap::new(),
            slices: HashMap::new(),
            fresh: Vec::new(),
            slice_reads: BTreeMap::new(),
            declarations: HashMap::new(),
            learned: Vec::new(),
            budget: Budget::with_work(eng.fuel, eng.work_limit),
            scope: None,
            closures: HashMap::new(),
            current: false,
        }
    }

    /// A request in the scope of a scenario, sharing `budget` (a solve's iterations share one).
    pub(crate) fn scoped(eng: &'e Engine, asof: &str, issued: &str, scope: Arc<Scope>, budget: &Budget) -> Req<'e> {
        let mut req = Req::new(eng, asof, issued);
        req.budget = budget.share();
        req.scope = Some(scope);
        req
    }

    /// Whether the cell's value may change under the scenario's overrides: a computed cell
    /// whose function reaches an overridden function through its declared inputs (as of the
    /// request's asof). Such a cell is evaluated in the scope and never answered from the memo
    /// or the store. Always false outside a scenario, and for a stored cell (its own override
    /// is the only change it can have).
    fn dependent(&mut self, cell: &Cell, desc: &FunctionDescriptor) -> Result<bool, EvalError> {
        let Some(scope) = self.scope.clone() else { return Ok(false) };
        if desc.lang == STORED || scope.affected.is_empty() {
            return Ok(false);
        }
        let closure = self.closure(&cell.function_name())?;
        Ok(closure.iter().any(|n| scope.affected.contains(n)))
    }

    /// The function names `fn_name` reaches through declared inputs (itself included), each
    /// resolved as of the request's asof; cached per request.
    fn closure(&mut self, fn_name: &str) -> Result<Arc<HashSet<String>>, EvalError> {
        if let Some(c) = self.closures.get(fn_name) {
            return Ok(c.clone());
        }
        let asof = self.asof.clone();
        let mut seen: HashSet<String> = HashSet::new();
        let mut todo = vec![fn_name.to_string()];
        while let Some(name) = todo.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(hash) = self.resolve(&name, &asof)? else { continue };
            let Some(d) = self.eng.descriptor_of(&hash).map_err(eng_eval)? else { continue };
            todo.extend(d.inputs.iter().cloned());
        }
        let closure = Arc::new(seen);
        self.closures.insert(fn_name.to_string(), closure.clone());
        Ok(closure)
    }

    /// The scenario's override of cell `id`, if any.
    fn overridden(&self, id: &str) -> Option<(Cell, Scalar)> {
        self.scope.as_ref().and_then(|s| s.values.get(id).cloned())
    }

    /// The answer of an overridden cell: its value, unsigned, valid from the asof on.
    fn override_answer(&self, cell: &Cell, value: Scalar, asof: &str) -> Arc<Ans> {
        let id = cell.id();
        Arc::new(Ans {
            hash: scoped_hash(&id, &value),
            id,
            bytes: Arc::new(Vec::new()),
            value,
            tx: MIN_INSTANT.to_string(),
            asof: asof.to_string(),
            licence: self.eng.licence.clone(),
            lo: asof.to_string(),
            hi: None,
            names: Arc::new(Names::new()),
            cell: cell.clone(),
            inputs: Vec::new(),
            memo: false,
        })
    }

    /// The binding `fn_name` has as of `at`, its function hash and when it began (one query
    /// per name and instant per request).
    fn binding(&mut self, fn_name: &str, at: &str) -> Result<Option<(String, String)>, EvalError> {
        Ok(self.binding_ref(fn_name, at)?.clone())
    }

    /// [`binding`](Req::binding), borrowed (no allocation when it is known).
    fn binding_ref(&mut self, fn_name: &str, at: &str) -> Result<&Option<(String, String)>, EvalError> {
        let known = self.names.get(at).is_some_and(|m| m.contains_key(fn_name));
        if !known {
            let b = self
                .eng
                .store
                .resolve_binding(fn_name, at)
                .map_err(store_err("resolve_binding"))?
                .map(|b| (b.fn_hash, b.tx));
            self.names.entry(at.to_string()).or_default().insert(fn_name.to_string(), b);
        }
        Ok(self.names.get(at).and_then(|m| m.get(fn_name)).unwrap_or(&None))
    }

    /// The function hash `fn_name` resolves to as of `at`.
    fn resolve(&mut self, fn_name: &str, at: &str) -> Result<Option<String>, EvalError> {
        Ok(self.binding_ref(fn_name, at)?.as_ref().map(|(h, _)| h.clone()))
    }

    /// Whether `fn_name` resolves to `hash` as of `at`.
    fn resolves_to(&mut self, fn_name: &str, at: &str, hash: &str) -> Result<bool, EvalError> {
        Ok(self.binding_ref(fn_name, at)?.as_ref().is_some_and(|(h, _)| h == hash))
    }

    /// Whether every name of `names` still resolves as recorded, as of `asof` (or its own
    /// transaction time).
    fn names_hold(&mut self, names: &Names, asof: &str) -> Result<bool, EvalError> {
        for ((name, at), hash) in names {
            if !self.resolves_to(name, at.as_deref().unwrap_or(asof), hash)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn set_done(&mut self, asof: &str, id: &str, d: Done) {
        match self.done.get_mut(asof) {
            Some(m) => {
                m.insert(id.to_string(), d);
            }
            None => {
                self.done.entry(asof.to_string()).or_default().insert(id.to_string(), d);
            }
        }
    }

    fn done_at(&self, asof: &str, id: &str) -> Option<&Done> {
        self.done.get(asof).and_then(|m| m.get(id))
    }

    /// A cell's answer as of `asof` if it can be had without evaluating a function.
    fn lookup(
        &mut self,
        cell: &Cell,
        id: &str,
        desc: Arc<FunctionDescriptor>,
        asof: &str,
    ) -> Result<Lookup, EvalError> {
        if let Some(d) = self.done_at(asof, id) {
            return Ok(Lookup::Ready(d.clone()));
        }
        if let Some((c, value)) = self.overridden(id) {
            let d = Done::Value(self.override_answer(&c, value, asof));
            self.set_done(asof, id, d.clone());
            return Ok(Lookup::Ready(d));
        }
        if self.dependent(cell, &desc)? {
            return Ok(Lookup::Eval(desc));
        }
        if let Some(d) = self.memo_done(id, asof)? {
            self.set_done(asof, id, d.clone());
            return Ok(Lookup::Ready(d));
        }
        if desc.lang == STORED {
            let d = self.stored(cell, id, asof)?;
            self.set_done(asof, id, d.clone());
            return Ok(Lookup::Ready(d));
        }
        Ok(Lookup::Eval(desc))
    }

    /// A memoised answer holding at `asof` whose names still resolve as recorded.
    pub(crate) fn memo_hit(&mut self, id: &str, asof: &str) -> Result<Option<Arc<Ans>>, EvalError> {
        let Some(a) = self.eng.memo.get(id, asof) else { return Ok(None) };
        if !self.names_hold(&a.names, asof)? {
            return Ok(None);
        }
        Ok(Some(a))
    }

    /// A memoised fault holding at `asof` whose names still resolve as recorded.
    fn memo_fault_hit(&mut self, id: &str, asof: &str) -> Result<Option<Arc<FaultAns>>, EvalError> {
        let Some(f) = self.eng.memo.get_fault(id, asof) else { return Ok(None) };
        if !self.names_hold(&f.names, asof)? {
            return Ok(None);
        }
        Ok(Some(f))
    }

    /// The memoised answer of a cell as of `asof` (a value or a fault), if any.
    pub(crate) fn memo_done(&mut self, id: &str, asof: &str) -> Result<Option<Done>, EvalError> {
        if let Some(a) = self.memo_hit(id, asof)? {
            return Ok(Some(Done::Value(a)));
        }
        Ok(self.memo_fault_hit(id, asof)?.map(Done::Fault))
    }

    /// The request's asof.
    pub(crate) fn asof(&self) -> String {
        self.asof.clone()
    }

    /// A stored cell's answer: the version answering as of `asof`.
    pub(crate) fn stored(&mut self, cell: &Cell, id: &str, asof: &str) -> Result<Done, EvalError> {
        let found = self.eng.store.stored_asof_with_receipt(id, asof).map_err(store_err("stored_asof"))?;
        let Some((v, bytes)) = found else { return Ok(Done::NotCovered) };
        if v.withdrawn {
            // The null version: no value, so no `Receipt`, and nothing the memo keeps (the
            // next read asks the store again, and sees a later version as soon as it is stored).
            let w = parse_withdrawal(&v.receipt_hash, &bytes).map_err(EvalError::Store)?;
            return Ok(Done::Withdrawn(Arc::new(w)));
        }
        let p = self.eng.parsed(&v.receipt_hash, &bytes).map_err(EvalError::Store)?;
        // The version answers from its asof (never earlier than its tx) until a version later
        // by tx answers: from that one's asof. Only the memo needs to know.
        let hi = if self.eng.memo.enabled() {
            // R4 (G9): the versions the store recorded after this one, not those signed after it:
            // a correction signed earlier but recorded later ends the memo (its answer from its
            // recording on, or its asof if that is later).
            let all = self.eng.store.versions(id, MIN_INSTANT, MAX_INSTANT).map_err(store_err("versions"))?;
            all.into_iter().filter(|w| w.recorded > v.recorded).map(|w| std::cmp::max(w.asof, w.recorded)).min()
        } else {
            None
        };
        let r = &p.receipt;
        let a = Arc::new(Ans {
            id: id.to_string(),
            hash: v.receipt_hash.clone(),
            bytes: p.bytes.clone(),
            value: r.value().clone(),
            tx: r.tx().to_string(),
            asof: r.asof().to_string(),
            licence: r.licence().to_string(),
            lo: r.asof().to_string(),
            hi,
            names: Arc::new(Names::new()),
            cell: cell.clone(),
            inputs: Vec::new(),
            memo: true,
        });
        self.learned.push(Entry::Value(a.clone()));
        Ok(Done::Value(a))
    }

    /// Answers `cell` (a computed cell with descriptor `desc`) as of the request's asof: its
    /// frames run as one root, and the roots they split off run first (see
    /// [`run_roots`](Req::run_roots)).
    pub(crate) fn run(
        &mut self,
        cell: Cell,
        desc: Arc<FunctionDescriptor>,
        unsigned: bool,
    ) -> Result<Done, EngineError> {
        let id = cell.id();
        let asof = self.asof.clone();
        self.run_roots(vec![Pend { cell, id: id.clone(), desc, asof: asof.clone() }], unsigned)?;
        self.done_at(&asof, &id)
            .cloned()
            .ok_or_else(|| EngineError::Runtime { cell_id: id, message: "no answer".into() })
    }

    /// Runs the roots, the last first. A root whose frames would pass the depth limit is set
    /// aside, and the pending cell that did not fit becomes a root of its own, run first
    /// (trampolining, Build 2 E4). Nothing is lost: the answers the set-aside frames finished
    /// are in the request, and the root runs again and finds them. Fuel and work are charged
    /// throughout, so an unbounded recursion ends in `BudgetExceeded` or `WorkExceeded`.
    fn run_roots(&mut self, mut roots: Vec<Pend>, unsigned: bool) -> Result<(), EngineError> {
        while let Some(root) = roots.last().cloned() {
            if self.done_at(&root.asof, &root.id).is_some() {
                roots.pop();
                continue;
            }
            let top = unsigned && roots.len() == 1;
            match self.run_stack(root, top)? {
                Ended::Done => {
                    roots.pop();
                }
                Ended::Split(p) => roots.push(p),
            }
        }
        Ok(())
    }

    /// One root's frames, from the root to its answer on an explicit stack. Instead of pushing
    /// a frame past the depth limit it returns [`Ended::Split`] with the pending cell: the
    /// frames are dropped, and their suspended attempts' fuel released.
    fn run_stack(&mut self, root: Pend, unsigned: bool) -> Result<Ended, EngineError> {
        let key = |asof: &str, id: &str| format!("{asof} {id}");
        let state = if unsigned { State::Eval { cand: None } } else { State::Start };
        let mut stack = vec![Frame {
            cell: root.cell,
            id: root.id.clone(),
            asof: root.asof.clone(),
            desc: root.desc,
            state,
            unsigned,
            pending: Vec::new(),
            floor: 0,
        }];
        let mut on_stack: HashSet<String> = HashSet::from([key(&root.asof, &root.id)]);
        loop {
            let Some(top) = stack.last_mut() else { return Ok(Ended::Done) };
            if let Some(p) = top.pending.pop() {
                if self.done_at(&p.asof, &p.id).is_some() {
                    continue;
                }
                if on_stack.contains(&key(&p.asof, &p.id)) {
                    // The path: the frames from the cell that reappeared, then it again.
                    let at = stack.iter().position(|f| f.id == p.id && f.asof == p.asof).unwrap_or(0);
                    let mut path: Vec<String> = stack[at..].iter().map(|f| canonical_call(&f.cell)).collect();
                    path.push(canonical_call(&p.cell));
                    return Err(EngineError::Cycle { path });
                }
                if stack.len() >= self.eng.depth_limit {
                    for f in &stack {
                        self.budget.release(f.floor);
                    }
                    return Ok(Ended::Split(p));
                }
                on_stack.insert(key(&p.asof, &p.id));
                stack.push(Frame {
                    cell: p.cell,
                    id: p.id,
                    asof: p.asof,
                    desc: p.desc,
                    state: State::Start,
                    unsigned: false,
                    pending: Vec::new(),
                    floor: 0,
                });
                continue;
            }
            let step = match top.state {
                State::Start => self.start_frame(top)?,
                State::Recheck { .. } => self.recheck_frame(top)?,
                State::FromStore => self.store_frame(top)?,
                State::Validate { .. } => self.validate_frame(top)?,
                State::Eval { .. } => self.eval_frame(top)?,
            };
            match step {
                Step::Again => {}
                Step::Done(d) => {
                    if let Some(f) = stack.pop() {
                        on_stack.remove(&key(&f.asof, &f.id));
                        match &d {
                            Done::Value(a) => self.learned.push(Entry::Value(a.clone())),
                            // A member without a value ends a computed slice's read: the members
                            // after it (in key order) are not evaluated.
                            _ => {
                                if let Done::Fault(fa) = &d {
                                    self.learned.push(Entry::Fault(fa.clone()));
                                }
                                if let Some(parent) = stack.last_mut() {
                                    parent.pending.clear();
                                }
                            }
                        }
                        self.set_done(&f.asof, &f.id, d);
                    }
                }
                Step::Push(mut list) => {
                    list.reverse();
                    if let Some(top) = stack.last_mut() {
                        top.pending = list;
                    }
                }
            }
        }
    }

    /// A scenario's answer for `cell` (its descriptor `desc`) as of the request's asof. Before
    /// the cell runs, the dependent cells below it are evaluated in dependency order (from the
    /// receipts of its latest computed version), so that it reads them without suspending.
    pub(crate) fn scope_run(&mut self, cell: Cell, desc: Arc<FunctionDescriptor>) -> Result<Done, EngineError> {
        let id = cell.id();
        let asof = self.asof.clone();
        for p in self.scope_order(&cell)? {
            // An overridden cell's answer is the override, given by `lookup` (not evaluated here).
            if self.done_at(&p.asof, &p.id).is_some() || self.overridden(&p.id).is_some() {
                continue;
            }
            // A cycle here is an artifact of the order (the receipts were made at other
            // instants): the cell is then run as it is, and reports the cycle if it has one.
            match self.run_roots(vec![p], false) {
                Ok(()) | Err(EngineError::Cycle { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        match self.lookup(&cell, &id, desc.clone(), &asof).map_err(|e| engine_error(e, &id))? {
            Lookup::Ready(d) => Ok(d),
            Lookup::Eval(desc) => self.run(cell, desc, false),
        }
    }

    /// The dependent computed cells in the lineage of `top`'s latest computed version, inputs
    /// first (a post-order walk of the receipts, which are acyclic).
    fn scope_order(&mut self, top: &Cell) -> Result<Vec<Pend>, EngineError> {
        let asof = self.asof.clone();
        let Some((v, bytes)) = self
            .eng
            .store
            .computed_asof_with_receipt(&top.id(), &asof)
            .map_err(|e| StoreError::new("computed_asof", e))?
        else {
            return Ok(Vec::new());
        };
        let root = self.eng.parsed(&v.receipt_hash, &bytes)?;
        let mut seen: HashSet<String> = HashSet::from([top.id()]);
        let mut out = Vec::new();
        let mut stack: Vec<(Arc<Parsed>, Arc<FunctionDescriptor>, bool)> = Vec::new();
        for c in self.lineage(&root)? {
            if let Some(d) = self.scope_child(&c)? {
                stack.push((c, d, false));
            }
        }
        while let Some((r, d, expanded)) = stack.pop() {
            let cell = r.receipt.cell().clone();
            let id = cell.id();
            if expanded {
                out.push(Pend { cell, id, desc: d, asof: asof.clone() });
                continue;
            }
            if !seen.insert(id) {
                continue;
            }
            stack.push((r.clone(), d, true));
            for c in self.lineage(&r)? {
                if let Some(cd) = self.scope_child(&c)? {
                    stack.push((c, cd, false));
                }
            }
        }
        Ok(out)
    }

    /// The descriptor of `r`'s cell, when that cell is a dependent computed one.
    fn scope_child(&mut self, r: &Parsed) -> Result<Option<Arc<FunctionDescriptor>>, EngineError> {
        let cell = r.receipt.cell();
        let Some(d) = self.eng.descriptor_of(&cell.fn_hash)? else { return Ok(None) };
        let dependent = self.dependent(cell, &d).map_err(|e| engine_error(e, &cell.id()))?;
        Ok(dependent.then_some(d))
    }

    /// The receipts a receipt reads: its inputs, and for a slice entry its members' receipts.
    fn lineage(&mut self, r: &Parsed) -> Result<Vec<Arc<Parsed>>, EngineError> {
        let mut out = Vec::new();
        for entry in r.receipt.inputs() {
            if entry.starts_with("slice:") {
                let Some(bytes) = self.eng.manifest_bytes(entry)? else { continue };
                let m = Manifest::from_json(&bytes).map_err(EngineError::Invalid)?;
                for h in m.members() {
                    if let Some(p) = self.receipt(h)? {
                        out.push(p);
                    }
                }
            } else if let Some(p) = self.receipt(entry)? {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// A dropped answer to recheck, if the memo has one holding at the asof.
    fn start_frame(&mut self, f: &mut Frame) -> Result<Step, EngineError> {
        if self.dependent(&f.cell, &f.desc).map_err(|e| engine_error(e, &f.id))? {
            f.state = State::Eval { cand: None };
            return Ok(Step::Again);
        }
        f.state = match self.eng.memo.stale(&f.id, &f.asof) {
            Some(ans) => State::Recheck { ans, idx: 0, read: Vec::new(), names: Names::new() },
            None => State::FromStore,
        };
        Ok(Step::Again)
    }

    /// Rechecks a dropped answer as a version is validated (same receipt or value from each
    /// input it rested on, names resolving the same), from memory: no store query unless an
    /// input needs one. Differing: the store's version is looked at as usual.
    fn recheck_frame(&mut self, f: &mut Frame) -> Result<Step, EngineError> {
        let State::Recheck { ans, idx, read, names } = &mut f.state else { return Ok(Step::Again) };
        let ans = ans.clone();
        let asof = f.asof.clone();
        while let Some(u) = ans.inputs.get(*idx) {
            let check = match u {
                Use::Cell { id, cell, hash, value, at } => {
                    self.check_use(cell, id, hash, value, at.as_deref(), &asof, &ans.asof)?
                }
                Use::Slice(old) => {
                    let old_members: Vec<(String, Scalar)> =
                        old.members.iter().map(|m| (m.id.clone(), m.value.clone())).collect();
                    self.check_slice(&old.entry, &old.slice, Some(&old_members), &asof)?
                }
            };
            match check {
                Check::Same(rd, name, hash) => {
                    read.push(rd);
                    if let Some(name) = name {
                        names.insert(name, hash);
                    }
                    *idx += 1;
                }
                Check::Pending(list) => return Ok(Step::Push(list)),
                Check::Differs => {
                    f.state = State::FromStore;
                    return Ok(Step::Again);
                }
            }
        }
        let read = std::mem::take(read);
        let names = std::mem::take(names);
        // Licence inheritance (SPEC 8.5): kept only while the rule allows its licence.
        let parsed = self.eng.parsed(&ans.hash, &ans.bytes)?;
        if !self.reusable(f, &read, &parsed.receipt)? {
            f.state = State::FromStore;
            return Ok(Step::Again);
        }
        let facts = self.facts(&read, names);
        Ok(Step::Done(Done::Value(self.kept(
            f,
            facts,
            &ans.hash,
            &ans.bytes,
            &ans.value,
            (&ans.tx, &ans.asof),
            &ans.licence,
        ))))
    }

    /// The answer of a version kept: rechecked or validated.
    #[allow(clippy::too_many_arguments)]
    fn kept(
        &self,
        f: &Frame,
        facts: Facts,
        hash: &str,
        bytes: &Arc<Vec<u8>>,
        value: &Scalar,
        (tx, asof): (&str, &str),
        licence: &str,
    ) -> Arc<Ans> {
        let (lo, hi) = if self.asof_dependent(f) {
            (f.asof.clone(), Some(just_after(&f.asof)))
        } else {
            (facts.lo.as_str().max(asof).to_string(), facts.hi.clone())
        };
        Arc::new(Ans {
            id: f.id.clone(),
            hash: hash.to_string(),
            bytes: bytes.clone(),
            value: value.clone(),
            tx: tx.to_string(),
            asof: asof.to_string(),
            licence: licence.to_string(),
            lo,
            hi,
            names: facts.names,
            cell: f.cell.clone(),
            inputs: facts.inputs,
            memo: facts.memo,
        })
    }

    /// Whether the frame's function is declared asof-dependent by its runtime.
    fn asof_dependent(&self, f: &Frame) -> bool {
        self.eng.asof_dependent(&f.cell.fn_hash, &f.desc)
    }

    /// The store's computed version answering as of the asof, to validate; else evaluate.
    /// An asof-dependent function's version is reused only as of its own asof.
    fn store_frame(&mut self, f: &mut Frame) -> Result<Step, EngineError> {
        let found = self
            .eng
            .store
            .computed_asof_with_receipt(&f.id, &f.asof)
            .map_err(|e| StoreError::new("computed_asof", e))?;
        f.state = match found {
            Some((v, bytes)) => {
                let cand = self.eng.parsed(&v.receipt_hash, &bytes)?;
                if self.asof_dependent(f) && cand.receipt.asof() != f.asof {
                    State::Eval { cand: None }
                } else {
                    State::Validate { cand, idx: 0, read: Vec::new(), names: Names::new() }
                }
            }
            None => State::Eval { cand: None },
        };
        Ok(Step::Again)
    }

    /// Validates the candidate version: valid as of the asof when each input, its name
    /// resolving to the same function, gives the same receipt or the same value (early
    /// cutoff), and each slice entry the same members hash or the same members' values. An
    /// input not answered yet is pushed first.
    fn validate_frame(&mut self, f: &mut Frame) -> Result<Step, EngineError> {
        let State::Validate { cand, idx, read, names } = &mut f.state else { return Ok(Step::Again) };
        let cand = cand.clone();
        let asof = f.asof.clone();
        while let Some(h) = cand.receipt.inputs().get(*idx) {
            let check = if h.starts_with("slice:") {
                self.check_entry(h, &asof)?
            } else {
                self.check_input(h, &asof, cand.receipt.asof())?
            };
            match check {
                Check::Same(rd, name, hash) => {
                    read.push(rd);
                    if let Some(name) = name {
                        names.insert(name, hash);
                    }
                    *idx += 1;
                }
                Check::Pending(list) => return Ok(Step::Push(list)),
                Check::Differs => {
                    f.state = State::Eval { cand: Some(cand) };
                    return Ok(Step::Again);
                }
            }
        }
        let read = std::mem::take(read);
        let names = std::mem::take(names);
        // Licence inheritance (SPEC 8.5): a version whose licence the rule no longer gives
        // (a receipt signed before 0.4.1, an input now under another licence) is evaluated
        // again, then signed again or refused. A refusal the rule gives now, over the same
        // members' licences, is given without evaluating again (G3: the row's licences).
        if !self.reusable(f, &read, &cand.receipt)? {
            let licences = self.licences(&read)?;
            let issued = self.issued.clone();
            let rule = self.inherit(f, &licences, &issued, &self.eng.licence.clone())?;
            if rule.licence.is_none() {
                return Err(EngineError::Unlicensed { cell_id: f.id.clone(), reason: rule.reason });
            }
            f.state = State::Eval { cand: Some(cand) };
            return Ok(Step::Again);
        }
        let facts = self.facts(&read, names);
        let r = &cand.receipt;
        Ok(Step::Done(Done::Value(self.kept(
            f,
            facts,
            r.hash(),
            &cand.bytes,
            r.value(),
            (r.tx(), r.asof()),
            r.licence(),
        ))))
    }

    /// Whether input receipt `h` of a version made as of `reader` still answers as of the asof.
    fn check_input(&mut self, h: &str, asof: &str, reader: &str) -> Result<Check, EngineError> {
        let Some(input) = self.receipt(h)? else { return Ok(Check::Differs) };
        let r = &input.receipt;
        self.check_use(r.cell(), &r.cell().id(), h, r.value(), None, asof, reader)
    }

    /// Whether `cell`, its name resolving to its function, gives receipt `h` or value `value`
    /// as of `at` (a transaction-time read), else as of `asof`. `reader` is the asof of the
    /// answer that read it. A read of a name resolved the name as of the reader's asof (the
    /// binding then starts no later than it: see [`Req::binding_asof`]); a pinned read
    /// (`read_pinned`, SPEC 16.3) did not, relies on no binding, and is checked by its hash
    /// alone, with no name recorded.
    #[allow(clippy::too_many_arguments)]
    fn check_use(
        &mut self,
        cell: &Cell,
        id: &str,
        h: &str,
        value: &Scalar,
        at: Option<&str>,
        asof: &str,
        reader: &str,
    ) -> Result<Check, EngineError> {
        let when = at.unwrap_or(asof).to_string();
        let name = cell.function_name();
        let fail = |e: EvalError| engine_error(e, id);
        // Pinned: the name did not resolve to the hash as of the reader's asof. The binding
        // current as of `when` answers for the reader's asof too when it began no later (no
        // binding starts in between): the store is asked only otherwise.
        let pinned = match at {
            Some(_) => false,
            None => {
                let current = self.binding_ref(&name, &when).map_err(fail)?.clone();
                let at_reader = match current {
                    None => None,
                    Some((bound, start)) if start.as_str() <= reader => Some(bound),
                    Some(_) => self.resolve(&name, reader).map_err(fail)?,
                };
                at_reader.as_deref() != Some(cell.fn_hash.as_str())
            }
        };
        if !pinned && !self.resolves_to(&name, &when, &cell.fn_hash).map_err(fail)? {
            return Ok(Check::Differs);
        }
        self.budget.work(1).map_err(|e| engine_error(e, id))?;
        let Some(desc) = self.eng.descriptor_of(&cell.fn_hash)? else { return Ok(Check::Differs) };
        match self.lookup(cell, id, desc, &when).map_err(|e| engine_error(e, id))? {
            Lookup::Ready(Done::Value(a)) if a.hash == h || &a.value == value => Ok(Check::Same(
                Rd::Cell { ans: a, at: at.map(str::to_string) },
                (!pinned).then(|| (name, at.map(str::to_string))),
                cell.fn_hash.clone(),
            )),
            Lookup::Ready(_) => Ok(Check::Differs),
            Lookup::Eval(desc) => {
                Ok(Check::Pending(vec![Pend { cell: cell.clone(), id: id.to_string(), desc, asof: when }]))
            }
        }
    }

    /// Whether the slice entry `entry` of a version (its manifest from the store) has the
    /// same members, or members with the same values, as of `asof`.
    fn check_entry(&mut self, entry: &str, asof: &str) -> Result<Check, EngineError> {
        if let Some(check) = self.reuse_entry(entry, asof)? {
            return Ok(check);
        }
        let Some(bytes) = self.eng.store.manifest(entry) else {
            // No manifest held (MemberSetOnly, or stored before 0.6.2 under another policy): the
            // member-set row gives the slice; its members are read again, and the version stands
            // when they are the same set (the reuse without a member read is G3, not yet made).
            let Some(row) = self.eng.store.member_set(entry).map_err(|e| StoreError::new("member_set", e))? else {
                return Ok(Check::Differs);
            };
            let Ok(slice) = Slice::from_json(&row.slice) else { return Ok(Check::Differs) };
            let slice = Arc::new(slice);
            return match self.check_slice(entry, &slice, None, asof)? {
                Check::Same(Rd::Slice(now), name, hash) if now.entry == entry => {
                    Ok(Check::Same(Rd::Slice(now), name, hash))
                }
                _ => Ok(Check::Differs),
            };
        };
        let Ok(m) = Manifest::from_json(&bytes) else { return Ok(Check::Differs) };
        let slice = Arc::new(m.slice().clone());
        let check = self.check_slice(entry, &slice, None, asof)?;
        let Check::Same(Rd::Slice(now), name, hash) = check else { return Ok(check) };
        if now.entry == entry {
            return Ok(Check::Same(Rd::Slice(now), name, hash));
        }
        // Early cutoff: the same cells with the same values. The old members not among the
        // new ones are read from their receipts.
        let by_hash: HashMap<&str, &MemberRec> = now.members.iter().map(|x| (x.hash.as_str(), x)).collect();
        let mut old: Vec<(String, Scalar)> = Vec::with_capacity(m.members().len());
        for h in m.members() {
            match by_hash.get(h.as_str()) {
                Some(x) => old.push((x.id.clone(), x.value.clone())),
                None => match self.receipt(h)? {
                    Some(p) => old.push((p.receipt.cell().id(), p.receipt.value().clone())),
                    None => return Ok(Check::Differs),
                },
            }
        }
        Ok(if same_members(&old, &now) { Check::Same(Rd::Slice(now), name, hash) } else { Check::Differs })
    }

    /// G3, "inserted since" (decided 26 Nov): a stored-function slice's row stands for a read as of
    /// `asof` when the asof is the row's own or later, no version of the slice was inserted since
    /// the row's `seen_seq` (`fn_seq` at or below it, else `slice_seq` still equal to it), and the row
    /// is not marked 0 (a change was recorded after its asof when it was read). The shell carries
    /// the row's licences and reads no member; it is never memoised nor written. `None`: the caller
    /// reads the members.
    fn reuse_entry(&mut self, entry: &str, asof: &str) -> Result<Option<Check>, EngineError> {
        if self.scope.is_some() {
            return Ok(None);
        }
        let Some(row) = self.eng.store.member_set(entry).map_err(|e| StoreError::new("member_set", e))? else {
            return Ok(None);
        };
        if row.seen_seq <= 0 || asof < row.asof.as_str() {
            return Ok(None);
        }
        let Ok(slice) = Slice::from_json(&row.slice) else { return Ok(None) };
        let slice = Arc::new(slice);
        let name = slice.function_name();
        let fail = |e: EvalError| engine_error(e, entry);
        if !self.resolves_to(&name, asof, slice.fn_hash()).map_err(fail)? {
            return Ok(None);
        }
        let Some(desc) = self.eng.descriptor_of(slice.fn_hash())? else { return Ok(None) };
        if desc.lang != STORED {
            return Ok(None);
        }
        // The quick check: no version of the function since the row was read (no candidate walk).
        if self.eng.store.fn_seq(slice.fn_hash()).map_err(|e| StoreError::new("fn_seq", e))? > row.seen_seq {
            let now = self.eng.store.slice_seq(&slice).map_err(|e| StoreError::new("slice_seq", e))?;
            if now != row.seen_seq {
                return Ok(None);
            }
        }
        self.budget.work(1).map_err(fail)?;
        let fn_hash = slice.fn_hash().to_string();
        let shell = SliceRead {
            entry: entry.to_string(),
            manifest: Arc::new(Vec::new()),
            set: None,
            seen_seq: row.seen_seq,
            row_licences: Some(row.licences.clone()),
            asof: row.asof.clone(),
            fn_name: name.clone(),
            fn_hash: fn_hash.clone(),
            computed: false,
            members: Vec::new(),
            member_ans: Vec::new(),
            lo: row.tx.clone(),
            hi: None,
            tx: row.tx.clone(),
            memo: false,
            slice,
        };
        Ok(Some(Check::Same(Rd::Slice(Arc::new(shell)), Some((name, None)), fn_hash)))
    }

    /// Reads `slice` again as of `asof` (its name resolving to its function). With `old`
    /// (the old members' cell ids and values) it is the same when the entry is, or when the
    /// cells and values are; without, the caller compares.
    fn check_slice(
        &mut self,
        entry: &str,
        slice: &Arc<Slice>,
        old: Option<&[(String, Scalar)]>,
        asof: &str,
    ) -> Result<Check, EngineError> {
        let fail = |e: EvalError| engine_error(e, entry);
        let name = slice.function_name();
        if !self.resolves_to(&name, asof, slice.fn_hash()).map_err(fail)? {
            return Ok(Check::Differs);
        }
        let Some(desc) = self.eng.descriptor_of(slice.fn_hash())? else { return Ok(Check::Differs) };
        let Some(spec) = spec_of(slice) else { return Ok(Check::Differs) };
        let now = match self.slice_at(&name, slice.clone(), desc, spec, asof).map_err(fail)? {
            SliceOutcome::Ready(s) => s,
            SliceOutcome::Pending(list) => return Ok(Check::Pending(list)),
            SliceOutcome::Absent(_) => return Ok(Check::Differs),
        };
        self.budget.work(1 + now.members.len() as u64).map_err(fail)?;
        let same = match old {
            None => true,
            Some(o) => now.entry == entry || same_members(o, &now),
        };
        let fn_hash = slice.fn_hash().to_string();
        Ok(if same { Check::Same(Rd::Slice(now), Some((name, None)), fn_hash) } else { Check::Differs })
    }

    /// The slice `slice` of function `fn_name` (descriptor `desc`) as of `asof`: for a
    /// stored function its members from the store; for a computed one the cells of its
    /// extent, each answered (pending when one is not yet); a version slice's versions.
    fn slice_at(
        &mut self,
        fn_name: &str,
        slice: Arc<Slice>,
        desc: Arc<FunctionDescriptor>,
        spec: Spec,
        asof: &str,
    ) -> Result<SliceOutcome, EvalError> {
        let key = (asof.to_string(), slice.id().to_string());
        if let Some(s) = self.slices.get(&key) {
            return Ok(SliceOutcome::Ready(s.clone()));
        }
        let fn_hash = slice.fn_hash().to_string();
        let computed = desc.lang != STORED;
        let mut members = Vec::new();
        let mut member_ans = Vec::new();
        let (lo, hi, memo);
        let mut seen_seq = 0i64;
        // The withdrawal that left a member out, as (tx, asof) (SPEC 8.2), when there is one.
        let mut withdrawn_at: Option<(String, String)> = None;
        match (&spec, computed) {
            (Spec::Cells { pattern }, true) => {
                let cells = extent_cells(fn_name, &fn_hash, &desc, pattern).map_err(EvalError::InvalidInput)?;
                let mut pending = Vec::new();
                let mut absent: Option<String> = None;
                // In key order, up to the first member without a value (SPEC 15.13, 15.12):
                // members after it are neither evaluated nor charged. Members not answered yet
                // before it are evaluated first (in key order; the run stops pushing them at
                // the first that has no value).
                for cell in cells {
                    let id = cell.id();
                    match self.lookup(&cell, &id, desc.clone(), asof)? {
                        Lookup::Ready(Done::Value(a)) => member_ans.push(a),
                        Lookup::Ready(Done::Fault(fa)) => {
                            absent = Some(fa.known.clone());
                            break;
                        }
                        Lookup::Ready(_) => {
                            absent = Some(MIN_INSTANT.to_string());
                            break;
                        }
                        Lookup::Eval(d) => pending.push(Pend { cell, id, desc: d, asof: asof.to_string() }),
                    }
                }
                if !pending.is_empty() {
                    return Ok(SliceOutcome::Pending(pending));
                }
                if let Some(known) = absent {
                    return Ok(SliceOutcome::Absent(known));
                }
                for a in &member_ans {
                    members.push(MemberRec {
                        id: a.id.clone(),
                        hash: a.hash.clone(),
                        value: a.value.clone(),
                        args: a.cell.args.clone(),
                        tx: a.tx.clone(),
                        asof: a.asof.clone(),
                        recorded: a.tx.clone(),
                        scoped: false,
                    });
                }
                lo = member_ans.iter().map(|a| a.lo.clone()).max();
                hi = member_ans.iter().filter_map(|a| a.hi.clone()).min();
                memo = member_ans.iter().all(|a| a.memo);
            }
            (_, true) => {
                return Err(EvalError::InvalidInput(CoreError::new(
                    ErrorCode::Slice,
                    "$",
                    "versions() of a computed function: versions are of stored cells",
                )))
            }
            (Spec::Cells { .. } | Spec::Versions { .. }, false) => {
                // Values, hashes, tx and asof from the store's columns and its typed-argument
                // index (store 0.3.2): no receipt bytes; the receipts are not needed to read a
                // slice, only to sign what is read.
                // The slice's seq is read before its members (G3): a change after it is one the
                // row does not hold, and the reuse check sees it as an insertion since.
                seen_seq = self.eng.store.slice_seq(&slice).map_err(store_err("slice_seq"))?;
                let found = self.eng.store.slice_values(&slice, asof).map_err(store_err("slice_values"))?;
                for v in found {
                    // The member's answer is not before the time the store recorded it (G9): a
                    // backfill signed earlier is an answer from its recording on.
                    let asof = std::cmp::max(v.version.asof.clone(), v.version.recorded.clone());
                    members.push(MemberRec {
                        id: v.version.cell_id,
                        hash: v.version.receipt_hash,
                        value: v.value,
                        args: v.cell.args,
                        tx: v.version.tx,
                        asof,
                        recorded: v.version.recorded,
                        scoped: false,
                    });
                }
                if let Some(scope) = self.scope.clone() {
                    apply_scope(&scope, &slice, asof, &mut members);
                }
                // Holding past the asof: when no version of a matching cell answers later, else
                // at the asof only. Versions stored later are the dirty set's.
                // A version recorded (or answering) after the asof: the members as of a later
                // asof may differ from these, so the row is not reused (G3).
                let changed =
                    self.eng.store.slice_changed_after(&slice, asof).map_err(store_err("slice_changed_after"))?;
                if changed {
                    seen_seq = 0;
                }
                hi = if self.eng.memo.enabled() && changed { Some(just_after(asof)) } else { None };
                let to = match &spec {
                    Spec::Versions { to, .. } => Some(to.clone()),
                    Spec::Cells { .. } => None,
                };
                // SPEC 8.2: a withdrawal that left a member out counts for the reader's time.
                withdrawn_at =
                    self.eng.store.slice_withdrawn_latest(&slice, asof).map_err(store_err("slice_withdrawn_latest"))?;
                lo = members
                    .iter()
                    .map(|m| m.asof.clone())
                    .chain(to)
                    .chain(withdrawn_at.as_ref().map(|w| w.1.clone()))
                    .max();
                memo = true;
            }
        }
        let mut hashes: Vec<String> = members.iter().map(|m| m.hash.clone()).collect();
        hashes.sort();
        hashes.dedup();
        let manifest = Manifest::new((*slice).clone(), hashes).map_err(EvalError::InvalidInput)?;
        let s = Arc::new(SliceRead {
            entry: manifest.entry(),
            manifest: Arc::new(manifest.canonical()),
            fn_name: fn_name.to_string(),
            fn_hash,
            computed,
            tx: members.iter().map(|m| m.recorded.clone()).chain(withdrawn_at.as_ref().map(|w| w.0.clone())).max(),
            set: Some(manifest),
            seen_seq,
            row_licences: None,
            asof: asof.to_string(),
            members,
            member_ans,
            lo,
            hi,
            memo,
            slice,
        });
        self.slices.insert(key, s.clone());
        Ok(SliceOutcome::Ready(s))
    }

    /// A stored receipt by hash, parsed (cached).
    fn receipt(&self, hash: &str) -> Result<Option<Arc<Parsed>>, EngineError> {
        if let Some(p) = self.eng.receipts.get(hash) {
            return Ok(Some(p));
        }
        match self.eng.store.receipt(hash) {
            Some(bytes) => Ok(Some(self.eng.parsed(hash, &bytes)?)),
            None => Ok(None),
        }
    }

    /// What answers resting on `read` derive: tx, the latest input asof, the interval, names,
    /// the inputs and whether the memo may keep them.
    fn facts(&self, read: &[Rd], own: Names) -> Facts {
        fn raise(slot: &mut String, v: &str) {
            if v > slot.as_str() {
                *slot = v.to_string();
            }
        }
        fn lower(hi: &mut Option<String>, v: &Option<String>) {
            if let Some(v) = v {
                if hi.as_ref().is_none_or(|h| v < h) {
                    *hi = Some(v.clone());
                }
            }
        }
        let mut tx: Option<String> = None;
        let mut latest = MIN_INSTANT.to_string();
        let mut lo = MIN_INSTANT.to_string();
        let mut hi: Option<String> = None;
        // The names, sharing an input's map when nothing is added to it.
        let mut names: Option<Arc<Names>> = None;
        let add = |names: &mut Option<Arc<Names>>, from: &Arc<Names>| match names {
            None => *names = Some(from.clone()),
            Some(acc) => {
                if from.keys().any(|k| !acc.contains_key(k)) {
                    let m = Arc::make_mut(acc);
                    for (k, v) in from.iter() {
                        m.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        };
        let mut extra = Names::new();
        let mut inputs = Vec::new();
        let mut memo = true;
        let mut entries = BTreeSet::new();
        for rd in read {
            entries.insert(rd.entry().to_string());
            match rd {
                Rd::Cell { ans, at } => {
                    if tx.as_ref().is_none_or(|t| ans.tx > *t) {
                        tx = Some(ans.tx.clone());
                    }
                    memo &= ans.memo;
                    match at {
                        None => {
                            raise(&mut latest, &ans.lo);
                            raise(&mut lo, &ans.lo);
                            lower(&mut hi, &ans.hi);
                            add(&mut names, &ans.names);
                        }
                        Some(t) => {
                            raise(&mut latest, t);
                            raise(&mut latest, &ans.asof);
                            for ((n, a), v) in ans.names.iter() {
                                let at = a.clone().unwrap_or_else(|| t.clone());
                                extra.entry((n.clone(), Some(at))).or_insert_with(|| v.clone());
                            }
                        }
                    }
                    inputs.push(Use::Cell {
                        id: ans.id.clone(),
                        cell: ans.cell.clone(),
                        hash: ans.hash.clone(),
                        value: ans.value.clone(),
                        at: at.clone(),
                    });
                }
                Rd::Slice(s) => {
                    if let Some(t) = &s.tx {
                        if tx.as_ref().is_none_or(|x| t > x) {
                            tx = Some(t.clone());
                        }
                    }
                    if let Some(l) = &s.lo {
                        raise(&mut latest, l);
                        raise(&mut lo, l);
                    }
                    lower(&mut hi, &s.hi);
                    memo &= s.memo;
                    extra.entry((s.fn_name.clone(), None)).or_insert_with(|| s.fn_hash.clone());
                    for a in &s.member_ans {
                        add(&mut names, &a.names);
                    }
                    inputs.push(Use::Slice(s.clone()));
                }
            }
        }
        // Own names first (as before: the first recorded wins), then what the inputs add.
        let mut names = names.unwrap_or_default();
        let missing = |m: &Names, from: &Names| from.keys().any(|k| !m.contains_key(k));
        if missing(&names, &own) || missing(&names, &extra) {
            let m = Arc::make_mut(&mut names);
            for (k, v) in own.into_iter().chain(extra) {
                m.insert(k, v);
            }
        }
        Facts { tx, latest, lo, hi, names, inputs, memo, entries }
    }

    /// Runs a frame's runtime once.
    fn eval_frame(&mut self, f: &mut Frame) -> Result<Step, EngineError> {
        let Some(rt) = self.eng.runtimes.get(&f.desc.lang).cloned() else {
            return Err(EngineError::NoRuntime { lang: f.desc.lang.clone(), fn_hash: f.cell.fn_hash.clone() });
        };
        let declared = self.eng.asof_dependent(&f.cell.fn_hash, &f.desc);
        let mut budget = self.budget.share();
        let floor = std::mem::take(&mut f.floor);
        budget.begin_attempt(floor);
        let mut reader = Reader {
            budget: self.budget.share(),
            asof: f.asof.clone(),
            req: self,
            desc: &f.desc,
            read: BTreeMap::new(),
            names: Names::new(),
            stop: None,
            faulted: false,
            fault_known: MIN_INSTANT.to_string(),
            compared: MIN_INSTANT.to_string(),
            future_seen: false,
            at_read: false,
            asof_read: false,
            fault_deps: Vec::new(),
            unmemo: false,
        };
        let result = rt.evaluate(&f.desc, &f.cell, &mut reader, &mut budget);
        let Reader {
            read,
            names,
            stop,
            faulted,
            fault_known,
            compared,
            future_seen,
            at_read,
            asof_read,
            fault_deps,
            unmemo,
            ..
        } = reader;
        let fail = |e: EvalError| engine_error(e, &f.id);
        // Exhaustion wins over anything the runtime made of it.
        if budget.is_exhausted() {
            return Err(exhausted(&budget));
        }
        match stop {
            Some(Stop::Pending(list)) => {
                f.floor = budget.suspend_attempt();
                return Ok(Step::Push(list));
            }
            Some(Stop::NotCovered) => {
                budget.finish_attempt();
                // The times come before the cells (SPEC 15.13): E_FORMULA_FUTURE the runtime
                // established (a time found after the asof) stands over an input without a
                // value. Either way the cell has no value; this is which code it reports. Not
                // memoised: the input without a value has no memo entry to follow.
                if matches!(result, Err(EvalError::Fault(Fault::Future))) && (future_seen || asof_read) {
                    let read: Vec<Rd> = read.into_values().collect();
                    let facts = self.facts(&read, names);
                    let fa = self.fault_answer(f, facts, &fault_deps, Fault::Future, None, false)?;
                    return Ok(Step::Done(Done::Fault(fa)));
                }
                return Ok(Step::Done(Done::NotCovered));
            }
            Some(Stop::Fatal(e)) => return Err(fail(e)),
            None => {}
        }
        budget.finish_attempt();
        let read: Vec<Rd> = read.into_values().collect();
        let value = match result {
            Ok(v) => v,
            Err(EvalError::Fault(code)) => {
                // Where the fault holds from: as a receipt's asof would be, had it a value. A
                // fault a read returned (caught or not) is a cell's: memoised as its own.
                let facts = self.facts(&read, names);
                let latest = facts.latest.clone().max(fault_known).max(compared);
                let memo = !unmemo && !at_read;
                let latest = (!(declared || asof_read)).then_some(latest);
                let fa = self.fault_answer(f, facts, &fault_deps, code, latest.as_deref(), memo)?;
                return Ok(Step::Done(Done::Fault(fa)));
            }
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
            for rd in &read {
                if let Rd::Slice(s) = rd {
                    self.slice_reads.insert(s.entry.clone(), s.clone());
                }
            }
            let entries: BTreeSet<String> = read.iter().map(|r| r.entry().to_string()).collect();
            return Ok(Step::Done(Done::Unsigned(value, entries.into_iter().collect())));
        }
        let cand = match &f.state {
            State::Eval { cand } => cand.clone(),
            _ => None,
        };
        let how = How {
            dependent: declared || asof_read,
            // A version is kept for later requests to validate only when its receipt says
            // everything validation needs: not when the value rests on a caught fault, on a
            // read at a transaction time, or on the asof read undeclared.
            computed: !faulted && !at_read && !(asof_read && !declared),
            memo: !faulted,
            known: Some(if faulted { fault_known.max(compared) } else { compared }),
        };
        let ans = self.answer_with(f, cand, value, &read, names, how)?;
        Ok(Step::Done(Done::Value(ans)))
    }

    /// The answer of a faulted cell (SPEC 15.12) for frame `f`: the fault, which holds from
    /// the instant its receipt's asof would have, and rests on what the evaluation read
    /// (`facts`) and on the faulted cells `deps` and their names. `latest` is the latest
    /// instant of what it read and relied on, from which the instant is found; `None` for a
    /// fault asof-dependent (16.7), which holds as of the frame's asof only. Kept by the memo
    /// when `memo` and nothing it rests on says otherwise.
    fn fault_answer(
        &mut self,
        f: &Frame,
        mut facts: Facts,
        deps: &[Arc<FaultAns>],
        fault: Fault,
        latest: Option<&str>,
        memo: bool,
    ) -> Result<Arc<FaultAns>, EngineError> {
        let mut faulted = Vec::with_capacity(deps.len());
        for d in deps {
            facts.memo &= d.memo;
            if let Some(h) = &d.hi {
                if facts.hi.as_ref().is_none_or(|x| h < x) {
                    facts.hi = Some(h.clone());
                }
            }
            if d.names.keys().any(|k| !facts.names.contains_key(k)) {
                let m = Arc::make_mut(&mut facts.names);
                for (k, v) in d.names.iter() {
                    m.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            faulted.push(d.id.clone());
        }
        let (known, lo, hi) = match latest {
            None => (f.asof.clone(), f.asof.clone(), Some(just_after(&f.asof))),
            Some(latest) => {
                let tx = facts.tx.clone().unwrap_or_else(|| MIN_INSTANT.to_string());
                let known = self
                    .binding_asof(latest.to_string(), &facts.names, &f.asof)
                    .map_err(|e| engine_error(e, &f.id))?
                    .max(tx);
                (known.clone(), known, facts.hi.clone())
            }
        };
        Ok(Arc::new(FaultAns {
            id: f.id.clone(),
            fault,
            known,
            lo,
            hi,
            names: facts.names,
            inputs: facts.inputs,
            faulted,
            memo: memo && facts.memo,
        }))
    }

    /// The answer for frame `f`'s cell, evaluated to `value` from `read`: the version that
    /// did not validate, `cand`, when its value is the same (early cutoff: keep the current
    /// version); a receipt already signed for exactly these inputs; else a new receipt.
    fn answer_with(
        &mut self,
        f: &Frame,
        cand: Option<Arc<Parsed>>,
        value: Scalar,
        read: &[Rd],
        own: Names,
        how: How,
    ) -> Result<Arc<Ans>, EngineError> {
        let mut facts = self.facts(read, own);
        if let Some(k) = &how.known {
            if *k > facts.latest {
                facts.latest = k.clone();
            }
        }
        let tx = facts.tx.clone().unwrap_or_else(|| MIN_INSTANT.to_string());
        // Each input's answer holds from its `lo` on (its receipt's asof, or later when the
        // answer is a version validated or kept across a change of its own inputs). The latest
        // of these, of the members' asofs, of the transaction times read, and of the start of
        // every name binding relied on, is where this value is known (SPEC 8.2 rule 2); for an
        // asof-dependent function, the evaluation asof.
        let asof = if how.dependent {
            f.asof.clone()
        } else {
            let a =
                self.binding_asof(facts.latest.clone(), &facts.names, &f.asof).map_err(|e| engine_error(e, &f.id))?;
            a.max(tx.clone())
        };
        let (lo, hi) = if how.dependent {
            (f.asof.clone(), Some(just_after(&f.asof)))
        } else {
            (facts.lo.as_str().max(asof.as_str()).to_string(), facts.hi.clone())
        };
        let memo = how.memo && facts.memo;
        let licences = self.licences(read)?;
        let mut existing = None;
        if let Some(c) = &cand {
            if c.receipt.value() == &value
                && (!how.dependent || c.receipt.asof() == f.asof)
                && self.entitled_with(f, &licences, &c.receipt)?
            {
                existing = Some(c.clone());
            } else if self.scope.is_none() && c.receipt.tx() == tx && c.receipt.asof() == asof {
                // The new version would share the old one's (tx, asof), which the store's key
                // holds once (an input corrected with a tx earlier than another input's; a
                // version signed again for its licence): a receipt signed before for exactly
                // these inputs, with a licence the rule allows, is reused rather than signing
                // another.
                existing = self.signed_before(f, &facts.entries, &value, &licences)?;
            }
        }
        if let Some(p) = existing {
            let r = &p.receipt;
            return Ok(Arc::new(Ans {
                id: f.id.clone(),
                hash: r.hash().to_string(),
                bytes: p.bytes.clone(),
                value,
                tx: r.tx().to_string(),
                asof: r.asof().to_string(),
                licence: r.licence().to_string(),
                lo: lo.as_str().max(r.asof()).to_string(),
                hi,
                names: facts.names,
                cell: f.cell.clone(),
                inputs: facts.inputs,
                memo,
            }));
        }
        // Licence inheritance (SPEC 8.5): the licence of the receipt to be issued now, or a
        // refusal (nothing is signed; the request fails, and with it every reader).
        let issued = self.issued.clone();
        let rule = self.inherit(f, &licences, &issued, &self.eng.licence.clone())?;
        let Some(licence) = rule.licence else {
            return Err(EngineError::Unlicensed { cell_id: f.id.clone(), reason: rule.reason });
        };
        if self.scope.is_some() {
            // A scenario writes nothing: its answer is unsigned, and nothing is kept.
            return Ok(Arc::new(Ans {
                id: f.id.clone(),
                hash: scoped_hash(&f.id, &value),
                bytes: Arc::new(Vec::new()),
                value,
                tx,
                asof,
                licence,
                lo,
                hi,
                names: facts.names,
                cell: f.cell.clone(),
                inputs: facts.inputs,
                memo: false,
            }));
        }
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
            .inputs(facts.entries.iter().cloned())
            .licence(licence.clone())
            .summary(summary)
            .sign(&self.eng.key)
            .map_err(EngineError::Sign)?;
        let bytes = Arc::new(receipt_json(&receipt));
        let hash = receipt.hash().to_string();
        for rd in read {
            if let Rd::Slice(s) = rd {
                self.slice_reads.insert(s.entry.clone(), s.clone());
            }
        }
        self.fresh.push(Fresh {
            cell_id: f.id.clone(),
            tx: tx.clone(),
            hash: hash.clone(),
            bytes: bytes.clone(),
            computed: how.computed,
        });
        Ok(Arc::new(Ans {
            id: f.id.clone(),
            hash,
            bytes,
            value,
            tx,
            asof,
            licence,
            lo,
            hi,
            names: facts.names,
            cell: f.cell.clone(),
            inputs: facts.inputs,
            memo,
        }))
    }

    /// The asof of a new receipt: the latest of `latest` and the start of every name binding
    /// the value relied on (`names`: the names read, transitively, as of `asof` or of their
    /// own transaction time). From then on, up to the asof, the inputs and the names are as
    /// they were read, so the receipt's value is the cell's value as of its own asof.
    fn binding_asof(&mut self, latest: String, names: &Names, asof: &str) -> Result<String, EvalError> {
        let mut out = latest;
        for ((name, at), hash) in names {
            match self.binding(name, at.as_deref().unwrap_or(asof))? {
                Some((h, start)) if h == *hash => {
                    if start > out {
                        out = start;
                    }
                }
                // Not as recorded (cannot happen: every name was resolved in this request):
                // the asof, at which the names are known.
                _ => return Ok(asof.to_string()),
            }
        }
        Ok(out)
    }

    /// A stored receipt of frame `f`'s cell by this mesh with exactly the inputs `entries`
    /// and `value`, and a licence the rule allows for the input licences `licences`
    /// ([`entitled_with`](Req::entitled_with)).
    fn signed_before(
        &mut self,
        f: &Frame,
        entries: &BTreeSet<String>,
        value: &Scalar,
        licences: &[String],
    ) -> Result<Option<Arc<Parsed>>, EngineError> {
        let id = f.id.as_str();
        let Some(first) = entries.iter().next() else { return Ok(None) };
        let candidates = if first.starts_with("slice:") {
            self.eng.store.slice_dependents(first).map_err(|e| StoreError::new("slice_dependents", e))?
        } else {
            self.eng.store.dependents(first).map_err(|e| StoreError::new("dependents", e))?
        };
        for h in candidates {
            let Some(p) = self.receipt(&h)? else { continue };
            let r = &p.receipt;
            if r.signer_host() == self.eng.host
                && r.value() == value
                && r.inputs().len() == entries.len()
                && r.inputs().iter().zip(entries.iter()).all(|(a, b)| a == b)
                && r.cell().id() == id
                && self.entitled_with(f, licences, r)?
            {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }

    /// The licences of the input receipts of `read` (SPEC 8.5): one per receipt read, every
    /// member of a slice included (a stored slice's members from their receipts in the store).
    fn licences(&mut self, read: &[Rd]) -> Result<Vec<String>, EngineError> {
        let mut out = Vec::new();
        for rd in read {
            match rd {
                Rd::Cell { ans, .. } => out.push(ans.licence.clone()),
                Rd::Slice(s) if s.row_licences.is_some() => {
                    out.extend(s.row_licences.iter().flatten().cloned());
                }
                Rd::Slice(s) if s.computed => out.extend(s.member_ans.iter().map(|a| a.licence.clone())),
                Rd::Slice(s) => {
                    for m in &s.members {
                        if m.scoped {
                            out.push(self.eng.licence.clone());
                            continue;
                        }
                        let Some(p) = self.receipt(&m.hash)? else {
                            return Err(StoreError::new(
                                "receipt",
                                datumesh_store::Error::NotFound { what: "receipt", key: m.hash.clone() },
                            )
                            .into());
                        };
                        out.push(p.receipt.licence().to_string());
                    }
                }
            }
        }
        Ok(out)
    }

    /// The declarations of the publisher of namespace `ns` (the host of `ns`, SPEC 8.5), from
    /// its key document in the store; none when the store has no document for the host, or
    /// one without declarations.
    fn declarations(&mut self, ns: &str) -> Result<Option<Arc<Declarations>>, EngineError> {
        let host = ns.split('/').next().unwrap_or(ns);
        if let Some(d) = self.declarations.get(host) {
            return Ok(d.clone());
        }
        let d = match self.eng.store.namespace(host).map_err(|e| StoreError::new("namespace", e))? {
            // The store checked the document when it was recorded (SPEC 9.2).
            Some(n) => KeyDocument::from_json(host, &n.keydoc).ok().and_then(|k| k.declarations).map(Arc::new),
            None => None,
        };
        self.declarations.insert(host.to_string(), d.clone());
        Ok(d)
    }

    /// Licence inheritance (SPEC 8.5, core's `inherit_licence`) for frame `f`'s cell over the
    /// input licences `licences`, for a receipt issued at `issued`; `mesh_licence` for a cell
    /// with no input. The publisher's declarations are read only when the rule needs them (a
    /// restricted input). Then the local acceptance rule (E1) applies to a refusal it may
    /// override (`accept`).
    fn inherit(
        &mut self,
        f: &Frame,
        licences: &[String],
        issued: &str,
        mesh_licence: &str,
    ) -> Result<Rule, EngineError> {
        let function = f.cell.function_name();
        let without = inherit_licence(licences, None, &function, issued, mesh_licence);
        let core = if matches!(without.reason, LicenceReason::NotDeclared | LicenceReason::DeclarationNotOpen) {
            let d = self.declarations(&f.cell.ns)?;
            inherit_licence(licences, d.as_deref(), &function, issued, mesh_licence)
        } else {
            without
        };
        self.accept(f, licences, Rule { licence: core.licence, reason: core.reason })
    }

    /// The local acceptance policy (engine 0.6.1, E1; D4.1): on an unrooted host, a refusal of
    /// core over restricted input licences (`not_declared`, `declaration_not_open`,
    /// `restricted_conflict`) is computed when every restricted input licence is accepted, under
    /// the one licence or LOCAL_DERIVATION_LICENCE. Otherwise the refusal stands for a
    /// rooted host, and `NotAccepted` names the restricted licences not accepted. The accepted
    /// set is read now, so a memoised receipt is judged under the current set.
    fn accept(&self, f: &Frame, licences: &[String], rule: Rule) -> Result<Rule, EngineError> {
        if rule.licence.is_some() || !is_unrooted(&self.eng.host) || !overridable(rule.reason) {
            return Ok(rule);
        }
        if licences.iter().all(|l| is_open(l)) {
            return Ok(rule);
        }
        let accepted = self.eng.accepted.read().unwrap_or_else(|p| p.into_inner());
        match decide(licences, &accepted) {
            Verdict::Give(licence) => Ok(Rule { licence: Some(licence), reason: rule.reason }),
            Verdict::Missing(licences) => Err(EngineError::NotAccepted { cell_id: f.id.clone(), licences }),
        }
    }

    /// Whether receipt `r` of frame `f`'s cell may be reused over inputs with licences
    /// `licences`: entitled at its own `issued` (its licence is the rule's then, as the
    /// entitlement check judges it, SPEC 10.3; a declaration closed since does not matter),
    /// or its licence is the one the rule gives now.
    fn entitled_with(&mut self, f: &Frame, licences: &[String], r: &Receipt) -> Result<bool, EngineError> {
        let then = self.inherit(f, licences, r.issued(), r.licence())?;
        if then.licence.as_deref() == Some(r.licence()) {
            return Ok(true);
        }
        let issued = self.issued.clone();
        let now = self.inherit(f, licences, &issued, &self.eng.licence.clone())?;
        Ok(now.licence.as_deref() == Some(r.licence()))
    }

    /// [`entitled_with`](Req::entitled_with) over the licences of `read`.
    fn reusable(&mut self, f: &Frame, read: &[Rd], r: &Receipt) -> Result<bool, EngineError> {
        if r.inputs().is_empty() {
            return Ok(true);
        }
        let licences = self.licences(read)?;
        self.entitled_with(f, &licences, r)
    }

    /// The licences of a fresh slice read's members, as the licence rule reads them (a scoped
    /// member counts with the mesh's own licence).
    fn slice_licences(&self, s: &SliceRead) -> Result<Vec<String>, EngineError> {
        if s.computed {
            return Ok(s.member_ans.iter().map(|a| a.licence.clone()).collect());
        }
        let mut out = Vec::with_capacity(s.members.len());
        for m in &s.members {
            if m.scoped {
                out.push(self.eng.licence.clone());
                continue;
            }
            let Some(p) = self.receipt(&m.hash)? else {
                return Err(StoreError::new(
                    "receipt",
                    datumesh_store::Error::NotFound { what: "receipt", key: m.hash.clone() },
                )
                .into());
            };
            out.push(p.receipt.licence().to_string());
        }
        Ok(out)
    }

    /// Whether the request writes nothing but slice definitions: a read as of a past time when
    /// the engine does not store those (`store_past_reads`), or a scenario.
    pub(crate) fn stores_nothing(&self) -> bool {
        self.scope.is_some()
            || (!self.current && self.asof.as_str() < self.issued.as_str() && !self.eng.store_past_reads)
    }

    /// Writes the request's new manifests, receipts and computed versions: manifests and
    /// receipts first (inputs before the receipts reading them), then the versions.
    pub(crate) fn flush(&mut self) -> Result<(), EngineError> {
        if self.scope.is_some() {
            self.fresh.clear();
            self.slice_reads.clear();
            return Ok(());
        }
        // A slice's definition is kept once per distinct slice, a read as of a past time
        // included (CONTRACT-guard-patch G10); nothing else is stored by a past read when the
        // engine does not store those (`store_past_reads`, G10 and the mesh's setting).
        for s in self.slice_reads.values() {
            self.eng.store.put_slice_definition(&s.slice).map_err(|e| StoreError::new("put_slice_definition", e))?;
        }
        if self.stores_nothing() {
            self.fresh.clear();
            self.slice_reads.clear();
            return Ok(());
        }
        // Nothing is written when a receipt would be (SPEC 9.9): the store refuses the receipts
        // under a suspended namespace, and the manifests (not refused) are not written before.
        if !self.fresh.is_empty() {
            if let Some(ns) = self.eng.store.namespace(&self.eng.host).map_err(|e| StoreError::new("namespace", e))? {
                if let Some(since) = ns.suspended_at {
                    return Err(EngineError::NamespaceSuspended { host: self.eng.host.clone(), suspended_at: since });
                }
            }
        }
        // The member-set row of every slice read (G2), and its manifest under the policy (G1).
        let reads: Vec<Arc<SliceRead>> = self.slice_reads.values().cloned().collect();
        for s in reads {
            let Some(set) = &s.set else { continue };
            let stored = match self.eng.manifests {
                ManifestPolicy::Store => true,
                ManifestPolicy::MemberSetOnly => false,
                ManifestPolicy::StoreUpTo(n) => set.members().len() <= n,
            };
            if stored {
                self.eng.store.put_manifest(&s.manifest).map_err(|e| StoreError::new("put_manifest", e))?;
            }
            // The members' distinct licences (G3): from their receipts, as the rule reads them.
            let mut row = datumesh_store::MemberSet::from_manifest(set, s.tx.as_deref(), &s.asof);
            row.licences = self.slice_licences(&s)?;
            row.seen_seq = s.seen_seq;
            self.eng.store.put_member_set(&row).map_err(|e| StoreError::new("put_member_set", e))?;
        }
        for f in &self.fresh {
            self.eng.store.put_receipt(&f.bytes).map_err(|e| StoreError::new("put_receipt", e))?;
        }
        for f in self.fresh.iter().filter(|f| f.computed) {
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
        self.slice_reads.clear();
        Ok(())
    }
}

/// Applies a scenario to the members of a stored slice read as of `asof`: the removed members
/// go, the value overrides of members apply, and the added members come (sorted by cell id).
fn apply_scope(scope: &Scope, slice: &Slice, asof: &str, members: &mut Vec<MemberRec>) {
    let ov = scope.slices.get(slice.id());
    if let Some(list) = ov.and_then(|o| o.members.as_ref()) {
        // A members override replaces the whole member set: the stored members are left out.
        members.clear();
        members.extend(list.iter().map(|(c, v)| override_member(c, v, asof, scope)));
        members.sort_by(|a, b| a.id.cmp(&b.id));
        return;
    }
    if let Some(o) = ov {
        members.retain(|m| !o.remove.contains(&m.id));
    }
    for m in members.iter_mut() {
        if let Some((_, v)) = scope.values.get(&m.id) {
            m.value = v.clone();
            m.hash = scoped_hash(&m.id, v);
            m.scoped = true;
        }
    }
    if let Some(o) = ov {
        for (c, v) in &o.add {
            members.push(override_member(c, v, asof, scope));
        }
    }
    members.sort_by(|a, b| a.id.cmp(&b.id));
}

/// An overridden member of a slice, as the scenario reads it (its value in the scope).
fn override_member(c: &Cell, v: &Scalar, asof: &str, scope: &Scope) -> MemberRec {
    let id = c.id();
    let value = scope.values.get(&id).map_or_else(|| v.clone(), |x| x.1.clone());
    MemberRec {
        hash: scoped_hash(&id, &value),
        id,
        value,
        args: c.args.clone(),
        tx: asof.to_string(),
        asof: asof.to_string(),
        recorded: asof.to_string(),
        scoped: true,
    }
}

/// A cell's canonical call, as a cycle's path names it (SPEC 5.8): `<ns>/<fn>(<label>: <literal>,
/// ...)`, labels ascending, strings quoted with `"` doubled; no arguments: `()`.
pub(crate) fn canonical_call(cell: &Cell) -> String {
    let args: Vec<String> = cell
        .args
        .iter()
        .map(|(k, a)| {
            let literal = match a {
                Arg::Value(Scalar::String(s)) => format!("\"{}\"", s.replace('"', "\"\"")),
                Arg::Value(v) => v.lexical().to_string(),
                Arg::Slice => "*".to_string(),
            };
            format!("{k}: {literal}")
        })
        .collect();
    format!("{}/{}({})", cell.ns, cell.fn_name, args.join(", "))
}

/// An evaluation error from the engine's own lookups, as an evaluation sees it.
fn eng_eval(e: EngineError) -> EvalError {
    match e {
        EngineError::Store(e) => EvalError::Store(e),
        EngineError::Invalid(e) => EvalError::InvalidInput(e),
        other => EvalError::Runtime(other.to_string()),
    }
}

/// What running a root's frames ended with.
enum Ended {
    /// The root has its answer.
    Done,
    /// The root's stack would pass the depth limit at this pending cell: it runs first.
    Split(Pend),
}

/// Whether the members `old` (cell id, value) are the cells of `now` with the same values.
fn same_members(old: &[(String, Scalar)], now: &SliceRead) -> bool {
    if old.len() != now.members.len() {
        return false;
    }
    let mut a: Vec<(&str, &Scalar)> = old.iter().map(|(i, v)| (i.as_str(), v)).collect();
    let mut b: Vec<(&str, &Scalar)> = now.members.iter().map(|m| (m.id.as_str(), &m.value)).collect();
    a.sort_by(|x, y| x.0.cmp(y.0));
    b.sort_by(|x, y| x.0.cmp(y.0));
    a == b
}

/// Why a frame's runtime cannot finish this run, recorded at the first read that says so; it
/// wins over whatever the runtime returns.
enum Stop {
    /// Inputs to evaluate first.
    Pending(Vec<Pend>),
    /// An input with no value: neither has this cell.
    NotCovered,
    Fatal(EvalError),
}

/// The [`Inputs`] of one run of a frame's runtime.
struct Reader<'r, 'e> {
    req: &'r mut Req<'e>,
    asof: String,
    desc: &'r FunctionDescriptor,
    budget: Budget,
    /// What was read, by entry (and the transaction time of a cell read at one).
    read: BTreeMap<(String, Option<String>), Rd>,
    /// The names relied on, with their hashes.
    names: Names,
    stop: Option<Stop>,
    /// A read returned a fault (the runtime may have caught it), and since when it holds.
    faulted: bool,
    fault_known: String,
    /// The latest instant `asof_compare` found at or before the asof: an asof candidate.
    compared: String,
    /// A time after the asof was established (`E_FORMULA_FUTURE` may be reported).
    future_seen: bool,
    /// A cell was read at a transaction time.
    at_read: bool,
    /// The asof was read.
    asof_read: bool,
    /// The faulted cells read, whose faults the answer rests on.
    fault_deps: Vec<Arc<FaultAns>>,
    /// A fault rests on something that is not a cell's fault (an absent slice member, which
    /// has no memo entry of its own): not memoised.
    unmemo: bool,
}

impl Reader<'_, '_> {
    /// A read returned a fault that holds from `known` on: the faulted cell `dep`, or none
    /// when the fault is not a cell's (then the answer is not memoised).
    fn fault(&mut self, known: &str, dep: Option<Arc<FaultAns>>) {
        self.faulted = true;
        if known > self.fault_known.as_str() {
            self.fault_known = known.to_string();
        }
        match dep {
            Some(d) => self.fault_deps.push(d),
            None => self.unmemo = true,
        }
    }

    /// A read returned `E_FORMULA_FUTURE`: a fault that depends on the asof itself.
    fn future(&mut self) -> EvalError {
        self.faulted = true;
        self.asof_read = true;
        self.future_seen = true;
        EvalError::Fault(Fault::Future)
    }

    fn declared(&self, fn_name: &str) -> Result<(), EvalError> {
        if !self.desc.inputs.iter().any(|i| i == fn_name) || !fn_name.contains('/') {
            return Err(EvalError::UndeclaredInput { fn_name: fn_name.to_string() });
        }
        Ok(())
    }

    /// The descriptor stored under `fn_hash`: a hash with none is a store failure.
    fn descriptor_for(&self, fn_hash: &str) -> Result<Arc<FunctionDescriptor>, EvalError> {
        match self.req.eng.descriptor_of(fn_hash) {
            Ok(Some(d)) => Ok(d),
            Ok(None) => Err(EvalError::Store(StoreError::new(
                "descriptor",
                datumesh_store::Error::NotFound { what: "descriptor", key: fn_hash.to_string() },
            ))),
            Err(EngineError::Store(e)) => Err(EvalError::Store(e)),
            Err(EngineError::Invalid(e)) => Err(EvalError::InvalidInput(e)),
            Err(e) => Err(EvalError::Runtime(e.to_string())),
        }
    }

    /// The function `fn_name` means as of `at`, recorded as relied on (`key_at`: the
    /// transaction time it is read at, none for the evaluation's asof).
    fn function(
        &mut self,
        fn_name: &str,
        at: &str,
        key_at: Option<&str>,
    ) -> Result<(String, Arc<FunctionDescriptor>), EvalError> {
        let Some(fn_hash) = self.req.resolve(fn_name, at)? else {
            return Err(EvalError::NameUnresolved { fn_name: fn_name.to_string(), asof: at.to_string() });
        };
        self.names.insert((fn_name.to_string(), key_at.map(str::to_string)), fn_hash.clone());
        let desc = self.descriptor_for(&fn_hash)?;
        Ok((fn_hash, desc))
    }

    /// A single-cell read as of the evaluation's asof, or as of `at`.
    fn read_at(&mut self, fn_name: &str, args: &Args, at: Option<&str>) -> Result<Input, EvalError> {
        self.budget.charge_shared(1)?;
        self.declared(fn_name)?;
        let when = at.unwrap_or(&self.asof).to_string();
        let (fn_hash, desc) = self.function(fn_name, &when, at)?;
        let cell = cell_named(fn_name, fn_hash, args);
        self.cell_read(cell, desc, at)
    }

    /// A pinned read (SPEC 16.3 `super`): the cell of `fn_name` at `fn_hash`, with the
    /// descriptor stored under that hash, as of the evaluation's asof. The name is not resolved
    /// and no binding is recorded: the receipt is an input as any read's is.
    fn read_pinned_at(&mut self, fn_name: &str, fn_hash: &str, args: &Args) -> Result<Input, EvalError> {
        self.budget.charge_shared(1)?;
        self.declared(fn_name)?;
        let desc = self.descriptor_for(fn_hash)?;
        let cell = cell_named(fn_name, fn_hash.to_string(), args);
        self.cell_read(cell, desc, None)
    }

    /// Reads `cell` (its descriptor `desc`) as of the evaluation's asof, or as of `at`: a value
    /// is an input, a fault is returned (and recorded), a cell with no value or one to evaluate
    /// stops the run.
    fn cell_read(&mut self, cell: Cell, desc: Arc<FunctionDescriptor>, at: Option<&str>) -> Result<Input, EvalError> {
        let when = at.unwrap_or(&self.asof).to_string();
        if let Some((k, _)) = cell.args.iter().find(|(_, a)| a.scalar().is_none()) {
            return Err(EvalError::InvalidInput(CoreError::new(
                ErrorCode::Slice,
                format!("$.args.{k}"),
                "an input is a cell, not a slice",
            )));
        }
        check_args(&cell, &desc).map_err(EvalError::InvalidInput)?;
        let id = cell.id();
        if at.is_some() {
            self.at_read = true;
        }
        match self.req.lookup(&cell, &id, desc, &when)? {
            Lookup::Ready(Done::Value(a)) => {
                let input = Input::new(a.value.clone(), a.hash.clone(), a.tx.clone(), a.asof.clone());
                let at = at.map(str::to_string);
                self.read.insert((a.hash.clone(), at.clone()), Rd::Cell { ans: a, at });
                Ok(input)
            }
            Lookup::Ready(Done::Fault(fa)) => {
                let known = fa.known.clone();
                self.fault(&known, Some(fa.clone()));
                Err(EvalError::Fault(fa.fault))
            }
            Lookup::Ready(Done::NotCovered | Done::Unsigned(..) | Done::Withdrawn(_)) => {
                self.stop = Some(Stop::NotCovered);
                Err(EvalError::InputNotCovered { cell_id: id })
            }
            Lookup::Eval(desc) => {
                self.stop = Some(Stop::Pending(vec![Pend { cell, id, desc, asof: when }]));
                Err(EvalError::Suspended)
            }
        }
    }

    fn slice_read(
        &mut self,
        fn_name: &str,
        slice: Arc<Slice>,
        desc: Arc<FunctionDescriptor>,
        spec: Spec,
    ) -> Result<Arc<SliceRead>, EvalError> {
        let asof = self.asof.clone();
        match self.req.slice_at(fn_name, slice, desc, spec, &asof)? {
            SliceOutcome::Ready(s) => {
                // 1 + members, once the members are obtained.
                self.budget.charge_shared(1 + s.members.len() as u64)?;
                self.read.insert((s.entry.clone(), None), Rd::Slice(s.clone()));
                Ok(s)
            }
            SliceOutcome::Pending(list) => {
                self.stop = Some(Stop::Pending(list));
                Err(EvalError::Suspended)
            }
            SliceOutcome::Absent(known) => {
                self.fault(&known, None);
                Err(EvalError::Fault(Fault::Absent))
            }
        }
    }

    /// Records the first error that must stop the run.
    fn guard<T>(&mut self, r: Result<T, EvalError>) -> Result<T, EvalError> {
        if let Err(e) = &r {
            if !matches!(e, EvalError::Suspended | EvalError::InputNotCovered { .. } | EvalError::Fault(_)) {
                self.stop = Some(Stop::Fatal(e.clone()));
            }
        }
        r
    }

    /// The error of a read made after the run had to stop.
    fn stopped(&self) -> Option<EvalError> {
        match &self.stop {
            Some(Stop::Pending(..)) => Some(EvalError::Suspended),
            Some(Stop::NotCovered) => Some(EvalError::InputNotCovered { cell_id: String::new() }),
            Some(Stop::Fatal(e)) => Some(e.clone()),
            None => None,
        }
    }
}

fn instant_arg(s: &str, what: &str) -> Result<(), EvalError> {
    lexical::check_instant(s, what).map_err(EvalError::InvalidInput)
}

impl Inputs for Reader<'_, '_> {
    fn read(&mut self, fn_name: &str, args: &Args) -> Result<Scalar, EvalError> {
        self.read_cell(fn_name, args).map(|i| i.value)
    }

    fn read_cell(&mut self, fn_name: &str, args: &Args) -> Result<Input, EvalError> {
        if let Some(e) = self.stopped() {
            return Err(e);
        }
        let r = self.read_at(fn_name, args, None);
        self.guard(r)
    }

    fn read_asof(&mut self, fn_name: &str, args: &Args, t: &str) -> Result<Input, EvalError> {
        // The time first, even after the run had to stop (SPEC 15.13).
        if let Some(e) = self.stopped().filter(|_| is_instant(t) && t <= self.asof.as_str()) {
            return Err(e);
        }
        let r = (|| {
            instant_arg(t, "$.t")?;
            if t > self.asof.as_str() {
                return Err(self.future());
            }
            self.read_at(fn_name, args, Some(t))
        })();
        self.guard(r)
    }

    fn read_pinned(&mut self, fn_name: &str, fn_hash: &str, args: &Args) -> Result<Input, EvalError> {
        if let Some(e) = self.stopped() {
            return Err(e);
        }
        let r = self.read_pinned_at(fn_name, fn_hash, args);
        self.guard(r)
    }

    fn read_slice(&mut self, fn_name: &str, pattern: &SlicePattern) -> Result<Vec<Member>, EvalError> {
        if let Some(e) = self.stopped() {
            return Err(e);
        }
        let r = (|| {
            self.declared(fn_name)?;
            let asof = self.asof.clone();
            let (fn_hash, desc) = self.function(fn_name, &asof, None)?;
            let slice = cells_slice(fn_name, &fn_hash, &desc, pattern).map_err(EvalError::InvalidInput)?;
            let s = self.slice_read(fn_name, Arc::new(slice), desc, Spec::Cells { pattern: pattern.clone() })?;
            Ok(s.members.iter().map(|m| Member::new(m.args.clone(), m.input())).collect())
        })();
        self.guard(r)
    }

    fn read_versions(&mut self, fn_name: &str, args: &Args, from: &str, to: &str) -> Result<Vec<Input>, EvalError> {
        // The times first, even after the run had to stop (SPEC 15.13).
        let times_ok = is_instant(from) && is_instant(to) && to <= self.asof.as_str();
        if let Some(e) = self.stopped().filter(|_| times_ok) {
            return Err(e);
        }
        let r = (|| {
            instant_arg(from, "$.from")?;
            instant_arg(to, "$.to")?;
            if to > self.asof.as_str() {
                return Err(self.future());
            }
            self.declared(fn_name)?;
            let asof = self.asof.clone();
            let (fn_hash, desc) = self.function(fn_name, &asof, None)?;
            let (ns, f) = fn_name.rsplit_once('/').unwrap_or(("", fn_name));
            let cell = Cell { ns: ns.to_string(), fn_name: f.to_string(), fn_hash, args: args.clone() };
            check_args(&cell, &desc).map_err(EvalError::InvalidInput)?;
            if desc.lang != STORED {
                return Err(EvalError::InvalidInput(CoreError::new(
                    ErrorCode::Slice,
                    "$",
                    "versions() of a computed function: versions are of stored cells",
                )));
            }
            let slice = versions_slice(&cell, from, to).map_err(EvalError::InvalidInput)?;
            let spec = Spec::Versions { to: to.to_string() };
            let s = self.slice_read(fn_name, Arc::new(slice), desc, spec)?;
            Ok(s.members.iter().map(MemberRec::input).collect())
        })();
        self.guard(r)
    }

    fn binding(&mut self, fn_name: &str, at: Option<&str>) -> Result<Option<FnBinding>, EvalError> {
        if let Some(e) = self.stopped() {
            return Err(e);
        }
        let r = (|| {
            self.declared(fn_name)?;
            if let Some(t) = at {
                instant_arg(t, "$.at")?;
            }
            let when = at.unwrap_or(&self.asof).to_string();
            let Some((fn_hash, start)) = self.req.binding(fn_name, &when)? else { return Ok(None) };
            let (_, descriptor) = self.function(fn_name, &when, at)?;
            Ok(Some(FnBinding { fn_hash, start, descriptor }))
        })();
        self.guard(r)
    }

    fn asof(&mut self) -> String {
        self.asof_read = true;
        self.asof.clone()
    }

    fn binding_peek(&mut self, fn_name: &str, at: Option<&str>) -> Result<Option<FnBinding>, EvalError> {
        if let Some(e) = self.stopped() {
            return Err(e);
        }
        let r = (|| {
            self.declared(fn_name)?;
            if let Some(t) = at {
                instant_arg(t, "$.at")?;
            }
            let when = at.unwrap_or(&self.asof).to_string();
            let Some((fn_hash, start)) = self.req.binding(fn_name, &when)? else { return Ok(None) };
            let descriptor = match self.req.eng.descriptor_of(&fn_hash) {
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
            Ok(Some(FnBinding::new(fn_hash, start, descriptor)))
        })();
        self.guard(r)
    }

    fn read_blob(&mut self, sha256: &str, length: u64) -> Result<Vec<u8>, EvalError> {
        if let Some(e) = self.stopped() {
            return Err(e);
        }
        let r = (|| {
            lexical::check_hex(sha256, 64, "$.sha256").map_err(EvalError::InvalidInput)?;
            let unavailable = |reason: String| EvalError::BlobUnavailable { sha256: sha256.to_string(), reason };
            let Some(bytes) = self.req.eng.store.blob(sha256).map_err(store_err("blob"))? else {
                return Err(unavailable("the store has no content for it".into()));
            };
            if bytes.len() as u64 != length {
                return Err(unavailable(format!("the content has {} bytes, the blob {length}", bytes.len())));
            }
            if datumesh_core::sha256_hex(&bytes) != sha256 {
                return Err(unavailable("the content has another SHA-256".into()));
            }
            Ok(bytes.into_owned())
        })();
        self.guard(r)
    }

    fn asof_compare(&mut self, t: &str) -> Result<std::cmp::Ordering, EvalError> {
        instant_arg(t, "$.t")?;
        let ord = t.cmp(self.asof.as_str());
        if ord == std::cmp::Ordering::Greater {
            // Changes once the asof reaches t: asof-dependent.
            self.asof_read = true;
            self.future_seen = true;
        } else if t > self.compared.as_str() {
            self.compared = t.to_string();
        }
        Ok(ord)
    }
}

/// The cell `fn_name` (`ns/name`) at `fn_hash`, with `args`.
fn cell_named(fn_name: &str, fn_hash: String, args: &Args) -> Cell {
    let (ns, f) = fn_name.rsplit_once('/').unwrap_or(("", fn_name));
    Cell { ns: ns.to_string(), fn_name: f.to_string(), fn_hash, args: args.clone() }
}

/// The request's error for an exhausted budget: its fuel, or its work.
pub(crate) fn exhausted(b: &Budget) -> EngineError {
    match b.exhausted() {
        crate::runtime::Exhausted::Work => EngineError::WorkExceeded { limit: b.work_limit() },
        _ => EngineError::BudgetExceeded { fuel: b.limit() },
    }
}

/// An evaluation error as the request's error, for the cell being evaluated.
pub(crate) fn engine_error(e: EvalError, cell_id: &str) -> EngineError {
    match e {
        EvalError::NoRuntime { lang } => EngineError::NoRuntime { lang, fn_hash: String::new() },
        EvalError::UndeclaredInput { fn_name } => EngineError::UndeclaredInput { fn_name },
        EvalError::Cycle { path } => EngineError::Cycle { path },
        EvalError::DepthExceeded { limit } => EngineError::DepthExceeded { limit },
        EvalError::BudgetExceeded { fuel } => EngineError::BudgetExceeded { fuel },
        EvalError::NameUnresolved { fn_name, asof } => EngineError::NameUnresolved { fn_name, asof },
        EvalError::Store(e) => e.into(),
        EvalError::InvalidInput(e) => EngineError::Invalid(e),
        EvalError::BlobUnavailable { sha256, reason } => EngineError::BlobUnavailable { sha256, reason },
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

/// Parses a withdrawal the store returned (its canonical bytes): its reason and canonical
/// bytes, as the store gives them (`Withdrawal`, never a `Receipt`).
pub(crate) fn parse_withdrawal(hash: &str, bytes: &[u8]) -> Result<Withdrawn, StoreError> {
    match Withdrawal::from_json(bytes) {
        Ok(w) if w.hash() == hash => {
            Ok(Withdrawn { bytes: Arc::new(w.canonical().to_vec()), reason: w.withdrawn().to_string() })
        }
        Ok(_) | Err(_) => Err(StoreError::new(
            "receipt",
            datumesh_store::Error::Invalid(CoreError::new(
                ErrorCode::InputInvalid,
                "$",
                format!("the store's withdrawal {hash} does not parse to that hash"),
            )),
        )),
    }
}

/// Whether `s` is an instant (for the asof of `Resolver::evaluate`).
pub(crate) fn is_instant(s: &str) -> bool {
    lexical::check_instant(s, "$").is_ok()
}
