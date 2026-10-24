//! Computed cells with real lineage: signed by the mesh, asof = the latest input asof, tx =
//! the latest input tx, inputs = the exact receipt hashes read; the receipts verify through
//! the engine as resolver at level recomputed.

mod common;

use common::*;
use datumesh_core::{verify, Level, Resolver};
use datumesh_engine::{Answer, EngineError, EvalError};

const FX: &str = "rates.example.org/fx";
const PRICE: &str = "prices.example.net/price";
const TOTAL: &str = "calc.example.net/total";

/// total(item) = fx(quote: USD) + price(item): two stored inputs from two publishers.
fn world() -> World {
    let mut w = World::new();
    w.stored_fn(FX, &[("quote", r#"{"type":"string","valid":false}"#)]);
    w.stored_fn(PRICE, &[("item", r#"{"type":"string","valid":false}"#)]);
    w.native_fn(TOTAL, &[FX, PRICE], &[("item", r#"{"type":"string","valid":false}"#)], |cell, inputs, _| {
        let item = cell.args["item"].scalar().unwrap().clone();
        let fx = inputs.read(FX, &args(&[("quote", text("USD"))]))?;
        let price = inputs.read(PRICE, &args(&[("item", item)]))?;
        Ok(add(&fx, &price))
    });
    w
}

#[test]
fn a_computed_receipt_has_real_lineage_and_recomputes_through_the_engine() {
    let w = world();
    let e = w.engine();
    let fx = w.cell(FX, &[("quote", text("USD"))]);
    let price = w.cell(PRICE, &[("item", text("bolt"))]);
    // fx: recorded on the 15th, signed late (asof the 18th); price: recorded and signed on
    // the 17th.
    let h_fx = w.publish_late(&fx, "3", &t(15, 16), &t(18, 0));
    let h_price = w.publish(&price, "4", &t(17, 10));
    let a = e.get(&w.cellref(TOTAL, &[("item", text("bolt"))]), Some(&t(20, 0))).unwrap();
    let r = receipt_of(&a);
    assert_eq!(r.value(), &num("7"));
    assert_eq!(r.computed_by(), format!("{MESH}#{}", key_for(MESH).key_id()));
    assert_eq!(r.publisher(), CALC);
    assert_eq!(r.asof(), t(18, 0), "the latest input asof");
    assert_eq!(r.tx(), t(17, 10), "the latest input tx");
    let mut want = vec![h_fx.clone(), h_price.clone()];
    want.sort();
    assert_eq!(r.inputs(), want.as_slice(), "the exact receipts read");
    assert_eq!(r.issued(), NOW, "the request's clock reading");
    assert_eq!(r.licence(), "CC-BY-4.0");
    assert_eq!(r.summary(), format!("{TOTAL} computed by {MESH}."));
    assert!(r.valid().is_empty());
    // Stored as a computed version (not a stored one), with the bytes returned.
    assert_eq!(w.db().count("computed_versions"), 1);
    assert_eq!(w.db().count("versions"), 2);
    assert_eq!(bytes_of(&a), w.stored_bytes(r.hash()));
    let v = w.store.computed_asof(&r.cell().id(), &t(20, 0)).unwrap().unwrap();
    assert_eq!((v.receipt_hash.as_str(), v.tx.as_str(), v.asof.as_str()), (r.hash(), r.tx(), r.asof()));
    // Verifies at signed with the keys, and at recomputed through the engine.
    let keys = all_keys();
    assert_eq!(verify(&bytes_of(&a), &keys, None, None).level, Some(Level::Signed));
    let out = verify(&bytes_of(&a), &keys, Some(&*e as &dyn Resolver), None);
    assert!(out.ok, "{out:?}");
    assert_eq!(out.level, Some(Level::Recomputed));
    assert_eq!(w.verify_all_issued(&e, "computed_lineage"), 1);
}

#[test]
fn not_covered_inputs_propagate_unless_the_runtime_handles_them() {
    let mut w = world();
    // fallback(item) = price(item), or 0 when it has no value.
    w.native_fn("calc.example.net/fallback", &[PRICE], &[("item", r#"{"type":"string","valid":false}"#)], |c, i, _| {
        match i.read(PRICE, &args(&[("item", c.args["item"].scalar().unwrap().clone())])) {
            Ok(v) => Ok(v),
            Err(EvalError::InputNotCovered { .. }) => Ok(num("0")),
            Err(e) => Err(e),
        }
    });
    let e = w.engine();
    let fx = w.cell(FX, &[("quote", text("USD"))]);
    w.publish(&fx, "3", &t(15, 16));
    // price(bolt) has no version: total has no value either.
    let total = w.cellref(TOTAL, &[("item", text("bolt"))]);
    assert_eq!(e.get(&total, Some(&t(20, 0))).unwrap(), Answer::NotCovered);
    let fb = receipt_of(&e.get(&w.cellref("calc.example.net/fallback", &[("item", text("bolt"))]), Some(&t(20, 0))).unwrap());
    assert_eq!(fb.value(), &num("0"));
    assert!(fb.inputs().is_empty());
    assert_eq!((fb.tx(), fb.asof()), (MIN_INSTANT, MIN_INSTANT), "no inputs: the earliest instant");
    assert_eq!(w.db().count("computed_versions"), 1);
    w.verify_all_issued(&e, "computed_not_covered");
}

#[test]
fn an_input_read_twice_is_listed_once_and_valid_time_arguments_are_echoed() {
    let mut w = world();
    let day = r#"{"type":"date","valid":true}"#;
    w.native_fn("calc.example.net/twice", &[FX], &[("day", day)], |_, i, _| {
        let a = i.read(FX, &args(&[("quote", text("USD"))]))?;
        let b = i.read(FX, &args(&[("quote", text("USD"))]))?;
        Ok(add(&a, &b))
    });
    let e = w.engine();
    let fx = w.cell(FX, &[("quote", text("USD"))]);
    let h = w.publish(&fx, "3", &t(15, 16));
    let date = datumesh_core::Scalar::from_lexical(datumesh_core::Type::Date, "2024-03-15").unwrap();
    let r = receipt_of(&e.get(&w.cellref("calc.example.net/twice", &[("day", date.clone())]), Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("6"));
    assert_eq!(r.inputs(), [h].as_slice());
    assert_eq!(r.valid(), &args(&[("day", date)]));
    w.verify_all_issued(&e, "computed_twice");
}

#[test]
fn licence_and_summary_come_from_the_engine_configuration() {
    let w = world();
    let e = w.engine_with(|b| {
        b.licence("https://example.com/licence").summary(|c| format!("Total for {}.", c.args["item"].scalar().unwrap().lexical()))
    });
    w.publish(&w.cell(FX, &[("quote", text("USD"))]), "3", &t(15, 16));
    w.publish(&w.cell(PRICE, &[("item", text("nut"))]), "1", &t(15, 17));
    let r = receipt_of(&e.get(&w.cellref(TOTAL, &[("item", text("nut"))]), Some(&t(20, 0))).unwrap());
    assert_eq!(r.licence(), "https://example.com/licence");
    assert_eq!(r.summary(), "Total for nut.");
}

#[test]
fn runtime_mistakes_are_errors_and_store_nothing() {
    let mut w = world();
    w.native_fn("calc.example.net/undeclared", &[], &[], |_, i, _| i.read(FX, &args(&[("quote", text("USD"))])));
    w.native_fn("calc.example.net/badargs", &[FX], &[], |_, i, _| i.read(FX, &args(&[("nope", text("USD"))])));
    w.native_fn("calc.example.net/fails", &[], &[], |_, _, _| Err(EvalError::Runtime("boom".into())));
    w.native_fn("calc.example.net/string", &[], &[], |_, _, _| Ok(text("not a number")));
    // A function in a language with no runtime, and one whose input name is not bound.
    let d = descriptor("expr", "1", &[], &[]);
    let h = w.store.put_descriptor(&d).unwrap();
    w.store.bind_name("calc.example.net/expr", &h, BIND_TX).unwrap();
    w.native_fn("calc.example.net/unbound", &["calc.example.net/never"], &[], |_, i, _| {
        i.read("calc.example.net/never", &args(&[]))
    });
    let e = w.engine();
    w.publish(&w.cell(FX, &[("quote", text("USD"))]), "3", &t(15, 16));
    let get = |name: &str| e.get(&w.cellref(name, &[]), Some(&t(20, 0)));
    assert!(matches!(get("calc.example.net/undeclared"), Err(EngineError::UndeclaredInput { fn_name }) if fn_name == FX));
    match get("calc.example.net/badargs") {
        Err(EngineError::Invalid(e)) => assert_eq!(e.code(), datumesh_core::ErrorCode::ArgUnknown),
        other => panic!("{other:?}"),
    }
    assert!(matches!(get("calc.example.net/fails"), Err(EngineError::Runtime { message, .. }) if message.contains("boom")));
    assert!(matches!(get("calc.example.net/string"), Err(EngineError::Runtime { .. })));
    assert!(matches!(get("calc.example.net/expr"), Err(EngineError::NoRuntime { lang, .. }) if lang == "expr"));
    assert!(matches!(get("calc.example.net/unbound"), Err(EngineError::NameUnresolved { fn_name, .. }) if fn_name == "calc.example.net/never"));
    assert_eq!(w.db().count("computed_versions"), 0);
    assert_eq!(w.db().count("receipts"), 1);
}

#[test]
fn an_input_signed_after_the_clock_cannot_be_signed_over() {
    // The engine's clock is behind a publisher's: the computed asof would follow issued.
    let w = world();
    let e = w.engine();
    w.clock.set(&t(19, 0));
    w.publish_late(&w.cell(FX, &[("quote", text("USD"))]), "3", &t(15, 16), &t(18, 0));
    w.publish(&w.cell(PRICE, &[("item", text("bolt"))]), "4", &t(18, 12));
    w.clock.set(&t(18, 6));
    // Asked as of a time the clock has not reached: inputs as of then are later than issued.
    match e.get(&w.cellref(TOTAL, &[("item", text("bolt"))]), Some(&t(20, 0))) {
        Err(EngineError::Sign(err)) => assert_eq!(err.code(), datumesh_core::ErrorCode::TimeOrder),
        other => panic!("{other:?}"),
    }
    assert_eq!(w.db().count("computed_versions"), 0);
}
