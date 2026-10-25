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
    lexical, Args, Cell, Error as CoreError, ErrorCode, Fault, FunctionDescriptor, Manifest, Receipt, ReceiptBuilder,
    Scalar, Slice,
};

use crate::cache::Parsed;
use crate::check::{check_args, valid_args};
use crate::engine::{Engine, STORED};
use crate::error::{EngineError, EvalError, StoreError};
use crate::runtime::{Budget, FnBinding, Input, Inputs, Member, SlicePattern};
use crate::sign::receipt_json;
use crate::slice::{cells_slice, extent_cells, spec_of, versions_slice, Spec};

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

impl Ans {
    /// The ids of the cells read whose changes reach this answer through the dependents
    /// index: single cells and the members of slices over computed functions. (A slice over a
    /// stored function is matched by its pattern instead: [`Ans::patterns`].)
    pub(crate) fn deps(&self) -> impl Iterator<Item = &String> {
        self.inputs.iter().flat_map(|u| -> Box<dyn Iterator<Item = &String> + '_> {
            match u {
                Use::Cell { id, .. } => Box::new(std::iter::once(id)),
                Use::Slice(s) if s.computed => Box::new(s.members.iter().map(|m| &m.id)),
                Use::Slice(_) => Box::new(std::iter::empty()),
            }
        })
    }

    /// The slices over stored functions (and version slices) it read.
    pub(crate) fn patterns(&self) -> impl Iterator<Item = &Arc<Slice>> {
        self.inputs.iter().filter_map(|u| match u {
            Use::Slice(s) if !s.computed => Some(&s.slice),
            _ => None,
        })
    }
}

/// A slice read as of an asof: its entry and manifest, and its members.
#[derive(Debug)]
pub(crate) struct SliceRead {
    pub(crate) entry: String,
    pub(crate) manifest: Arc<Vec<u8>>,
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
    /// The latest member tx.
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
    /// No value: the evaluation faulted; the fault holds from the instant given (the latest
    /// asof of what the evaluation read, and of the bindings it relied on).
    Fault(Fault, String),
    /// The top cell evaluated without a receipt (`Resolver::evaluation`), with the entries
    /// it read.
    Unsigned(Scalar, Vec<String>),
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
    /// Names resolved as of an instant: the function hash and the start of the binding.
    names: HashMap<(String, String), Option<(String, String)>>,
    /// Cells answered, by (asof, cell id).
    done: HashMap<(String, String), Done>,
    /// Slices read, by (asof, slice id).
    slices: HashMap<(String, String), Arc<SliceRead>>,
    fresh: Vec<Fresh>,
    /// Manifests of the slices runtimes read, by entry.
    manifests: BTreeMap<String, Arc<Vec<u8>>>,
    /// The answers this request made (not taken from the memo), inputs first: for the memo.
    pub(crate) learned: Vec<Arc<Ans>>,
    budget: Budget,
}

/// How an input of a version compares as of the asof.
enum Check {
    Same(Rd, (String, Option<String>), String),
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
    /// The value rests on a fault a read returned, which holds from this instant.
    known: Option<String>,
}

fn store_err(op: &'static str) -> impl Fn(datumesh_store::Error) -> EvalError {
    move |e| EvalError::Store(StoreError::new(op, e))
}

impl<'e> Req<'e> {
    pub(crate) fn new(eng: &'e Engine, asof: &str, issued: &str) -> Req<'e> {
        Req {
            eng,
            asof: asof.to_string(),
            issued: issued.to_string(),
            names: HashMap::new(),
            done: HashMap::new(),
            slices: HashMap::new(),
            fresh: Vec::new(),
            manifests: BTreeMap::new(),
            learned: Vec::new(),
            budget: Budget::with_work(eng.fuel, eng.work_limit),
        }
    }

    /// The binding `fn_name` has as of `at`, its function hash and when it began (one query
    /// per name and instant per request).
    fn binding(&mut self, fn_name: &str, at: &str) -> Result<Option<(String, String)>, EvalError> {
        let key = (fn_name.to_string(), at.to_string());
        if let Some(b) = self.names.get(&key) {
            return Ok(b.clone());
        }
        let b = self
            .eng
            .store
            .resolve_binding(fn_name, at)
            .map_err(store_err("resolve_binding"))?
            .map(|b| (b.fn_hash, b.tx));
        self.names.insert(key, b.clone());
        Ok(b)
    }

    /// The function hash `fn_name` resolves to as of `at`.
    fn resolve(&mut self, fn_name: &str, at: &str) -> Result<Option<String>, EvalError> {
        Ok(self.binding(fn_name, at)?.map(|(h, _)| h))
    }

    /// Whether every name of `names` still resolves as recorded, as of `asof` (or its own
    /// transaction time).
    fn names_hold(&mut self, names: &Names, asof: &str) -> Result<bool, EvalError> {
        for ((name, at), hash) in names {
            if self.resolve(name, at.as_deref().unwrap_or(asof))?.as_deref() != Some(hash.as_str()) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn done_at(&self, asof: &str, id: &str) -> Option<&Done> {
        self.done.get(&(asof.to_string(), id.to_string()))
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
        if let Some(a) = self.memo_hit(id, asof)? {
            let d = Done::Value(a);
            self.done.insert((asof.to_string(), id.to_string()), d.clone());
            return Ok(Lookup::Ready(d));
        }
        if desc.lang == STORED {
            let d = self.stored(cell, id, asof)?;
            self.done.insert((asof.to_string(), id.to_string()), d.clone());
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

    /// The request's asof.
    pub(crate) fn asof(&self) -> String {
        self.asof.clone()
    }

    /// A stored cell's answer: the version answering as of `asof`.
    pub(crate) fn stored(&mut self, cell: &Cell, id: &str, asof: &str) -> Result<Done, EvalError> {
        let found = self.eng.store.stored_asof_with_receipt(id, asof).map_err(store_err("stored_asof"))?;
        let Some((v, bytes)) = found else { return Ok(Done::NotCovered) };
        let p = self.eng.parsed(&v.receipt_hash, &bytes).map_err(EvalError::Store)?;
        // The version answers from its asof (never earlier than its tx) until a version later
        // by tx answers: from that one's asof. Only the memo needs to know.
        let hi = if self.eng.memo.enabled() {
            let later = self.eng.store.versions(id, &v.tx, MAX_INSTANT).map_err(store_err("versions"))?;
            later.into_iter().filter(|w| w.tx > v.tx).map(|w| w.asof).min()
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
            lo: r.asof().to_string(),
            hi,
            names: Arc::new(Names::new()),
            cell: cell.clone(),
            inputs: Vec::new(),
            memo: true,
        });
        self.learned.push(a.clone());
        Ok(Done::Value(a))
    }

    /// Answers `cell` (a computed cell with descriptor `desc`) as of the request's asof.
    pub(crate) fn run(
        &mut self,
        cell: Cell,
        desc: Arc<FunctionDescriptor>,
        unsigned: bool,
    ) -> Result<Done, EngineError> {
        let id = cell.id();
        let asof = self.asof.clone();
        let state = if unsigned { State::Eval { cand: None } } else { State::Start };
        let key = |asof: &str, id: &str| format!("{asof} {id}");
        let mut stack = vec![Frame {
            cell,
            id: id.clone(),
            asof: asof.clone(),
            desc,
            state,
            unsigned,
            pending: Vec::new(),
            floor: 0,
        }];
        let mut on_stack: HashSet<String> = HashSet::from([key(&asof, &id)]);
        while let Some(top) = stack.last_mut() {
            if let Some(p) = top.pending.pop() {
                if self.done_at(&p.asof, &p.id).is_some() {
                    continue;
                }
                if !on_stack.insert(key(&p.asof, &p.id)) {
                    return Err(EngineError::Cycle { cell_id: p.id });
                }
                if stack.len() >= self.eng.depth_limit {
                    return Err(EngineError::DepthExceeded { limit: self.eng.depth_limit });
                }
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
                State::Start => self.start_frame(top),
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
                        if let Done::Value(a) = &d {
                            self.learned.push(a.clone());
                        }
                        self.done.insert((f.asof, f.id), d);
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
        self.done_at(&asof, &id)
            .cloned()
            .ok_or_else(|| EngineError::Runtime { cell_id: id, message: "no answer".into() })
    }

    /// A dropped answer to recheck, if the memo has one holding at the asof.
    fn start_frame(&mut self, f: &mut Frame) -> Step {
        f.state = match self.eng.memo.stale(&f.id, &f.asof) {
            Some(ans) => State::Recheck { ans, idx: 0, read: Vec::new(), names: Names::new() },
            None => State::FromStore,
        };
        Step::Again
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
                    self.check_use(cell, id, hash, value, at.as_deref(), &asof)?
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
                    names.insert(name, hash);
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
        let facts = self.facts(&read, names);
        Ok(Step::Done(Done::Value(self.kept(f, facts, &ans.hash, &ans.bytes, &ans.value, &ans.tx, &ans.asof))))
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
        tx: &str,
        asof: &str,
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
        self.eng.runtimes.get(&f.desc.lang).is_some_and(|rt| rt.asof_dependent(&f.desc))
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
            let check = if h.starts_with("slice:") { self.check_entry(h, &asof)? } else { self.check_input(h, &asof)? };
            match check {
                Check::Same(rd, name, hash) => {
                    read.push(rd);
                    names.insert(name, hash);
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
        let facts = self.facts(&read, names);
        let r = &cand.receipt;
        Ok(Step::Done(Done::Value(self.kept(f, facts, r.hash(), &cand.bytes, r.value(), r.tx(), r.asof()))))
    }

    /// Whether input receipt `h` of a version still answers as of the asof.
    fn check_input(&mut self, h: &str, asof: &str) -> Result<Check, EngineError> {
        let Some(input) = self.receipt(h)? else { return Ok(Check::Differs) };
        let r = &input.receipt;
        self.check_use(r.cell(), &r.cell().id(), h, r.value(), None, asof)
    }

    /// Whether `cell`, its name resolving to its function, gives receipt `h` or value `value`
    /// as of `at` (a transaction-time read), else as of `asof`.
    fn check_use(
        &mut self,
        cell: &Cell,
        id: &str,
        h: &str,
        value: &Scalar,
        at: Option<&str>,
        asof: &str,
    ) -> Result<Check, EngineError> {
        let when = at.unwrap_or(asof).to_string();
        let name = cell.function_name();
        if self.resolve(&name, &when).map_err(|e| engine_error(e, id))?.as_deref() != Some(cell.fn_hash.as_str()) {
            return Ok(Check::Differs);
        }
        self.budget.work(1).map_err(|e| engine_error(e, id))?;
        let Some(desc) = self.eng.descriptor_of(&cell.fn_hash)? else { return Ok(Check::Differs) };
        match self.lookup(cell, id, desc, &when).map_err(|e| engine_error(e, id))? {
            Lookup::Ready(Done::Value(a)) if a.hash == h || &a.value == value => Ok(Check::Same(
                Rd::Cell { ans: a, at: at.map(str::to_string) },
                (name, at.map(str::to_string)),
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
        let Some(bytes) = self.eng.store.manifest(entry) else { return Ok(Check::Differs) };
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
        if self.resolve(&name, asof).map_err(fail)?.as_deref() != Some(slice.fn_hash()) {
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
        Ok(if same { Check::Same(Rd::Slice(now), (name, None), fn_hash) } else { Check::Differs })
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
        match (&spec, computed) {
            (Spec::Cells { pattern }, true) => {
                let cells = extent_cells(fn_name, &fn_hash, &desc, pattern).map_err(EvalError::InvalidInput)?;
                let mut pending = Vec::new();
                let mut absent: Option<String> = None;
                for cell in cells {
                    let id = cell.id();
                    match self.lookup(&cell, &id, desc.clone(), asof)? {
                        Lookup::Ready(Done::Value(a)) => member_ans.push(a),
                        Lookup::Ready(Done::Fault(_, known)) => {
                            absent = Some(absent.map_or(known.clone(), |k| k.max(known)));
                        }
                        Lookup::Ready(_) => {
                            absent.get_or_insert_with(|| MIN_INSTANT.to_string());
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
            (Spec::Cells { .. }, false) => {
                let found = self.eng.store.slice_members(&slice, asof).map_err(store_err("slice_members"))?;
                for m in &found {
                    members.push(self.member(&m.version.receipt_hash, &m.receipt)?);
                }
                // Holding past the asof: when nothing answers later (the members as of the end
                // of time are these), else at the asof only. Versions stored later are the
                // dirty set's.
                hi = if self.eng.memo.enabled() {
                    let later =
                        self.eng.store.slice_members(&slice, MAX_INSTANT).map_err(store_err("slice_members"))?;
                    let same = later.len() == found.len()
                        && later.iter().zip(&found).all(|(a, b)| a.version.receipt_hash == b.version.receipt_hash);
                    if same {
                        None
                    } else {
                        Some(just_after(asof))
                    }
                } else {
                    None
                };
                lo = members.iter().map(|m| m.asof.clone()).max();
                memo = true;
            }
            (Spec::Versions { cell, from, to }, false) => {
                let id = cell.id();
                let found =
                    self.eng.store.versions_between(&id, from, to, asof).map_err(store_err("versions_between"))?;
                for m in &found {
                    members.push(self.member(&m.version.receipt_hash, &m.receipt)?);
                }
                hi = if self.eng.memo.enabled() {
                    let later = self
                        .eng
                        .store
                        .versions_between(&id, from, to, MAX_INSTANT)
                        .map_err(store_err("versions_between"))?;
                    let same = later.len() == found.len()
                        && later.iter().zip(&found).all(|(a, b)| a.version.receipt_hash == b.version.receipt_hash);
                    if same {
                        None
                    } else {
                        Some(just_after(asof))
                    }
                } else {
                    None
                };
                lo = members.iter().map(|m| m.asof.clone()).chain(std::iter::once(to.clone())).max();
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
            tx: members.iter().map(|m| m.tx.clone()).max(),
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

    /// A stored member: its receipt parsed (cached by hash).
    fn member(&self, hash: &str, bytes: &[u8]) -> Result<MemberRec, EvalError> {
        let p = self.eng.parsed(hash, bytes).map_err(EvalError::Store)?;
        let r = &p.receipt;
        Ok(MemberRec {
            id: r.cell().id(),
            hash: hash.to_string(),
            value: r.value().clone(),
            args: r.cell().args.clone(),
            tx: r.tx().to_string(),
            asof: r.asof().to_string(),
        })
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
        let mut names = own;
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
                            for (k, v) in ans.names.iter() {
                                names.entry(k.clone()).or_insert_with(|| v.clone());
                            }
                        }
                        Some(t) => {
                            raise(&mut latest, t);
                            raise(&mut latest, &ans.asof);
                            for ((n, a), v) in ans.names.iter() {
                                let at = a.clone().unwrap_or_else(|| t.clone());
                                names.entry((n.clone(), Some(at))).or_insert_with(|| v.clone());
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
                    names.entry((s.fn_name.clone(), None)).or_insert_with(|| s.fn_hash.clone());
                    for a in &s.member_ans {
                        for (k, v) in a.names.iter() {
                            names.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                    inputs.push(Use::Slice(s.clone()));
                }
            }
        }
        Facts { tx, latest, lo, hi, names: Arc::new(names), inputs, memo, entries }
    }

    /// Runs a frame's runtime once.
    fn eval_frame(&mut self, f: &mut Frame) -> Result<Step, EngineError> {
        let Some(rt) = self.eng.runtimes.get(&f.desc.lang).cloned() else {
            return Err(EngineError::NoRuntime { lang: f.desc.lang.clone(), fn_hash: f.cell.fn_hash.clone() });
        };
        let declared = rt.asof_dependent(&f.desc);
        let mut budget = self.budget.share();
        budget.begin_attempt(f.floor);
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
            at_read: false,
            asof_read: false,
        };
        let result = rt.evaluate(&f.desc, &f.cell, &mut reader, &mut budget);
        let Reader { read, names, stop, faulted, fault_known, at_read, asof_read, .. } = reader;
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
                // Where the fault holds from: as a receipt's asof would be, had it a value.
                let facts = self.facts(&read, names);
                let latest = facts.latest.max(fault_known);
                let known = if declared || asof_read {
                    f.asof.clone()
                } else {
                    self.binding_asof(latest, &facts.names, &f.asof).map_err(fail)?
                };
                return Ok(Step::Done(Done::Fault(code, known)));
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
                    self.manifests.insert(s.entry.clone(), s.manifest.clone());
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
            known: faulted.then_some(fault_known),
        };
        let ans = self.answer_with(f, cand, value, &read, names, how)?;
        Ok(Step::Done(Done::Value(ans)))
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
        let existing = match &cand {
            Some(c) if c.receipt.value() == &value && (!how.dependent || c.receipt.asof() == f.asof) => Some(c.clone()),
            // The new version would share the old one's (tx, asof), which the store's key holds
            // once (an input corrected with a tx earlier than another input's): a receipt
            // signed before for exactly these inputs is reused rather than signing another.
            Some(c) if c.receipt.tx() == tx && c.receipt.asof() == asof => {
                self.signed_before(&f.id, &facts.entries, &value)?
            }
            _ => None,
        };
        if let Some(p) = existing {
            let r = &p.receipt;
            return Ok(Arc::new(Ans {
                id: f.id.clone(),
                hash: r.hash().to_string(),
                bytes: p.bytes.clone(),
                value,
                tx: r.tx().to_string(),
                asof: r.asof().to_string(),
                lo: lo.as_str().max(r.asof()).to_string(),
                hi,
                names: facts.names,
                cell: f.cell.clone(),
                inputs: facts.inputs,
                memo,
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
            .licence(self.eng.licence.clone())
            .summary(summary)
            .sign(&self.eng.key)
            .map_err(EngineError::Sign)?;
        let bytes = Arc::new(receipt_json(&receipt));
        let hash = receipt.hash().to_string();
        for rd in read {
            if let Rd::Slice(s) = rd {
                self.manifests.insert(s.entry.clone(), s.manifest.clone());
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

    /// A stored receipt of cell `id` by this mesh with exactly the inputs `entries` and
    /// `value`.
    fn signed_before(
        &mut self,
        id: &str,
        entries: &BTreeSet<String>,
        value: &Scalar,
    ) -> Result<Option<Arc<Parsed>>, EngineError> {
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
            {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }

    /// Writes the request's new manifests, receipts and computed versions: manifests and
    /// receipts first (inputs before the receipts reading them), then the versions.
    pub(crate) fn flush(&mut self) -> Result<(), EngineError> {
        for m in self.manifests.values() {
            self.eng.store.put_manifest(m).map_err(|e| StoreError::new("put_manifest", e))?;
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
        self.manifests.clear();
        Ok(())
    }
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
    /// A cell was read at a transaction time.
    at_read: bool,
    /// The asof was read.
    asof_read: bool,
}

impl Reader<'_, '_> {
    /// A read returned a fault that holds from `known` on.
    fn fault(&mut self, known: &str) {
        self.faulted = true;
        if known > self.fault_known.as_str() {
            self.fault_known = known.to_string();
        }
    }

    /// A read returned `E_FORMULA_FUTURE`: a fault that depends on the asof itself.
    fn future(&mut self) -> EvalError {
        self.faulted = true;
        self.asof_read = true;
        EvalError::Fault(Fault::Future)
    }

    fn declared(&self, fn_name: &str) -> Result<(), EvalError> {
        if !self.desc.inputs.iter().any(|i| i == fn_name) || !fn_name.contains('/') {
            return Err(EvalError::UndeclaredInput { fn_name: fn_name.to_string() });
        }
        Ok(())
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
        Ok((fn_hash, desc))
    }

    /// A single-cell read as of the evaluation's asof, or as of `at`.
    fn read_at(&mut self, fn_name: &str, args: &Args, at: Option<&str>) -> Result<Input, EvalError> {
        self.budget.charge_shared(1)?;
        self.declared(fn_name)?;
        let when = at.unwrap_or(&self.asof).to_string();
        let (fn_hash, desc) = self.function(fn_name, &when, at)?;
        let (ns, f) = fn_name.rsplit_once('/').unwrap_or(("", fn_name));
        let cell = Cell { ns: ns.to_string(), fn_name: f.to_string(), fn_hash, args: args.clone() };
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
            Lookup::Ready(Done::Fault(code, known)) => {
                self.fault(&known);
                Err(EvalError::Fault(code))
            }
            Lookup::Ready(Done::NotCovered | Done::Unsigned(..)) => {
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
                self.fault(&known);
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
        if let Some(e) = self.stopped() {
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
        if let Some(e) = self.stopped() {
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
            let spec = Spec::Versions { cell, from: from.to_string(), to: to.to_string() };
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
