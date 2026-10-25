//! What a function's language plugs into: [`Runtime`], the [`Inputs`] it reads through and
//! the [`Budget`] it charges.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use datumesh_core::{Args, Cell, FunctionDescriptor, Scalar};

use crate::error::EvalError;

/// The evaluator of one function language (`code.lang` of a descriptor).
///
/// `evaluate` computes the value of `cell` (a cell of the function `descriptor` describes;
/// `cell.fn_hash` is the descriptor's hash) from the cell's arguments and from the values of
/// other cells, which it reads through `inputs`. It must be a pure function of those (and of
/// [`Inputs::asof`] for an asof-dependent function): no clock, no randomness, no I/O. It
/// charges its work to `budget` and propagates every error `inputs` and `budget` return (see
/// [`EvalError`]); it may catch an [`EvalError::Fault`] its language lets it catch.
///
/// It may be called more than once for one cell in one request: when it reads an input that
/// is not available yet, the read returns [`EvalError::Suspended`], the engine evaluates the
/// input, then calls `evaluate` again from the start (reads already made are then answered at
/// once). So `evaluate` must not have side effects either. Each call is an attempt with a
/// budget meter of its own: the fuel of a cell's evaluation is that of its last, completed
/// call ([`Budget`]).
pub trait Runtime: Send + Sync {
    /// The language this runtime evaluates: a name (SPEC 2.2), e.g. `formula`. `stored` is
    /// the engine's own (a lookup) and cannot be configured.
    fn lang(&self) -> &str;

    /// The value of `cell`, whose type must be the descriptor's declared return type; or
    /// [`EvalError::Fault`]: the cell has no value (SPEC 15.12).
    fn evaluate(
        &self,
        descriptor: &FunctionDescriptor,
        cell: &Cell,
        inputs: &mut dyn Inputs,
        budget: &mut Budget,
    ) -> Result<Scalar, EvalError>;

    /// Whether the function is **asof-dependent** (SPEC 15.12: its body uses `asof()` or a
    /// bounded `latest`), derived from the descriptor alone. Its receipts then have the
    /// evaluation asof as their `asof`, one per requested asof, and the engine never reuses
    /// one for another asof, from the memo or from the store. Provided: `false`.
    ///
    /// A function that reads [`Inputs::asof`] is treated as asof-dependent for that
    /// evaluation even when this says `false`; its receipt is then not kept as a computed
    /// version (a later request could not tell it apart), so declaring it is cheaper.
    fn asof_dependent(&self, descriptor: &FunctionDescriptor) -> bool {
        let _ = descriptor;
        false
    }
}

/// What a read gave: the value and the receipt stating it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    /// The value.
    pub value: Scalar,
    /// The hash of the receipt stating it (SPEC 8.3).
    pub receipt_hash: String,
    /// The receipt's `tx`.
    pub tx: String,
    /// The receipt's `asof`.
    pub asof: String,
}

impl Input {
    /// An input (for a runtime's own tests).
    pub fn new(value: Scalar, receipt_hash: impl Into<String>, tx: impl Into<String>, asof: impl Into<String>) -> Input {
        Input { value, receipt_hash: receipt_hash.into(), tx: tx.into(), asof: asof.into() }
    }
}

/// A member of a slice ([`Inputs::read_slice`]): the member cell's arguments and its input.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// Every argument of the member cell.
    pub args: Args,
    /// Its value and receipt.
    pub input: Input,
}

impl Member {
    /// A member (for a runtime's own tests).
    pub fn new(args: Args, input: Input) -> Member {
        Member { args, input }
    }
}

/// One argument of a slice pattern ([`Inputs::read_slice`]).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// Bound to a value.
    Value(Scalar),
    /// Open (`*`): any value.
    Open,
    /// Within `[lo..hi]`, both ends inclusive, in the type's order (SPEC 15.13); `lo` after
    /// `hi` is empty. Both ends are of the argument's type: `number`, `date`, `instant` or
    /// `string`.
    Range {
        /// The low end.
        lo: Scalar,
        /// The high end.
        hi: Scalar,
    },
}

/// A slice pattern: every argument of the function, by name.
pub type SlicePattern = BTreeMap<String, Pattern>;

/// A function name's binding ([`Inputs::binding`]).
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct FnBinding {
    /// The function hash the name means.
    pub fn_hash: String,
    /// When the binding began (an instant).
    pub start: String,
    /// The function's descriptor.
    pub descriptor: Arc<FunctionDescriptor>,
}

/// How a runtime reads the cells its function depends on, and the evaluation's asof.
///
/// Every read checks that the function read is in the descriptor's `inputs`
/// ([`EvalError::UndeclaredInput`]) and resolves its name ([`EvalError::NameUnresolved`]),
/// and is recorded: it becomes one of the computed receipt's `inputs` entries, and the name's
/// binding is relied on (its start is a candidate for the receipt's `asof`, SPEC 8.2) as soon
/// as the read is attempted. The fuel each costs is SPEC 15.12's.
///
/// Errors a runtime must propagate (with `?`): [`EvalError::Suspended`] (an input not
/// evaluated yet: the engine evaluates it and calls the runtime again),
/// [`EvalError::InputNotCovered`] (an input with no value: neither has the cell, whatever the
/// runtime returns next), [`EvalError::BudgetExceeded`] and the engine's failures. A
/// [`EvalError::Fault`] it may catch where its language allows (SPEC 15.12: `otherwise`
/// catches `E_FORMULA_EMPTY` and `E_FORMULA_ABSENT`).
pub trait Inputs {
    /// The value of the cell of function `fn_name` (fully qualified, `<namespace>/<name>`)
    /// with arguments `args`, as of the evaluation's asof: [`read_cell`](Inputs::read_cell)'s
    /// value. Costs 1.
    fn read(&mut self, fn_name: &str, args: &Args) -> Result<Scalar, EvalError>;

    /// The cell of `fn_name` with `args` as of the evaluation's asof: its value and the
    /// receipt that answers (hash, `tx`, `asof`). A computed cell is evaluated by the engine;
    /// its fault, if it has one, is returned ([`EvalError::Fault`]). Costs 1.
    fn read_cell(&mut self, fn_name: &str, args: &Args) -> Result<Input, EvalError>;

    /// The cell of `fn_name` with `args` as of the transaction time `t` (SPEC 15.13,
    /// `asof(cell, T)`): the name resolved as of `t`, the version answering as of `t`; an
    /// ordinary single-cell input, and `t` a candidate for the receipt's `asof`. `t` after the
    /// evaluation's asof is the fault `E_FORMULA_FUTURE`, before anything is read. Costs 1.
    fn read_asof(&mut self, fn_name: &str, args: &Args, t: &str) -> Result<Input, EvalError>;

    /// The members of the slice of `fn_name` with `pattern` (every declared argument; at least
    /// one open or ranged) as of the evaluation's asof, in SPEC 15.13 key order: for a stored
    /// function, its cells matching the pattern with a version answering then; for any other,
    /// the cells of the declared `extent` (or, for a range over a `date` or integer `number`
    /// argument without one, every day or integer in it) within the pattern, each evaluated
    /// by the engine, and every one must have a value: else the read is the fault
    /// `E_FORMULA_ABSENT`. One `inputs` entry `slice:<slice id>:<members hash>`, whose
    /// manifest the engine stores. Costs 1 + members, charged once the members are obtained.
    fn read_slice(&mut self, fn_name: &str, pattern: &SlicePattern) -> Result<Vec<Member>, EvalError>;

    /// The versions of the stored cell of `fn_name` with `args` whose `tx` is in `[from, to]`,
    /// as known as of the evaluation's asof, ascending by `tx` (SPEC 15.13 `versions`): a
    /// version slice, one `inputs` entry, `to` a candidate for the receipt's `asof`. `to` after
    /// the evaluation's asof is `E_FORMULA_FUTURE`. Costs 1 + versions.
    fn read_versions(&mut self, fn_name: &str, args: &Args, from: &str, to: &str) -> Result<Vec<Input>, EvalError>;

    /// The binding of `fn_name` as of the evaluation's asof, or as of `at`: its function hash,
    /// when it began and the descriptor; `None` when it has none then. Relied on as a read's
    /// name is. Free.
    fn binding(&mut self, fn_name: &str, at: Option<&str>) -> Result<Option<FnBinding>, EvalError>;

    /// The evaluation's asof (SPEC 15.12 `asof()`). Reading it makes the evaluation
    /// asof-dependent (see [`Runtime::asof_dependent`]). Free.
    fn asof(&mut self) -> String;
}

/// The fuel of an evaluation attempt, on the request's meter.
///
/// Fuel is normative (SPEC 15.12): the fuel of a cell's evaluation is what its last,
/// completed attempt charges ([`used`](Budget::used)), whatever number of times the engine
/// restarted it. A request's normative fuel is the sum of the evaluations it completes, and
/// its limit ([`limit`](Budget::limit)) is checked at every charge against a lower bound of
/// that sum (each suspended evaluation will charge at least what its last attempt did), so a
/// request fails with `BudgetExceeded` exactly when its normative fuel exceeds the limit.
/// Separately the request's **work** (every attempt, those discarded by a restart included,
/// and the engine's validation reads) has a limit of its own, which bounds the total cost.
/// When either runs out, the request fails and stores nothing.
pub struct Budget {
    meter: Arc<Mutex<Meter>>,
}

#[derive(Debug)]
pub(crate) struct Meter {
    limit: u64,
    work_limit: u64,
    /// The normative fuel of the evaluations completed.
    committed: u64,
    /// The lower bounds of the suspended evaluations on the stack.
    pending: u64,
    /// The running attempt: its charges and its lower bound (its earlier attempt's charges).
    attempt: u64,
    floor: u64,
    work: u64,
    pub(crate) exhausted: Exhausted,
}

/// What ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exhausted {
    No,
    Fuel,
    Work,
}

impl Meter {
    fn bound(&self) -> u64 {
        self.committed.saturating_add(self.pending).saturating_add(self.attempt.max(self.floor))
    }
}

impl Budget {
    /// A budget of `fuel` units, with no work limit (for a runtime's own tests; the engine
    /// makes the budget of a request from its configuration).
    pub fn new(fuel: u64) -> Budget {
        Budget::with_work(fuel, u64::MAX)
    }

    pub(crate) fn with_work(fuel: u64, work_limit: u64) -> Budget {
        Budget {
            meter: Arc::new(Mutex::new(Meter {
                limit: fuel,
                work_limit,
                committed: 0,
                pending: 0,
                attempt: 0,
                floor: 0,
                work: 0,
                exhausted: Exhausted::No,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Meter> {
        self.meter.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Charges `steps` units of work to this attempt. [`EvalError::BudgetExceeded`] when the
    /// request's fuel (or its work) runs out: the budget is then exhausted for the rest of the
    /// request.
    pub fn charge(&mut self, steps: u64) -> Result<(), EvalError> {
        self.charge_shared(steps)
    }

    /// The fuel this attempt has charged so far: at the end of a completed attempt, the
    /// normative fuel of the evaluation.
    pub fn used(&self) -> u64 {
        self.lock().attempt
    }

    /// The fuel left: the limit less the lower bound of the request's normative fuel.
    pub fn remaining(&self) -> u64 {
        let m = self.lock();
        if m.exhausted != Exhausted::No {
            return 0;
        }
        m.limit.saturating_sub(m.bound())
    }

    /// The request's fuel limit.
    pub fn limit(&self) -> u64 {
        self.lock().limit
    }

    /// Whether a charge has failed.
    pub fn is_exhausted(&self) -> bool {
        self.lock().exhausted != Exhausted::No
    }

    pub(crate) fn exhausted(&self) -> Exhausted {
        self.lock().exhausted
    }

    pub(crate) fn work_limit(&self) -> u64 {
        self.lock().work_limit
    }

    /// Another handle on the same meter.
    pub(crate) fn share(&self) -> Budget {
        Budget { meter: Arc::clone(&self.meter) }
    }

    /// A normative charge of the running attempt.
    pub(crate) fn charge_shared(&self, steps: u64) -> Result<(), EvalError> {
        let mut m = self.lock();
        if m.exhausted != Exhausted::No {
            return Err(EvalError::BudgetExceeded { fuel: m.limit });
        }
        m.work = m.work.saturating_add(steps);
        if m.work > m.work_limit {
            m.exhausted = Exhausted::Work;
            return Err(EvalError::BudgetExceeded { fuel: m.limit });
        }
        m.attempt = m.attempt.saturating_add(steps);
        if m.bound() > m.limit {
            m.exhausted = Exhausted::Fuel;
            return Err(EvalError::BudgetExceeded { fuel: m.limit });
        }
        Ok(())
    }

    /// Work that is not normative (validation reads, rechecks).
    pub(crate) fn work(&self, steps: u64) -> Result<(), EvalError> {
        let mut m = self.lock();
        if m.exhausted != Exhausted::No {
            return Err(EvalError::BudgetExceeded { fuel: m.limit });
        }
        m.work = m.work.saturating_add(steps);
        if m.work > m.work_limit {
            m.exhausted = Exhausted::Work;
            return Err(EvalError::BudgetExceeded { fuel: m.limit });
        }
        Ok(())
    }

    #[allow(dead_code)] // used by the evaluation (next commits)
    /// Starts an attempt of a frame whose earlier attempts charged at most `floor` (0 for a
    /// first attempt); the frame's floor is no longer pending.
    pub(crate) fn begin_attempt(&self, floor: u64) {
        let mut m = self.lock();
        m.pending = m.pending.saturating_sub(floor);
        m.attempt = 0;
        m.floor = floor;
    }

    #[allow(dead_code)]
    /// The running attempt suspended: returns the frame's new floor, now pending.
    pub(crate) fn suspend_attempt(&self) -> u64 {
        let mut m = self.lock();
        let floor = m.attempt.max(m.floor);
        m.pending = m.pending.saturating_add(floor);
        m.attempt = 0;
        m.floor = 0;
        floor
    }

    #[allow(dead_code)]
    /// The running attempt completed: its fuel is the evaluation's.
    pub(crate) fn finish_attempt(&self) -> u64 {
        let mut m = self.lock();
        let used = m.attempt;
        m.committed = m.committed.saturating_add(used);
        m.attempt = 0;
        m.floor = 0;
        used
    }
}

impl fmt::Debug for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.lock();
        f.debug_struct("Budget")
            .field("limit", &m.limit)
            .field("used", &m.attempt)
            .field("committed", &m.committed)
            .field("exhausted", &(m.exhausted != Exhausted::No))
            .finish()
    }
}
