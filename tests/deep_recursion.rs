//! Deep recursion splits itself (Build 2 Part 3, E4). Tests first.
//!
//! A chain deeper than the depth limit (default 10,000, unchanged) no longer fails with
//! `DepthExceeded`: the engine evaluates the deepest pending dependency first as a separate
//! root in the same request (trampolining), keeps its answer in the request memo and retries
//! the outer cell. The answer, its receipts and their hashes are those of a shallow-first
//! (warm) evaluation. Fuel and the work limit still bound the request.
//!
//! NOTE for the implementer: tests/recursion.rs `the_depth_limit_is_enforced` expects
//! `DepthExceeded` for chain(100) under `depth_limit(100)` and for chain(10_000) under the
//! default limit; the contract replaces that behaviour, so that test must be updated (or
//! `DepthExceeded` kept only where splitting cannot help). The new tests below use depths
//! beyond the limit and expect success.

mod common;

use common::*;
use datumesh_engine::EngineError;

const CHAIN: &str = "calc.example.net/chain";
const FIBM: &str = "calc.example.net/fibm";
const N: &str = r#"{"type":"number","unit":"1","valid":false}"#;
const MODULUS: i128 = 1_000_000_007;

fn chain_world(w: &mut World) {
    w.native_fn(CHAIN, &[CHAIN], &[("n", N)], |c, i, _| {
        let n = int(c.args["n"].scalar().unwrap());
        if n == 0 {
            return Ok(num("0"));
        }
        Ok(add(&i.read(CHAIN, &args(&[("n", num(&(n - 1).to_string()))]))?, &num("1")))
    });
}

/// fib(n) mod 1,000,000,007: two reads per link, depth n.
fn fibm_world(w: &mut World) {
    w.native_fn(FIBM, &[FIBM], &[("n", N)], |c, i, _| {
        let n = int(c.args["n"].scalar().unwrap());
        if n < 2 {
            return Ok(num(&n.to_string()));
        }
        let a = int(&i.read(FIBM, &args(&[("n", num(&(n - 1).to_string()))]))?);
        let b = int(&i.read(FIBM, &args(&[("n", num(&(n - 2).to_string()))]))?);
        Ok(num(&((a + b) % MODULUS).to_string()))
    });
}

fn fibm_expected(n: u32) -> i128 {
    let (mut a, mut b) = (0i128, 1i128);
    for _ in 0..n {
        (a, b) = (b, (a + b) % MODULUS);
    }
    a
}

#[test]
fn a_chain_deeper_than_the_limit_answers_cold_with_the_warm_value_and_receipt() {
    // Cold: an empty mesh, chain(15000) in one request.
    let cold = on_thread(900, || {
        let mut w = World::new();
        chain_world(&mut w);
        let e = w.engine();
        assert_eq!(datumesh_engine::DEFAULT_DEPTH_LIMIT, 10_000);
        let a = e.get(&w.cellref(CHAIN, &[("n", num("15000"))]), None).unwrap();
        assert_eq!(w.db().count("computed_versions"), 15_001, "every link is a cell with a version");
        // Every link answers from the memo afterwards.
        w.reset_calls();
        assert_eq!(e.get(&w.cellref(CHAIN, &[("n", num("15000"))]), None).unwrap(), a);
        assert_eq!(w.calls(CHAIN), 0);
        assert_eq!(receipt_of(&e.get(&w.cellref(CHAIN, &[("n", num("14000"))]), None).unwrap()).value(), &num("14000"));
        // Receipts verify (a sample: the top and the base).
        a
    });
    // Warm: chain(9000) first (within the limit), then chain(15000), in a world of its own.
    let warm = on_thread(900, || {
        let mut w = World::new();
        chain_world(&mut w);
        let e = w.engine();
        assert_eq!(receipt_of(&e.get(&w.cellref(CHAIN, &[("n", num("9000"))]), None).unwrap()).value(), &num("9000"));
        let a = e.get(&w.cellref(CHAIN, &[("n", num("15000"))]), None).unwrap();
        assert_eq!(w.db().count("computed_versions"), 15_001);
        a
    });
    let (c, h) = (receipt_of(&cold), receipt_of(&warm));
    assert_eq!(c.value(), &num("15000"));
    assert_eq!(h.value(), &num("15000"));
    assert_eq!(c.hash(), h.hash(), "cold and warm: the same receipt hash");
    assert_eq!(cold, warm);
}

#[test]
fn a_two_read_recursion_deeper_than_the_limit_is_cold_equals_warm() {
    let run = |warm_up: Option<u32>| {
        on_thread(900, move || {
            let mut w = World::new();
            fibm_world(&mut w);
            let e = w.engine();
            if let Some(k) = warm_up {
                let r = receipt_of(&e.get(&w.cellref(FIBM, &[("n", num(&k.to_string()))]), None).unwrap());
                assert_eq!(int(r.value()), fibm_expected(k));
            }
            receipt_of(&e.get(&w.cellref(FIBM, &[("n", num("12000"))]), None).unwrap())
        })
    };
    let cold = run(None);
    let warm = run(Some(9000));
    assert_eq!(int(cold.value()), fibm_expected(12_000));
    assert_eq!(cold.hash(), warm.hash());
    assert_eq!(cold.inputs(), warm.inputs());
}

#[test]
fn a_raised_or_lowered_limit_changes_nothing_about_the_answer() {
    // The limit bounds the stack, not the reach: with a small limit a deep chain still answers,
    // with the same receipt as under the default.
    let mut w = World::new();
    chain_world(&mut w);
    let small = w.engine_with(|b| b.depth_limit(50));
    let r = w.cellref(CHAIN, &[("n", num("1000"))]);
    let a = small.get(&r, None).unwrap();
    assert_eq!(receipt_of(&a).value(), &num("1000"));
    let big = w.engine();
    assert_eq!(big.get(&r, None).unwrap(), a);
}

#[test]
fn fuel_and_the_work_limit_still_bound_a_split_evaluation() {
    let mut w = World::new();
    chain_world(&mut w);
    let r = w.cellref(CHAIN, &[("n", num("15000"))]);
    // Each link costs at least 2 fuel (the call and its read): 15,000 links exceed 20,000.
    let poor = w.engine_with(|b| b.fuel(20_000));
    match poor.get(&r, None) {
        Err(EngineError::BudgetExceeded { fuel: 20_000 }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(rows(&w), [0, 0, 0, 0], "nothing stored when the budget runs out");
    // Plenty of fuel but little work: the discarded attempts and validation reads count.
    let busy = w.engine_with(|b| b.work_limit(10_000));
    match busy.get(&r, None) {
        Err(EngineError::WorkExceeded { limit: 10_000 }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(rows(&w), [0, 0, 0, 0]);
    // Neither is a depth error: DepthExceeded is gone for a chain the split can handle.
    // And a request that fits succeeds on the same world afterwards.
    let ok = w.engine();
    assert_eq!(receipt_of(&ok.get(&r, None).unwrap()).value(), &num("15000"));
}
