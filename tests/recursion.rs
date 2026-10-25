//! Recursion: a function reading its own name with other arguments (SecDB fib), each call a
//! memoised cell; a cell reappearing on the evaluation stack is a Cycle; the depth limit;
//! a 100,000-deep chain on a 2 MiB thread stack under a raised limit.

mod common;

use common::*;
use datumesh_engine::{Answer, EngineError};

const FIB: &str = "calc.example.net/fib";
const CHAIN: &str = "calc.example.net/chain";
const N: &str = r#"{"type":"number","unit":"1","valid":false}"#;

fn fib_world(w: &mut World) {
    w.native_fn(FIB, &[FIB], &[("n", N)], |c, i, _| {
        let n = int(c.args["n"].scalar().unwrap());
        if n < 2 {
            return Ok(num(&n.to_string()));
        }
        let a = i.read(FIB, &args(&[("n", num(&(n - 1).to_string()))]))?;
        let b = i.read(FIB, &args(&[("n", num(&(n - 2).to_string()))]))?;
        Ok(add(&a, &b))
    });
}

/// chain(n) = chain(n - 1) + 1, chain(0) = 0.
fn chain_world(w: &mut World) {
    w.native_fn(CHAIN, &[CHAIN], &[("n", N)], |c, i, _| {
        let n = int(c.args["n"].scalar().unwrap());
        if n == 0 {
            return Ok(num("0"));
        }
        Ok(add(&i.read(CHAIN, &args(&[("n", num(&(n - 1).to_string()))]))?, &num("1")))
    });
}

#[test]
fn fib_90_is_exact_and_every_fib_k_is_a_memoised_cell() {
    let mut w = World::new();
    fib_world(&mut w);
    let e = w.engine();
    let fib = |k: u32| w.cellref(FIB, &[("n", num(&k.to_string()))]);
    let r = receipt_of(&e.get(&fib(90), None).unwrap());
    assert_eq!(r.value(), &num("2880067194370816120"));
    // fib(0) .. fib(90): 91 cells, each a computed version with its own receipt.
    assert_eq!(w.db().count("computed_versions"), 91);
    // fib(0) and fib(1) ran once; fib(2) three times (fib(1), then fib(0), missing); each
    // fib(k), k >= 3, twice: once to find fib(k - 1) missing, once to finish (fib(k - 2) was
    // answered on the way).
    assert_eq!(w.calls(FIB), 2 + 3 + 2 * 88);
    // fib(0) and fib(1) read nothing: tx and asof are the earliest instant. Above them the
    // name fib is read, which was bound only then: asof is the start of that binding, tx
    // stays the latest input tx.
    assert_eq!((r.tx(), r.asof()), (MIN_INSTANT, BIND_TX));
    w.reset_calls();
    for k in [0, 1, 2, 45, 89, 90] {
        let a = receipt_of(&e.get(&fib(k), None).unwrap());
        assert_eq!(a.cell().args["n"].scalar().unwrap(), &num(&k.to_string()));
    }
    assert_eq!(w.calls(FIB), 0, "every fib(k) is memoised");
    // fib(90) read exactly fib(89) and fib(88).
    let f89 = receipt_of(&e.get(&fib(89), None).unwrap());
    let f88 = receipt_of(&e.get(&fib(88), None).unwrap());
    let mut want = vec![f89.hash().to_string(), f88.hash().to_string()];
    want.sort();
    assert_eq!(r.inputs(), want.as_slice());
    // A fresh engine validates them from the store without running anything.
    let fresh = w.engine();
    assert_eq!(receipt_of(&fresh.get(&fib(90), None).unwrap()).hash(), r.hash());
    assert_eq!(w.calls(FIB), 0);
    assert_eq!(w.verify_all_issued(&e, "recursion_fib"), 91);
}

#[test]
fn fib_above_its_base_cases_answers_from_the_start_of_its_names_binding() {
    // fib(0) and fib(1) are known for all time (asof the earliest instant); fib(10) relies on
    // the name fib, bound at BIND_TX. Asked as of two different instants after the binding,
    // it is one receipt, one computed version, whose asof is the binding's start; it
    // recomputes there; a fresh engine validates it as of a later instant still.
    let mut w = World::new();
    fib_world(&mut w);
    let e = w.engine();
    let fib = |k: u32| w.cellref(FIB, &[("n", num(&k.to_string()))]);
    let first = receipt_of(&e.get(&fib(10), Some("2024-05-01T00:00:00.000000Z")).unwrap());
    let fresh = w.engine();
    let second = receipt_of(&fresh.get(&fib(10), Some("2025-09-01T00:00:00.000000Z")).unwrap());
    assert_eq!(first.value(), &num("55"));
    assert_eq!(second.hash(), first.hash(), "one receipt");
    assert_eq!((first.tx(), first.asof()), (MIN_INSTANT, BIND_TX), "asof: the start of fib's binding");
    let versions: i64 = w
        .db()
        .client()
        .query_one("SELECT count(*) FROM computed_versions WHERE cell_id = $1", &[&first.cell().id()])
        .unwrap()
        .get(0);
    assert_eq!(versions, 1, "one computed version");
    for k in [0, 1] {
        let base = receipt_of(&e.get(&fib(k), Some("2024-05-01T00:00:00.000000Z")).unwrap());
        assert_eq!((base.tx(), base.asof()), (MIN_INSTANT, MIN_INSTANT), "fib({k}) relies on no name");
    }
    // It recomputes at its asof: the value there, and through verify.
    use datumesh_core::Resolver;
    assert_eq!(e.evaluate(first.cell(), BIND_TX).as_ref(), Some(first.value()));
    let out = datumesh_core::verify(&w.stored_bytes(first.hash()), &all_keys(), Some(&*fresh as &dyn Resolver), None);
    assert_eq!(out.level, Some(datumesh_core::Level::Recomputed), "{out:?}");
    // Validation at a later asof accepts it: a fresh engine, no runtime call.
    w.reset_calls();
    let later = w.engine().get(&fib(10), Some("2026-06-01T00:00:00.000000Z")).unwrap();
    assert_eq!(receipt_of(&later).hash(), first.hash());
    assert_eq!(w.calls(FIB), 0);
    // Before the binding the name means nothing.
    assert_eq!(e.get(&fib(10), Some("2019-12-31T23:59:59.999999Z")).unwrap(), Answer::NOT_COVERED);
    w.verify_all_issued(&e, "recursion_fib_binding");
}

#[test]
fn a_cell_reappearing_on_the_stack_is_a_cycle() {
    let mut w = World::new();
    w.native_fn("calc.example.net/f", &["calc.example.net/f"], &[("x", N)], |c, i, _| {
        i.read("calc.example.net/f", &args(&[("x", c.args["x"].scalar().unwrap().clone())]))
    });
    // g(x) = h(x), h(x) = g(x).
    w.native_fn("calc.example.net/g", &["calc.example.net/h"], &[("x", N)], |c, i, _| {
        i.read("calc.example.net/h", &args(&[("x", c.args["x"].scalar().unwrap().clone())]))
    });
    w.native_fn("calc.example.net/h", &["calc.example.net/g"], &[("x", N)], |c, i, _| {
        i.read("calc.example.net/g", &args(&[("x", c.args["x"].scalar().unwrap().clone())]))
    });
    let e = w.engine();
    for name in ["calc.example.net/f", "calc.example.net/g"] {
        let cell = w.cell(name, &[("x", num("1"))]);
        match e.get(&w.cellref(name, &[("x", num("1"))]), None) {
            Err(EngineError::Cycle { cell_id }) => assert_eq!(cell_id, cell.id(), "{name}"),
            other => panic!("{name}: {other:?}"),
        }
    }
    assert_eq!(w.db().count("receipts"), 0);
}

#[test]
fn the_depth_limit_is_enforced() {
    let mut w = World::new();
    chain_world(&mut w);
    let shallow = w.engine_with(|b| b.depth_limit(100));
    let chain = |k: u32| w.cellref(CHAIN, &[("n", num(&k.to_string()))]);
    // 100 frames: chain(99) .. chain(0) fit, chain(100) does not.
    assert!(matches!(shallow.get(&chain(100), None), Err(EngineError::DepthExceeded { limit: 100 })));
    assert_eq!(w.db().count("receipts"), 0, "nothing stored");
    assert_eq!(receipt_of(&shallow.get(&chain(99), None).unwrap()).value(), &num("99"));
    // The default limit is 10,000.
    let e = w.engine();
    assert!(matches!(e.get(&chain(10_000), None), Err(EngineError::DepthExceeded { limit: 10_000 })));
    assert_eq!(receipt_of(&e.get(&chain(1_000), None).unwrap()).value(), &num("1000"));
    w.verify_all_issued(&e, "recursion_depth");
}

#[test]
fn a_100000_deep_chain_runs_on_a_2_mib_stack_under_a_raised_limit() {
    // On PostgreSQL: 100,001 receipts signed, verified by the store and stored, in one request.
    let value = on_thread(1800, || {
        let started = std::time::Instant::now();
        let mut w = World::new();
        chain_world(&mut w);
        let e = w.engine_with(|b| b.depth_limit(100_001));
        let r = w.cellref(CHAIN, &[("n", num("100000"))]);
        let a = e.get(&r, None).unwrap();
        // Memoised: again at once, and every link is a cell.
        assert_eq!(e.get(&r, None).unwrap(), a);
        assert!(matches!(e.get(&w.cellref(CHAIN, &[("n", num("54321"))]), None).unwrap(), Answer::Receipt(_)));
        assert_eq!(e.memo_stats().entries, 100_001);
        assert_eq!(w.db().count("computed_versions"), 100_001);
        eprintln!("100,000-deep chain on PostgreSQL: {:.1} s", started.elapsed().as_secs_f64());
        drop(e);
        drop(w);
        receipt_of(&a).value().clone()
    });
    assert_eq!(value, num("100000"));
}
