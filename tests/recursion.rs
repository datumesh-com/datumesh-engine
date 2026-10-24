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
    // name fib is read, which was not bound then: asof is the request's (NOW), tx stays the
    // latest input tx.
    assert_eq!((r.tx(), r.asof()), (MIN_INSTANT, NOW));
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
    let value = on_thread(1200, || {
        let mut w = World::in_memory();
        chain_world(&mut w);
        let e = w.engine_with(|b| b.depth_limit(100_001));
        let r = w.cellref(CHAIN, &[("n", num("100000"))]);
        let a = e.get(&r, None).unwrap();
        // Memoised: again at once, and every link is a cell.
        assert_eq!(e.get(&r, None).unwrap(), a);
        assert!(matches!(e.get(&w.cellref(CHAIN, &[("n", num("54321"))]), None).unwrap(), Answer::Receipt(_)));
        assert_eq!(e.memo_stats().entries, 100_001);
        drop(e);
        drop(w);
        receipt_of(&a).value().clone()
    });
    assert_eq!(value, num("100000"));
}
