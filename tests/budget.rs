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
    w.verify_all_issued(&rich, "budget_fib");
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

/// top() reads c1 .. c5 (computed, each charging 3 of its own) and charges 2 of its own;
/// each run of top records the fuel its attempt used when it completes.
fn fan(w: &mut World, used: std::sync::Arc<std::sync::Mutex<Vec<u64>>>) {
    let names: Vec<String> = (1..=5).map(|k| format!("calc.example.net/c{k}")).collect();
    for n in &names {
        w.native_fn(n, &[], &[], |_, _, b| {
            b.charge(3)?;
            Ok(num("1"))
        });
    }
    let reads: Vec<&str> = names.iter().map(String::as_str).collect();
    let names2 = names.clone();
    w.native_fn("calc.example.net/top", &reads, &[], move |_, i, b| {
        let mut total = num("0");
        for n in &names2 {
            total = add(&total, &i.read(n, &args(&[]))?);
        }
        b.charge(2)?;
        used.lock().unwrap().push(b.used());
        Ok(total)
    });
}

#[test]
fn normative_fuel_does_not_depend_on_restarts() {
    // top's evaluation: 1 (the native call) + 5 reads + 2 = 8, each ci 1 + 3 = 4: 28 in all,
    // whether top was restarted five times (its inputs missing, cold) or ran once (its inputs
    // answered first).
    let used = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut cold = World::new();
    fan(&mut cold, used.clone());
    let top = cold.cellref("calc.example.net/top", &[]);
    assert_eq!(receipt_of(&cold.engine_with(|b| b.fuel(28)).get(&top, Some(&t(20, 0))).unwrap()).value(), &num("5"));
    assert_eq!(cold.calls("calc.example.net/top"), 6, "five restarts");
    assert_eq!(*used.lock().unwrap(), [8], "the completed run's fuel");
    // One unit less: exceeded, cold as warm.
    let mut short = World::new();
    fan(&mut short, std::sync::Arc::default());
    match short.engine_with(|b| b.fuel(27)).get(&top, Some(&t(20, 0))) {
        Err(EngineError::BudgetExceeded { fuel: 27 }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(short.db().count("receipts"), 0);
    // Warm: the inputs answered first, top runs once with the same fuel.
    let used_warm = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut warm = World::new();
    fan(&mut warm, used_warm.clone());
    let e = warm.engine();
    for k in 1..=5 {
        e.get(&warm.cellref(&format!("calc.example.net/c{k}"), &[]), Some(&t(20, 0))).unwrap();
    }
    assert_eq!(receipt_of(&e.get(&top, Some(&t(20, 0))).unwrap()).value(), &num("5"));
    assert_eq!(warm.calls("calc.example.net/top"), 1);
    assert_eq!(*used_warm.lock().unwrap(), [8]);
    // The same receipt, cold or warm.
    assert_eq!(
        receipt_of(&cold.engine().get(&top, Some(&t(20, 0))).unwrap()).value(),
        receipt_of(&e.get(&top, Some(&t(20, 0))).unwrap()).value()
    );
}

#[test]
fn the_work_limit_bounds_restarts() {
    // Cold, top's discarded attempts cost 2 + 3 + 4 + 5 + 6 = 20 besides its last run's 8 and
    // the inputs' 20: 48 units of work. A work limit of 40 stops it; the fuel would not.
    let mut w = World::new();
    fan(&mut w, std::sync::Arc::default());
    let top = w.cellref("calc.example.net/top", &[]);
    match w.engine_with(|b| b.fuel(1000).work_limit(40)).get(&top, Some(&t(20, 0))) {
        Err(EngineError::WorkExceeded { limit: 40 }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(w.db().count("receipts"), 0);
    assert!(w.engine_with(|b| b.fuel(1000).work_limit(48)).get(&top, Some(&t(20, 0))).is_ok());
    assert_eq!(datumesh_engine::DEFAULT_WORK_FACTOR, 16);
}
