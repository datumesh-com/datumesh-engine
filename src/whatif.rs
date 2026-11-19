//! Scenarios (Build 2 Part 3, E1 and E2; SPEC 5.9): [`Engine::whatif`] evaluates a cell as of
//! an asof with overrides in place, and writes nothing. The solver ([`Solve`]) finds the value
//! of one number cell that makes another equal a number, by bisection.
//!
//! A scenario answer is not an [`Answer`](crate::Answer): it has no receipt and is never
//! stored, signed or memoised. Cells that do not depend on the overrides answer as `get` would,
//! from the memo and the store; the dependent ones are evaluated in the request's scope.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use datumesh_core::{lexical, Arg, Cell, CellRef, FunctionDescriptor, Scalar, Type};

use crate::check::{check_args, check_no_slice};
use crate::engine::{Engine, STORED};
use crate::error::{EngineError, StoreError};
use crate::eval::{Done, Req};
use crate::runtime::{Budget, Pattern, SlicePattern};
use crate::slice::cells_slice;

/// The most overrides a scenario may have (SPEC 5.9).
const MAX_OVERRIDES: usize = 64;

/// The overrides of a scenario: value overrides and slice overrides, in the order given.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    items: Vec<Override>,
}

#[derive(Clone, Debug)]
enum Override {
    Value { cell: CellRef, value: Scalar },
    Slice { slice: CellRef, add: Vec<(CellRef, Scalar)>, remove: Vec<CellRef> },
}

impl Overrides {
    /// No overrides.
    pub fn new() -> Overrides {
        Overrides::default()
    }

    /// `cell` answers `value` (of the function's return type) in the scenario.
    pub fn value(mut self, cell: CellRef, value: Scalar) -> Overrides {
        self.items.push(Override::Value { cell, value });
        self
    }

    /// The members of the stored slice `slice` (a cell reference with its open arguments left
    /// open) change: `add` are new members with their values, `remove` are members that go.
    pub fn slice(mut self, slice: CellRef, add: Vec<(CellRef, Scalar)>, remove: Vec<CellRef>) -> Overrides {
        self.items.push(Override::Slice { slice, add, remove });
        self
    }

    /// The number of overrides.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are no overrides.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// A solve for [`Engine::whatif`]: find the value of `vary` (a number cell) in `[lo, hi]` at
/// which `target` (a number cell) equals `equals`. All four are number scalars. The value found
/// carries 15 significant digits (rounded half-even), and the answer is the requested cell's
/// value under it: the target's, when the requested cell is the target.
#[derive(Clone, Debug)]
pub struct Solve {
    vary: CellRef,
    target: CellRef,
    equals: Scalar,
    lo: Scalar,
    hi: Scalar,
}

impl Solve {
    /// The solve of `target = equals` over `vary` in `[lo, hi]`.
    pub fn new(vary: CellRef, target: CellRef, equals: Scalar, lo: Scalar, hi: Scalar) -> Solve {
        Solve { vary, target, equals, lo, hi }
    }
}

/// A scenario's answer ([`Engine::whatif`]): the value of the cell as of the asof, with the
/// overrides in place. It is labelled a scenario: it has no receipt and is never stored.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct ScenarioAnswer {
    /// The cell asked for.
    pub cell: Cell,
    /// Its type (the function's return type).
    pub ty: Type,
    /// Its value in the scenario.
    pub value: Scalar,
    /// The asof: the one asked for, else the clock's now.
    pub asof: String,
    /// The overrides, sorted by [`ScenarioOverride::id`].
    pub overrides: Vec<ScenarioOverride>,
    /// The solved cell and the value found for it, when the request had a solve.
    pub solved: Option<(Cell, Scalar)>,
}

/// An override of a scenario, as its answer lists it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub enum ScenarioOverride {
    /// A cell with a value.
    Value {
        /// The cell.
        cell: Cell,
        /// Its value.
        value: Scalar,
    },
    /// A stored slice with members added and removed; both sorted by cell id.
    Slice {
        /// The slice (its open arguments are [`Arg::Slice`]).
        slice: Cell,
        /// The added members, with their values.
        add: Vec<(Cell, Scalar)>,
        /// The removed members.
        remove: Vec<Cell>,
    },
}

impl ScenarioOverride {
    /// The id of the overridden cell, or of the slice.
    pub fn id(&self) -> String {
        match self {
            ScenarioOverride::Value { cell, .. } => cell.id(),
            ScenarioOverride::Slice { slice, .. } => slice.id(),
        }
    }
}

/// The overrides of one scenario, validated and resolved, as the evaluation reads them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Scope {
    /// The overridden cells, by cell id.
    pub(crate) values: HashMap<String, (Cell, Scalar)>,
    /// The overridden slices, by slice id.
    pub(crate) slices: HashMap<String, SliceOverride>,
    /// The names of the overridden functions.
    pub(crate) affected: HashSet<String>,
}

/// A slice override: the members added, and the cell ids of those removed.
#[derive(Clone, Debug)]
pub(crate) struct SliceOverride {
    pub(crate) add: Vec<(Cell, Scalar)>,
    pub(crate) remove: HashSet<String>,
}

impl Scope {
    /// This scope with `cell` overridden to `value` as well (the solver's trial values).
    pub(crate) fn with_value(&self, cell: Cell, value: Scalar) -> Scope {
        let mut s = self.clone();
        s.affected.insert(cell.function_name());
        s.values.insert(cell.id(), (cell, value));
        s
    }
}

impl Engine {
    /// The value of `cell` as of `asof` (else `cell.asof`, else now) with `overrides` in place.
    /// With `solve`, `vary` is first set to the value that makes its `target` equal its `equals`
    /// (bisection within its interval; see [`Solve`]), and `cell`'s value is then that under the
    /// solved `vary`; the answer's `solved` says what was found.
    ///
    /// The overridden cells answer their values; a slice override changes a stored slice's
    /// members. A cell that does not depend on the overrides answers from the memo and the
    /// store as [`get`](Engine::get) would; a dependent cell is evaluated in the request's scope.
    /// Nothing is written: no receipt, computed version, memo entry or signature. The answer
    /// is labelled a scenario, not a fact.
    ///
    /// Errors: [`EngineError::Override`] for overrides that are not valid (or more than 64),
    /// [`EngineError::NotCovered`] when the cell has no value (a fault in scope included), and
    /// [`EngineError::WhatifNoSolution`] for a solve without a solution or out of fuel. The
    /// usual request errors (asof, arguments, unlicensed, budget) are those of `get`.
    pub fn whatif(
        &self,
        cell: &CellRef,
        asof: Option<&str>,
        overrides: &Overrides,
        solve: Option<&Solve>,
    ) -> Result<ScenarioAnswer, EngineError> {
        let now = self.clock.now();
        let a = asof.or(cell.asof.as_deref()).unwrap_or(&now).to_string();
        lexical::check_instant(&a, "$.asof").map_err(EngineError::Invalid)?;
        check_no_slice(&cell.args)?;
        if overrides.len() > MAX_OVERRIDES {
            return Err(EngineError::Override("E_CANON_LIMIT".into()));
        }
        let Some((fn_hash, desc)) = self.function_as_of(&cell.ns, &cell.fn_name, &a)? else {
            return Err(EngineError::NotCovered { fault: None });
        };
        let target = cell.resolve(&fn_hash).map_err(EngineError::Invalid)?;
        check_args(&target, &desc).map_err(EngineError::Invalid)?;
        let (scope, list) = self.build_scope(overrides, &a)?;
        let scope = Arc::new(scope);
        // One budget for the whole request: a solve's iterations share its fuel.
        let budget = Budget::with_work(self.fuel, self.work_limit);
        let ty = desc.signature.returns.ty;
        let (value, solved) = match solve {
            None => (self.scope_value(&target, &desc, &a, &now, &scope, &budget)?, None),
            Some(s) => self.solve(s, &target, &desc, &a, &now, &scope, &budget)?,
        };
        Ok(ScenarioAnswer { cell: target, ty, value, asof: a, overrides: list, solved })
    }

    /// The function `<ns>/<fn_name>` means as of `asof`: its hash and descriptor, or none when
    /// the name is unbound or has no descriptor.
    fn function_as_of(
        &self,
        ns: &str,
        fn_name: &str,
        asof: &str,
    ) -> Result<Option<(String, Arc<FunctionDescriptor>)>, EngineError> {
        let name = format!("{ns}/{fn_name}");
        let Some(hash) = self.store.resolve_name(&name, asof).map_err(|e| StoreError::new("resolve_name", e))? else {
            return Ok(None);
        };
        Ok(self.descriptor_of(&hash)?.map(|d| (hash, d)))
    }

    /// A cell reference resolved as of `asof`, with its arguments checked against the signature.
    fn resolve_cell(&self, c: &CellRef, asof: &str) -> Result<(Cell, Arc<FunctionDescriptor>), EngineError> {
        check_no_slice(&c.args)?;
        let Some((hash, desc)) = self.function_as_of(&c.ns, &c.fn_name, asof)? else {
            return Err(EngineError::NameUnresolved { fn_name: format!("{}/{}", c.ns, c.fn_name), asof: asof.into() });
        };
        let cell = c.resolve(&hash).map_err(EngineError::Invalid)?;
        check_args(&cell, &desc).map_err(EngineError::Invalid)?;
        Ok((cell, desc))
    }

    /// Validates the overrides as of `asof` (SPEC 5.9) and makes the scope and the sorted list
    /// the answer shows.
    fn build_scope(&self, overrides: &Overrides, asof: &str) -> Result<(Scope, Vec<ScenarioOverride>), EngineError> {
        let refused = |code: &str| EngineError::Override(code.to_string());
        let mut scope = Scope::default();
        let mut list = Vec::new();
        for o in &overrides.items {
            match o {
                Override::Value { cell, value } => {
                    let (c, desc) = self.resolve_cell(cell, asof)?;
                    if value.ty() != desc.signature.returns.ty {
                        return Err(refused("E_CANON_TYPE"));
                    }
                    let id = c.id();
                    if scope.values.contains_key(&id) {
                        return Err(refused("E_CANON_ARG_DUPLICATE"));
                    }
                    scope.affected.insert(c.function_name());
                    scope.values.insert(id, (c.clone(), value.clone()));
                    list.push(ScenarioOverride::Value { cell: c, value: value.clone() });
                }
                Override::Slice { slice, add, remove } => {
                    let Some((hash, desc)) = self.function_as_of(&slice.ns, &slice.fn_name, asof)? else {
                        return Err(EngineError::NameUnresolved {
                            fn_name: format!("{}/{}", slice.ns, slice.fn_name),
                            asof: asof.into(),
                        });
                    };
                    let fn_name = format!("{}/{}", slice.ns, slice.fn_name);
                    // Only stored slices change by membership.
                    if desc.lang != STORED {
                        return Err(refused("E_SLICE_MEMBER"));
                    }
                    let pattern: SlicePattern = slice
                        .args
                        .iter()
                        .map(|(k, a)| {
                            let p = match a {
                                Arg::Value(v) => Pattern::Value(v.clone()),
                                Arg::Slice => Pattern::Open,
                            };
                            (k.clone(), p)
                        })
                        .collect();
                    let s = cells_slice(&fn_name, &hash, &desc, &pattern).map_err(EngineError::Invalid)?;
                    let sid = s.id().to_string();
                    if scope.slices.contains_key(&sid) {
                        return Err(refused("E_CANON_ARG_DUPLICATE"));
                    }
                    let slice_cell = slice.resolve(&hash).map_err(EngineError::Invalid)?;
                    let mut adds: Vec<(Cell, Scalar)> = Vec::with_capacity(add.len());
                    for (c, v) in add {
                        let cell = c.resolve(&hash).map_err(EngineError::Invalid)?;
                        check_args(&cell, &desc).map_err(EngineError::Invalid)?;
                        if cell.function_name() != fn_name {
                            return Err(refused("E_SLICE_MEMBER"));
                        }
                        adds.push((cell, v.clone()));
                    }
                    let mut removes: Vec<Cell> = Vec::with_capacity(remove.len());
                    for c in remove {
                        let cell = c.resolve(&hash).map_err(EngineError::Invalid)?;
                        check_args(&cell, &desc).map_err(EngineError::Invalid)?;
                        if cell.function_name() != fn_name {
                            return Err(refused("E_SLICE_MEMBER"));
                        }
                        removes.push(cell);
                    }
                    let mut add_ids = HashSet::new();
                    for (c, _) in &adds {
                        if !add_ids.insert(c.id()) {
                            return Err(refused("E_CANON_ARG_DUPLICATE"));
                        }
                    }
                    let mut remove_ids = HashSet::new();
                    for c in &removes {
                        if !remove_ids.insert(c.id()) {
                            return Err(refused("E_CANON_ARG_DUPLICATE"));
                        }
                    }
                    if adds.iter().any(|(c, _)| remove_ids.contains(&c.id())) {
                        return Err(refused("E_SLICE_MEMBER"));
                    }
                    let members: HashSet<String> = self
                        .store
                        .slice_values(&s, asof)
                        .map_err(|e| StoreError::new("slice_values", e))?
                        .into_iter()
                        .map(|v| v.version.cell_id)
                        .collect();
                    for c in &removes {
                        if !matches(&pattern, c) || !members.contains(&c.id()) {
                            return Err(refused("E_SLICE_MEMBER"));
                        }
                    }
                    for (c, v) in &adds {
                        if !matches(&pattern, c) || members.contains(&c.id()) {
                            return Err(refused("E_SLICE_MEMBER"));
                        }
                        if v.ty() != desc.signature.returns.ty {
                            return Err(refused("E_CANON_TYPE"));
                        }
                        // Read alone, an added member answers its value, as an overridden cell does.
                        scope.values.insert(c.id(), (c.clone(), v.clone()));
                    }
                    scope.affected.insert(fn_name);
                    scope.slices.insert(sid, SliceOverride { add: adds.clone(), remove: remove_ids });
                    let mut add_sorted = adds;
                    add_sorted.sort_by_key(|(c, _)| c.id());
                    removes.sort_by_key(|c| c.id());
                    list.push(ScenarioOverride::Slice { slice: slice_cell, add: add_sorted, remove: removes });
                }
            }
        }
        list.sort_by_key(|o| o.id());
        Ok((scope, list))
    }

    /// The value of `target` in `scope` as of `asof`: the override's value when it is
    /// overridden, else what the scenario evaluation gives.
    fn scope_value(
        &self,
        target: &Cell,
        desc: &Arc<FunctionDescriptor>,
        asof: &str,
        now: &str,
        scope: &Arc<Scope>,
        budget: &Budget,
    ) -> Result<Scalar, EngineError> {
        if let Some((_, v)) = scope.values.get(&target.id()) {
            return Ok(v.clone());
        }
        let mut req = Req::scoped(self, asof, now, scope.clone(), budget);
        match req.scope_run(target.clone(), desc.clone())? {
            Done::Value(a) => Ok(a.value.clone()),
            Done::Fault(f) => Err(EngineError::NotCovered { fault: Some(f.fault) }),
            Done::NotCovered | Done::Unsigned(..) => Err(EngineError::NotCovered { fault: None }),
        }
    }

    /// The solve of [`Engine::whatif`] (E2): bisection of `target - equals` over `vary`. Out of
    /// fuel, or no sign change, is `WhatifNoSolution`.
    #[allow(clippy::too_many_arguments)]
    fn solve(
        &self,
        s: &Solve,
        target: &Cell,
        desc: &Arc<FunctionDescriptor>,
        asof: &str,
        now: &str,
        scope: &Arc<Scope>,
        budget: &Budget,
    ) -> Result<(Scalar, Option<(Cell, Scalar)>), EngineError> {
        // The bisection is on the solve's `target`; the answer is the requested cell's value.
        let (vary, vary_desc) = self.resolve_cell(&s.vary, asof)?;
        let (solve_target, solve_desc) = self.resolve_cell(&s.target, asof)?;
        let number = Type::Number;
        if vary_desc.signature.returns.ty != number
            || solve_desc.signature.returns.ty != number
            || [&s.equals, &s.lo, &s.hi].iter().any(|x| x.ty() != number)
        {
            return Err(EngineError::Override("E_CANON_TYPE".into()));
        }
        let result = (|| {
            let num = |x: &Scalar| -> f64 { x.lexical().parse().unwrap_or(f64::NAN) };
            let (eq, lo, hi) = (num(&s.equals), num(&s.lo), num(&s.hi));
            if lo > hi {
                return Err(EngineError::WhatifNoSolution);
            }
            // The scope with `vary` at `x`, rounded as the answer will carry it.
            let trial = |x: f64| -> Result<(Scalar, Arc<Scope>), EngineError> {
                let text = decimal15(x).ok_or(EngineError::WhatifNoSolution)?;
                let value = Scalar::from_lexical(number, &text).map_err(|_| EngineError::WhatifNoSolution)?;
                Ok((value.clone(), Arc::new(scope.with_value(vary.clone(), value))))
            };
            let f = |x: f64| -> Result<f64, EngineError> {
                let (_, sc) = trial(x)?;
                let v = self.scope_value(&solve_target, &solve_desc, asof, now, &sc, budget)?;
                Ok(num(&v) - eq)
            };
            let flo = f(lo)?;
            let x = if flo == 0.0 {
                lo
            } else {
                let fhi = f(hi)?;
                if fhi == 0.0 {
                    hi
                } else {
                    if (flo < 0.0) == (fhi < 0.0) {
                        return Err(EngineError::WhatifNoSolution);
                    }
                    let (mut a, mut fa, mut b, mut fb) = (lo, flo, hi, fhi);
                    loop {
                        let mid = a + (b - a) / 2.0;
                        if mid <= a || mid >= b {
                            // The interval holds no double between its ends: the nearer one.
                            break if fa.abs() <= fb.abs() { a } else { b };
                        }
                        let fm = f(mid)?;
                        if fm.abs() <= 1e-12 * eq.abs().max(1.0)
                            || (b - a).abs() <= 1e-12 * a.abs().max(b.abs()).max(1.0)
                        {
                            break mid;
                        }
                        if (fm < 0.0) == (fa < 0.0) {
                            a = mid;
                            fa = fm;
                        } else {
                            b = mid;
                            fb = fm;
                        }
                    }
                }
            };
            let (value, sc) = trial(x)?;
            let v = self.scope_value(target, desc, asof, now, &sc, budget)?;
            Ok((v, Some((vary.clone(), value))))
        })();
        match result {
            Err(EngineError::BudgetExceeded { .. } | EngineError::WorkExceeded { .. }) => {
                Err(EngineError::WhatifNoSolution)
            }
            other => other,
        }
    }
}

/// The hash standing for an unsigned scenario answer (an overridden value, or a member a
/// scenario changed): not a receipt's, but distinct for each cell and value.
pub(crate) fn scoped_hash(id: &str, value: &Scalar) -> String {
    datumesh_core::sha256_hex(format!("scenario {id} {} {}", value.ty().as_str(), value.lexical()).as_bytes())
}

/// Whether `cell`'s arguments match `pattern` (a bound argument equal, an open one anything).
fn matches(pattern: &SlicePattern, cell: &Cell) -> bool {
    pattern.iter().all(|(k, p)| match (cell.args.get(k), p) {
        (Some(Arg::Value(v)), Pattern::Value(pv)) => v == pv,
        (Some(Arg::Value(_)), Pattern::Open) => true,
        _ => false,
    })
}

/// A finite number rounded to 15 significant digits, half-even (the rounding of the Rust float
/// formatter on the exact binary value), as a canonical decimal (SPEC 3.1: no exponent, no
/// trailing zeros). Solve values carry these digits: the bisection runs on binary doubles, and
/// 15 digits are what a double holds deterministically. `None` for a non-finite value or one
/// whose text is too long.
fn decimal15(x: f64) -> Option<String> {
    if !x.is_finite() {
        return None;
    }
    if x == 0.0 {
        return Some("0".into());
    }
    let text = format!("{:.14e}", x.abs());
    let (mantissa, exp) = text.split_once('e')?;
    let exp: i64 = exp.parse().ok()?;
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if exp >= 0 {
        let int_len = usize::try_from(exp).ok()? + 1;
        if digits.len() <= int_len {
            out.push_str(digits);
            out.extend(std::iter::repeat_n('0', int_len - digits.len()));
        } else {
            out.push_str(&digits[..int_len]);
            out.push('.');
            out.push_str(&digits[int_len..]);
        }
    } else {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', usize::try_from(-exp - 1).ok()?));
        out.push_str(digits);
    }
    (out.len() <= 128).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::decimal15;

    #[test]
    fn decimals_are_canonical() {
        assert_eq!(decimal15(3.0).as_deref(), Some("3"));
        assert_eq!(decimal15(-0.5).as_deref(), Some("-0.5"));
        assert_eq!(decimal15(0.0).as_deref(), Some("0"));
        assert_eq!(decimal15(1e-5).as_deref(), Some("0.00001"));
        assert_eq!(decimal15(120.0).as_deref(), Some("120"));
        assert_eq!(decimal15(2f64.sqrt()).as_deref(), Some("1.4142135623731"));
    }
}
