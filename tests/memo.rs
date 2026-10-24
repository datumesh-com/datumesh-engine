//! Memo and validation: a second get makes no runtime call; a new input version makes a
//! later asof recompute once while an earlier asof keeps the old receipt; a diamond computes
//! its shared node once; historical backfill adds an older computed version; names resolve
//! as of the asof, so rebinding one later does not change historical answers.

mod common;

use common::*;
use datumesh_engine::{Answer, EngineError};

const FX: &str = "rates.example.org/fx";
const PRICE: &str = "prices.example.net/price";
const TOTAL: &str = "calc.example.net/total";
const ITEM: &str = r#"{"type":"string","valid":false}"#;

fn total(cell: &datumesh_core::Cell, i: &mut dyn datumesh_engine::Inputs) -> Result<datumesh_core::Scalar, datumesh_engine::EvalError> {
    let item = cell.args["item"].scalar().unwrap().clone();
    let fx = i.read(FX, &args(&[("quote", text("USD"))]))?;
    let price = i.read(PRICE, &args(&[("item", item)]))?;
    Ok(add(&fx, &price))
}

fn world() -> World {
    let mut w = World::new();
    w.stored_fn(FX, &[("quote", ITEM)]);
    w.stored_fn(PRICE, &[("item", ITEM)]);
    w.native_fn(TOTAL, &[FX, PRICE], &[("item", ITEM)], |c, i, _| total(c, i));
    w
}

#[test]
fn a_second_get_makes_no_runtime_call() {
    let w = world();
    let e = w.engine();
    w.publish(&w.cell(FX, &[("quote", text("USD"))]), "3", &t(15, 16));
    w.publish(&w.cell(PRICE, &[("item", text("bolt"))]), "4", &t(16, 10));
    let r = w.cellref(TOTAL, &[("item", text("bolt"))]);
    let first = e.get(&r, Some(&t(20, 0))).unwrap();
    assert_eq!(w.calls(TOTAL), 1);
    assert_eq!(e.get(&r, Some(&t(20, 0))).unwrap(), first);
    // Any asof the answer holds for, and "now" as the clock moves.
    assert_eq!(e.get(&r, Some(&t(16, 10))).unwrap(), first);
    assert_eq!(e.get(&r, None).unwrap(), first);
    w.clock.set("2026-06-01T00:00:00.000000Z");
    assert_eq!(e.get(&r, None).unwrap(), first);
    assert_eq!(w.calls(TOTAL), 1);
    // A fresh engine (empty memo) validates the stored version instead of recomputing.
    let fresh = w.engine();
    assert_eq!(fresh.get(&r, Some(&t(25, 0))).unwrap(), first);
    assert_eq!(w.calls(TOTAL), 1);
    assert_eq!(w.db().count("computed_versions"), 1);
    // Before the inputs' asofs: no value.
    assert_eq!(e.get(&r, Some(&t(16, 9))).unwrap(), Answer::NotCovered);
    w.verify_all_issued(&e, "memo_second_get");
}

#[test]
fn a_new_input_version_recomputes_later_asofs_once_and_keeps_earlier_ones() {
    let w = world();
    let e = w.engine();
    w.publish(&w.cell(FX, &[("quote", text("USD"))]), "3", &t(15, 16));
    let price = w.cell(PRICE, &[("item", text("bolt"))]);
    w.publish(&price, "4", &t(16, 10));
    let r = w.cellref(TOTAL, &[("item", text("bolt"))]);
    let old = e.get(&r, Some(&t(20, 0))).unwrap();
    assert_eq!(receipt_of(&old).value(), &num("7"));
    w.publish(&price, "5", &t(21, 0));
    w.reset_calls();
    let new = e.get(&r, Some(&t(22, 0))).unwrap();
    assert_eq!(receipt_of(&new).value(), &num("8"));
    assert_eq!(receipt_of(&new).tx(), t(21, 0));
    assert_eq!(w.calls(TOTAL), 1, "recomputed once");
    assert_eq!(e.get(&r, Some(&t(22, 0))).unwrap(), new);
    assert_eq!(e.get(&r, Some(&t(23, 0))).unwrap(), new);
    assert_eq!(w.calls(TOTAL), 1);
    // Earlier asofs: the old receipt, without a runtime call, from the memo or the store.
    assert_eq!(e.get(&r, Some(&t(20, 0))).unwrap(), old);
    assert_eq!(e.get(&r, Some(&t(20, 23))).unwrap(), old);
    assert_eq!(w.engine().get(&r, Some(&t(20, 23))).unwrap(), old);
    assert_eq!(w.calls(TOTAL), 1);
    assert_eq!(w.db().count("computed_versions"), 2);
    w.verify_all_issued(&e, "memo_new_version");
}

#[test]
fn a_diamond_computes_its_shared_node_once() {
    let mut w = World::new();
    const S: &str = "rates.example.org/s";
    const SHARED: &str = "calc.example.net/shared";
    w.stored_fn(S, &[]);
    w.native_fn(SHARED, &[S], &[], |_, i, _| Ok(add(&i.read(S, &args(&[]))?, &num("1"))));
    w.native_fn("calc.example.net/left", &[SHARED], &[], |_, i, _| Ok(add(&i.read(SHARED, &args(&[]))?, &num("10"))));
    w.native_fn("calc.example.net/right", &[SHARED], &[], |_, i, _| Ok(add(&i.read(SHARED, &args(&[]))?, &num("100"))));
    w.native_fn("calc.example.net/top", &["calc.example.net/left", "calc.example.net/right"], &[], |_, i, _| {
        let l = i.read("calc.example.net/left", &args(&[]))?;
        let r = i.read("calc.example.net/right", &args(&[]))?;
        Ok(add(&l, &r))
    });
    let e = w.engine();
    let s = w.cell(S, &[]);
    w.publish(&s, "1", &t(15, 0));
    let top = w.cellref("calc.example.net/top", &[]);
    assert_eq!(receipt_of(&e.get(&top, Some(&t(20, 0))).unwrap()).value(), &num("114"));
    assert_eq!(w.calls(SHARED), 1);
    assert_eq!(w.db().count("computed_versions"), 4);
    // A change below the diamond: the shared node again once.
    w.publish(&s, "2", &t(21, 0));
    assert_eq!(receipt_of(&e.get(&top, Some(&t(22, 0))).unwrap()).value(), &num("116"));
    assert_eq!(w.calls(SHARED), 2);
    assert_eq!(w.db().count("computed_versions"), 8);
    w.verify_all_issued(&e, "memo_diamond");
}

#[test]
fn historical_backfill_adds_an_older_computed_version_and_leaves_versions_alone() {
    let w = world();
    let e = w.engine();
    let fx = w.cell(FX, &[("quote", text("USD"))]);
    let price = w.cell(PRICE, &[("item", text("bolt"))]);
    w.publish(&fx, "3", "2024-06-01T00:00:00.000000Z");
    w.publish(&price, "4", "2024-06-01T00:00:01.000000Z");
    w.publish(&fx, "5", "2025-06-01T00:00:00.000000Z");
    w.publish(&price, "6", "2025-06-01T00:00:01.000000Z");
    let r = w.cellref(TOTAL, &[("item", text("bolt"))]);
    // Today's state first.
    let today = receipt_of(&e.get(&r, None).unwrap());
    assert_eq!(today.value(), &num("11"));
    assert_eq!(today.tx(), "2025-06-01T00:00:01.000000Z");
    let versions = w.db().count("versions");
    assert_eq!(w.db().count("computed_versions"), 1);
    // A year earlier: a new computed version with the older tx.
    let year_ago = receipt_of(&e.get(&r, Some("2025-01-01T00:00:00.000000Z")).unwrap());
    assert_eq!(year_ago.value(), &num("7"));
    assert_eq!(year_ago.tx(), "2024-06-01T00:00:01.000000Z");
    assert_eq!(year_ago.asof(), "2024-06-01T00:00:01.000000Z");
    assert_eq!(w.db().count("computed_versions"), 2);
    assert_eq!(w.db().count("versions"), versions, "versions untouched");
    let v = w.store.computed_asof(&today.cell().id(), "2025-01-01T00:00:00.000000Z").unwrap().unwrap();
    assert_eq!(v.receipt_hash, year_ago.hash());
    // Today's answer is unchanged, from a fresh engine too.
    assert_eq!(receipt_of(&w.engine().get(&r, None).unwrap()).hash(), today.hash());
    w.verify_all_issued(&e, "memo_backfill");
}

#[test]
fn names_resolve_as_of_the_asof_and_rebinding_later_keeps_historical_answers() {
    let mut w = world();
    let e = w.engine();
    w.publish(&w.cell(FX, &[("quote", text("USD"))]), "3", &t(15, 16));
    w.publish(&w.cell(PRICE, &[("item", text("bolt"))]), "4", &t(16, 10));
    let r = w.cellref(TOTAL, &[("item", text("bolt"))]);
    let before = e.get(&r, Some(&t(20, 0))).unwrap();
    // Rebind the function: total + 1000, from the 25th.
    let h2 = w.native_fn_body("calc.example.net/total_v2", "total v2", &[FX, PRICE], &[("item", ITEM)], |c, i, _| {
        Ok(add(&total(c, i)?, &num("1000")))
    });
    w.store.bind_name(TOTAL, &h2, &t(25, 0)).unwrap();
    assert_eq!(e.get(&r, Some(&t(20, 0))).unwrap(), before);
    assert_eq!(w.engine().get(&r, Some(&t(24, 23))).unwrap(), before);
    let after = receipt_of(&e.get(&r, Some(&t(26, 0))).unwrap());
    assert_eq!(after.value(), &num("1007"));
    assert_eq!(after.cell().fn_hash, h2);
    // Rebind an input's name, from the 27th: a new price function with its own cells.
    let p2 = w.store.put_descriptor(&descriptor("stored", "v2", &[], &[("item", ITEM)])).unwrap();
    w.store.bind_name(PRICE, &p2, &t(27, 0)).unwrap();
    let price2 = datumesh_core::Cell { fn_hash: p2.clone(), ..w.cell(PRICE, &[("item", text("bolt"))]) };
    w.publish(&price2, "40", &t(27, 1));
    // The memoised answer at the 26th held for every later asof, but not once the input's
    // name means another function: by memo and by validation of the stored version alike.
    let later = receipt_of(&e.get(&r, Some(&t(28, 0))).unwrap());
    assert_eq!(later.value(), &num("1043"));
    assert_eq!(receipt_of(&w.engine().get(&r, Some(&t(28, 0))).unwrap()).hash(), later.hash());
    assert_eq!(receipt_of(&e.get(&r, Some(&t(26, 0))).unwrap()).hash(), after.hash());
    // Between the rebinding and the new function's first version: no value.
    assert_eq!(e.get(&r, Some(&t(27, 0))).unwrap(), Answer::NotCovered);
    w.verify_all_issued(&e, "memo_names");
}

#[test]
fn an_undeclared_input_fails() {
    let mut w = world();
    w.native_fn("calc.example.net/sneaky", &[FX], &[], |_, i, _| i.read(PRICE, &args(&[("item", text("bolt"))])));
    let e = w.engine();
    w.publish(&w.cell(PRICE, &[("item", text("bolt"))]), "4", &t(16, 10));
    match e.get(&w.cellref("calc.example.net/sneaky", &[]), Some(&t(20, 0))) {
        Err(EngineError::UndeclaredInput { fn_name }) => assert_eq!(fn_name, PRICE),
        other => panic!("{other:?}"),
    }
}

#[test]
fn without_a_memo_every_request_validates_through_the_store() {
    let w = world();
    let e = w.engine_with(|b| b.memo_capacity(0));
    w.publish(&w.cell(FX, &[("quote", text("USD"))]), "3", &t(15, 16));
    w.publish(&w.cell(PRICE, &[("item", text("bolt"))]), "4", &t(16, 10));
    let r = w.cellref(TOTAL, &[("item", text("bolt"))]);
    let first = e.get(&r, Some(&t(20, 0))).unwrap();
    assert_eq!(e.get(&r, Some(&t(21, 0))).unwrap(), first);
    assert_eq!(w.calls(TOTAL), 1);
    assert_eq!(e.memo_stats().entries, 0);
}
