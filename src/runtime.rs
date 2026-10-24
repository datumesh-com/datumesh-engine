//! What a function's language plugs into: [`Runtime`], the [`Inputs`] it reads through and
//! the [`Budget`] it charges.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use datumesh_core::{Args, Cell, Descriptor, Scalar};

use crate::error::EvalError;

/// The evaluator of one function language (`code.lang` of a descriptor).
///
/// `evaluate` computes the value of `cell` (a cell of the function `descriptor` describes;
/// `cell.fn_hash` is the descriptor's hash) from the cell's arguments and from the values of
/// other cells, which it reads through `inputs`. It must be a pure function of those: no
/// clock, no randomness, no I/O. It charges its work to `budget` and propagates every error
/// `inputs` and `budget` return (see [`EvalError`]).
///
/// It may be called more than once for one cell in one request: when it reads an input that
/// is not available yet, `read` returns [`EvalError::Suspended`], the engine evaluates the
/// input, then calls `evaluate` again from the start (reads already made are then answered at
/// once). So `evaluate` must not have side effects either.
pub trait Runtime: Send + Sync {
    /// The language this runtime evaluates: a name (SPEC 2.2), e.g. `expr`. `stored` is the
    /// engine's own (a lookup) and cannot be configured.
    fn lang(&self) -> &str;

    /// The value of `cell`, whose type must be the descriptor's declared return type.
    fn evaluate(
        &self,
        descriptor: &Descriptor,
        cell: &Cell,
        inputs: &mut dyn Inputs,
        budget: &mut Budget,
    ) -> Result<Scalar, EvalError>;
}

/// How a runtime reads the cells its function depends on.
pub trait Inputs {
    /// The value of the cell of function `fn_name` (fully qualified, `<namespace>/<name>`)
    /// with arguments `args`, as of the evaluation's asof.
    ///
    /// Checks that `fn_name` is in the descriptor's `inputs` ([`EvalError::UndeclaredInput`]),
    /// resolves it as of the evaluation's asof ([`EvalError::NameUnresolved`]), gets the cell
    /// through the engine and records the receipt used (it becomes one of the computed
    /// receipt's `inputs`), and returns its value. Costs one unit of fuel.
    ///
    /// [`EvalError::Suspended`]: the input is a computed cell not evaluated yet; return it
    /// (with `?`): the engine evaluates the input and calls the runtime again.
    /// [`EvalError::InputNotCovered`]: the input has no value as of the asof, so neither has
    /// the cell being evaluated, whatever the runtime returns next (a receipt cannot list an
    /// absence among its inputs).
    ///
    /// The asof itself is deliberately not offered: a value may depend on time only through
    /// the inputs it reads, which is what makes memoised versions checkable.
    fn read(&mut self, fn_name: &str, args: &Args) -> Result<Scalar, EvalError>;
}

/// The fuel of one request: runtime steps plus input reads, shared by every evaluation the
/// request makes. When it runs out, the request fails with `BudgetExceeded` and stores
/// nothing.
pub struct Budget {
    fuel: Arc<Fuel>,
}

struct Fuel {
    limit: u64,
    remaining: AtomicU64,
    exhausted: AtomicBool,
}

impl Budget {
    /// A budget of `fuel` units (for a runtime's own tests; the engine makes the budget of a
    /// request from its configured fuel).
    pub fn new(fuel: u64) -> Budget {
        Budget { fuel: Arc::new(Fuel { limit: fuel, remaining: AtomicU64::new(fuel), exhausted: AtomicBool::new(false) }) }
    }

    /// Charges `steps` units of work. [`EvalError::BudgetExceeded`] when fewer remain: the
    /// budget is then exhausted for the rest of the request.
    pub fn charge(&mut self, steps: u64) -> Result<(), EvalError> {
        self.charge_shared(steps)
    }

    /// The units left.
    pub fn remaining(&self) -> u64 {
        self.fuel.remaining.load(Ordering::Relaxed)
    }

    /// The units the request started with.
    pub fn limit(&self) -> u64 {
        self.fuel.limit
    }

    /// Whether a charge has failed.
    pub fn is_exhausted(&self) -> bool {
        self.fuel.exhausted.load(Ordering::Relaxed)
    }

    /// Another handle on the same fuel (the engine's reads charge it too).
    pub(crate) fn share(&self) -> Budget {
        Budget { fuel: Arc::clone(&self.fuel) }
    }

    pub(crate) fn charge_shared(&self, steps: u64) -> Result<(), EvalError> {
        let f = &self.fuel;
        let mut cur = f.remaining.load(Ordering::Relaxed);
        loop {
            if f.exhausted.load(Ordering::Relaxed) || cur < steps {
                f.exhausted.store(true, Ordering::Relaxed);
                f.remaining.store(0, Ordering::Relaxed);
                return Err(EvalError::BudgetExceeded { fuel: f.limit });
            }
            match f.remaining.compare_exchange_weak(cur, cur - steps, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return Ok(()),
                Err(now) => cur = now,
            }
        }
    }
}

impl fmt::Debug for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Budget")
            .field("limit", &self.limit())
            .field("remaining", &self.remaining())
            .field("exhausted", &self.is_exhausted())
            .finish()
    }
}
