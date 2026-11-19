//! The solver (Build 2 Part 3, E2; SPEC 5.9 `solve`): bisection on `vary` within [lo, hi].
//! Tests first.
//!
//! API specified (besides tests/whatif.rs):
//!
//! ```text
//! Solve::new(vary: CellRef, target: CellRef, equals: Scalar, lo: Scalar, hi: Scalar) -> Solve
//!   all four scalars of type number. (The contract names `Decimal`; the engine has no such
//!   type and does not depend on the formula crate, so the engine takes number Scalars.)
//! EngineError::WhatifNoSolution   E_WHATIF_NO_SOLUTION: no sign change, or the request's
//!                                 fuel ran out (never BudgetExceeded, no partial answer)
//! ScenarioAnswer.solved = Some((vary cell, value found)); value = target's value under it.
//! ```

mod common;

use common::*;
use datumesh_core::{Cell, CellRef, Scalar};
use datumesh_engine::{Engine, EngineError, EvalError, Overrides, Solve};

const X: &str = "rates.example.org/x";
const LIN: &str = "calc.example.net/lin";
const SQ: &str = "calc.example.net/sq";
const CUBIC: &str = "calc.example.net/cubic";
const POS: &str = "calc.example.net/pos";

fn f(s: &Scalar) -> f64 {
    s.lexical().parse().unwrap()
}

fn fnum(v: f64) -> Scalar {
    num(&format!("{v}"))
}

/// x stored (and never published: the solver overrides it); lin = 3x + 1; sq = x^2;
/// cubic = x^3 + x; pos = x^2 + 1 (never zero). Closures use f64 (their values are decimal
/// strings, bounded by the engine's 34-digit rounding).
fn world() -> World {
    let mut w = World::new();
    w.stored_fn(X, &[]);
    let read = |i: &mut dyn datumesh_engine::Inputs| -> Result<f64, EvalError> { Ok(f(&i.read(X, &args(&[]))?)) };
    w.native_fn(LIN, &[X], &[], move |_, i, _| Ok(fnum(3.0 * read(i)? + 1.0)));
    w.native_fn(SQ, &[X], &[], move |_, i, _| {
        let x = read(i)?;
        Ok(fnum(x * x))
    });
    w.native_fn(CUBIC, &[X], &[], move |_, i, _| {
        let x = read(i)?;
        Ok(fnum(x * x * x + x))
    });
    w.native_fn(POS, &[X], &[], move |_, i, _| {
        let x = read(i)?;
        Ok(fnum(x * x + 1.0))
    });
    w
}

fn solve(w: &World, target: &str, equals: &str, lo: &str, hi: &str) -> Solve {
    Solve::new(w.cellref(X, &[]), w.cellref(target, &[]), num(equals), num(lo), num(hi))
}

fn run(w: &World, e: &Engine, target: &str, equals: &str, lo: &str, hi: &str) -> Result<datumesh_engine::ScenarioAnswer, EngineError> {
    let c: CellRef = w.cellref(target, &[]);
    e.whatif(&c, None, &Overrides::new(), Some(&solve(w, target, equals, lo, hi)))
}

fn solved(a: &datumesh_engine::ScenarioAnswer) -> (Cell, Scalar) {
    a.solved.clone().expect("solved is present when the request had solve")
}

fn tolerance_ok(value: &Scalar, equals: f64) -> bool {
    // The contract's stop is |target - equals| <= 1e-12 * max(1, |equals|) or an interval under
    // 1e-12 * max(1, |lo|, |hi|); the second can leave the value a few 1e-12 off, so 1e-10.
    (f(value) - equals).abs() <= 1e-10 * equals.abs().max(1.0)
}

#[test]
fn bisection_finds_the_root_of_a_linear_function() {
    let w = world();
    let e = w.engine();
    let a = run(&w, &e, LIN, "10", "-100", "100").unwrap();
    let (vary, x) = solved(&a);
    assert_eq!(vary, w.cell(X, &[]));
    assert!((f(&x) - 3.0).abs() < 1e-9, "{x:?}");
    assert!(tolerance_ok(&a.value, 10.0), "the value is the target's under the solution: {:?}", a.value);
    assert_eq!(a.ty, datumesh_core::Type::Number);
    assert!(a.overrides.is_empty());
    // A root at the end of the interval: one of the two evaluations is zero.
    let a = run(&w, &e, LIN, "1", "0", "5").unwrap();
    assert!(f(&solved(&a).1).abs() < 1e-9);
    let a = run(&w, &e, LIN, "16", "0", "5").unwrap();
    assert!((f(&solved(&a).1) - 5.0).abs() < 1e-9);
    // Decreasing direction (hi gives the smaller value): the signs are opposite either way.
    let a = run(&w, &e, LIN, "-8", "-10", "10").unwrap();
    assert!((f(&solved(&a).1) + 3.0).abs() < 1e-9);
}

#[test]
fn bisection_finds_the_root_of_a_non_linear_function() {
    let w = world();
    let e = w.engine();
    let a = run(&w, &e, SQ, "2", "0", "2").unwrap();
    let x = f(&solved(&a).1);
    assert!((x - 2f64.sqrt()).abs() < 1e-9, "{x}");
    assert!(tolerance_ok(&a.value, 2.0), "{:?}", a.value);
    let a = run(&w, &e, CUBIC, "10", "-10", "10").unwrap();
    assert!((f(&solved(&a).1) - 2.0).abs() < 1e-9);
    assert!(tolerance_ok(&a.value, 10.0));
    // With other overrides in the same request: lin is solved with c-like extras not
    // involved, the result is the same.
    let c = w.cellref(LIN, &[]);
    let a = e.whatif(&c, None, &Overrides::new(), Some(&solve(&w, LIN, "4", "0", "10"))).unwrap();
    assert!((f(&solved(&a).1) - 1.0).abs() < 1e-9);
}

#[test]
fn the_solved_value_is_rounded_to_34_significant_digits() {
    let w = world();
    let e = w.engine();
    let a = run(&w, &e, SQ, "2", "0", "2").unwrap();
    let lex = solved(&a).1.lexical().to_string();
    let digits = lex.trim_start_matches('-').replace('.', "");
    let sig = digits.trim_start_matches('0');
    assert!(sig.len() <= 34, "{lex}");
    assert!(num(&lex) == solved(&a).1);
}

#[test]
fn no_sign_change_is_no_solution() {
    let w = world();
    let e = w.engine();
    // x^2 + 1 is above zero everywhere.
    assert!(matches!(run(&w, &e, POS, "0", "-5", "5"), Err(EngineError::WhatifNoSolution)));
    // The target never reaches 100 within [0, 2].
    assert!(matches!(run(&w, &e, SQ, "100", "0", "2"), Err(EngineError::WhatifNoSolution)));
    // Both ends on one side, below.
    assert!(matches!(run(&w, &e, LIN, "-100", "0", "5"), Err(EngineError::WhatifNoSolution)));
}

#[test]
fn fuel_exhaustion_is_no_solution_not_a_budget_error() {
    let w = world();
    // The same request solves with the default fuel.
    let full = w.engine();
    let ok = run(&w, &full, SQ, "2", "0", "2").unwrap();
    assert!(solved(&ok).1.lexical().len() > 5);
    // A little fuel: the first evaluations fit, the iterations to 1e-12 do not.
    let short = w.engine_with(|b| b.fuel(30));
    match run(&w, &short, SQ, "2", "0", "2") {
        Err(EngineError::WhatifNoSolution) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(rows(&w), [0, 0, 0, 0], "nothing stored");
}

#[test]
fn the_solver_is_deterministic_and_writes_nothing() {
    let w = world();
    let e = w.engine();
    let first = run(&w, &e, CUBIC, "10", "-10", "10").unwrap();
    let second = run(&w, &e, CUBIC, "10", "-10", "10").unwrap();
    assert_eq!(first, second);
    assert_eq!(solved(&first), solved(&second));
    let other = w.engine();
    assert_eq!(solved(&run(&w, &other, CUBIC, "10", "-10", "10").unwrap()), solved(&first));
    assert_eq!(rows(&w), [0, 0, 0, 0]);
    assert_eq!(e.memo_stats().entries, 0);
}
