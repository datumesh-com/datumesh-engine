//! Budget: fuel counts runtime steps plus input reads, per request; exhaustion is
//! BudgetExceeded and nothing is stored for the request.

mod common;

use common::*;
use datumesh_engine::{EngineError, EvalError, DEFAULT_FUEL};

const A: &str = "rates.example.org/a";
const B: &str = "rates.example.org/b";
const FIB: &str = "calc.example.net/fib";
const N: &str = r#"{"type":"number","unit":"1","valid":false}"#;

fn world() -> World {
    let mut w = World::new();
    w.stored_fn(A, &[]);
    w.stored_fn(B, &[]);
    // sum = a + b, charging 10 steps of its own (plus 1 the native runtime charges per call).
    w.native_fn("calc.example.net/sum", &[A, B], &[], |_, i, budget| {
        budget.charge(10)?;
        Ok(add(&i.read(A, &args(&[]))?, &i.read(B, &args(&[]))?))
    });
    // The same, but it swallows the budget error of its own charge.
    w.native_fn("calc.example.net/greedy", &[A, B], &[], |_, i, budget| {
        let _ = budget.charge(1_000);
        Ok(add(&i.read(A, &args(&[]))?, &i.read(B, &args(&[]))?))
    });
    // And one that reads nothing after swallowing it.
    w.native_fn("calc.example.net/quiet", &[], &[], |_, _, budget| {
        let _ = budget.charge(1_000);
        Ok(num("1"))
    });
    w.native_fn(FIB, &[FIB], &[("n", N)], |c, i, _| {
        let n = int(c.args["n"].scalar().unwrap());
        if n < 2 {
            return Ok(num(&n.to_string()));
        }
        let x = i.read(FIB, &args(&[("n", num(&(n - 1).to_string()))]))?;
        let y = i.read(FIB, &args(&[("n", num(&(n - 2).to_string()))]))?;
        Ok(add(&x, &y))
    });
    w.publish(&w.cell(A, &[]), "1", &t(10, 0));
    w.publish(&w.cell(B, &[]), "2", &t(10, 1));
    w
}

fn rows(w: &World) -> [i64; 3] {
    [w.db().count("receipts"), w.db().count("computed_versions"), w.db().count("versions")]
}

#[test]
fn fuel_is_runtime_steps_plus_input_reads() {
    let w = world();
    let sum = w.cellref("calc.example.net/sum", &[]);
    // 1 (the call) + 10 (its steps) + 2 (two reads) = 13.
    let short = w.engine_with(|b| b.fuel(12));
    match short.get(&sum, Some(&t(20, 0))) {
        Err(EngineError::BudgetExceeded { fuel }) => assert_eq!(fuel, 12),
        other => panic!("{other:?}"),
    }
    let exact = w.engine_with(|b| b.fuel(13));
    assert_eq!(receipt_of(&exact.get(&sum, Some(&t(20, 0))).unwrap()).value(), &num("3"));
    // Per request: the next request has its fuel again (validating the stored version reads
    // its two inputs: 2).
    let again = w.engine_with(|b| b.fuel(2));
    assert_eq!(receipt_of(&again.get(&sum, Some(&t(21, 0))).unwrap()).value(), &num("3"));
    assert_eq!(receipt_of(&again.get(&sum, Some(&t(22, 0))).unwrap()).value(), &num("3"));
    assert_eq!(DEFAULT_FUEL, 10_000_000);
}

#[test]
fn budget_exceeded_stores_nothing() {
    let w = world();
    // fib(40) needs about 5 units per cell (two runs, three reads): 100 runs out half way.
    let e = w.engine_with(|b| b.fuel(100));
    let fib = w.cellref(FIB, &[("n", num("40"))]);
    let before = rows(&w);
    match e.get(&fib, None) {
        Err(EngineError::BudgetExceeded { fuel: 100 }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(rows(&w), before, "no receipts, no versions");
    assert_eq!(e.memo_stats().entries, 0, "nothing memoised either");
    // With fuel enough, the same request succeeds and stores fib(0) .. fib(40).
    let rich = w.engine();
    assert_eq!(receipt_of(&rich.get(&fib, None).unwrap()).value(), &num("102334155"));
    assert_eq!(rows(&w)[1], before[1] + 41);
}

#[test]
fn a_runtime_that_swallows_the_budget_error_still_fails() {
    let w = world();
    let e = w.engine_with(|b| b.fuel(100));
    let before = rows(&w);
    for name in ["calc.example.net/greedy", "calc.example.net/quiet"] {
        match e.get(&w.cellref(name, &[]), Some(&t(20, 0))) {
            Err(EngineError::BudgetExceeded { fuel: 100 }) => {}
            other => panic!("{name}: {other:?}"),
        }
    }
    assert_eq!(rows(&w), before);
}

#[test]
fn a_budget_on_its_own() {
    let mut b = datumesh_engine::Budget::new(5);
    assert!(b.charge(3).is_ok());
    assert_eq!(b.remaining(), 2);
    assert!(matches!(b.charge(3), Err(EvalError::BudgetExceeded { fuel: 5 })));
    assert!(b.is_exhausted());
    assert!(b.charge(0).is_err(), "exhausted for the rest of the request");
}
