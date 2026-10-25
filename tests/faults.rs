//! Faults (SPEC 15.11, 15.12; the user's decision): a formula evaluation fault gives the cell
//! no value and no receipt: `Answer::NotCovered` carrying the code, which the HTTP layer puts
//! in the envelope's `fault`. Nothing is stored for a faulted cell. A reader of a faulted
//! input faults with the same code unless it catches it.

mod common;

use common::*;
use datumesh_core::{Fault, Resolver};
use datumesh_engine::{Answer, EvalError};

const S: &str = "rates.example.org/s";
const INV: &str = "calc.example.net/inverse";
const READER: &str = "calc.example.net/reader";
const CATCHER: &str = "calc.example.net/catcher";
const CODE: &str = r#"{"type":"string","valid":false}"#;

/// inverse() = 1000 / s, E_FORMULA_DIV_ZERO when s is 0; reader() = inverse() + 1;
/// catcher() = inverse(), or 7 when it faults; fault(code) faults with `code`.
fn world() -> World {
    let mut w = World::new();
    w.stored_fn(S, &[]);
    w.native_fn(INV, &[S], &[], |_, i, _| {
        let s = int(&i.read(S, &args(&[]))?);
        if s == 0 {
            return Err(EvalError::Fault(Fault::DivZero));
        }
        Ok(num(&(1000 / s).to_string()))
    });
    w.native_fn(READER, &[INV], &[], |_, i, _| Ok(add(&i.read(INV, &args(&[]))?, &num("1"))));
    w.native_fn(CATCHER, &[INV], &[], |_, i, _| match i.read_cell(INV, &args(&[])) {
        Ok(v) => Ok(v.value),
        Err(EvalError::Fault(_)) => Ok(num("7")),
        Err(e) => Err(e),
    });
    w.native_fn("calc.example.net/fault", &[], &[("code", CODE)], |c, _, _| {
        let code = c.args["code"].scalar().unwrap().lexical().to_string();
        Err(EvalError::Fault(Fault::from_code(&code).unwrap()))
    });
    w
}

#[test]
fn a_fault_is_not_covered_with_its_code_and_stores_nothing() {
    let w = world();
    let e = w.engine();
    w.publish(&w.cell(S, &[]), "0", &t(10, 0));
    let before = rows(&w);
    assert_eq!(e.get(&w.cellref(INV, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    // The reader faults with the same code; nothing is stored for either.
    assert_eq!(e.get(&w.cellref(READER, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    assert_eq!(rows(&w), before, "no receipt, no version, no manifest");
    assert_eq!(e.memo_stats().entries, 1, "only the stored input's answer");
    // As a resolver: no value.
    assert_eq!(e.evaluate(&w.cell(INV, &[]), &t(20, 0)), None);
    // Every fault code passes through as itself.
    for f in Fault::ALL {
        let r = w.cellref("calc.example.net/fault", &[("code", text(f.as_str()))]);
        assert_eq!(e.get(&r, Some(&t(20, 0))).unwrap(), Answer::NotCovered { fault: Some(*f) }, "{f}");
    }
    assert_eq!(rows(&w), before);
    // Once s is not 0, the cells have values.
    w.publish(&w.cell(S, &[]), "4", &t(21, 0));
    assert_eq!(receipt_of(&e.get(&w.cellref(READER, &[]), Some(&t(22, 0))).unwrap()).value(), &num("251"));
    // As of before, still the fault (faults are not memoised as answers, and recomputed).
    assert_eq!(e.get(&w.cellref(READER, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    w.verify_all_issued(&e, "faults");
}

#[test]
fn a_reader_may_catch_a_fault() {
    let w = world();
    let e = w.engine();
    let s = w.cell(S, &[]);
    w.publish(&s, "0", &t(10, 0));
    let c = w.cellref(CATCHER, &[]);
    let r = receipt_of(&e.get(&c, Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("7"));
    assert!(r.inputs().is_empty(), "a fault is not an input");
    // Its value rests on the fault, which no receipt lists: the engine does not keep it as a
    // version, nor in the memo, so that a change below is seen.
    assert_eq!(w.db().count("computed_versions"), 0);
    w.publish(&s, "10", &t(21, 0));
    assert_eq!(receipt_of(&e.get(&c, Some(&t(22, 0))).unwrap()).value(), &num("100"));
    assert_eq!(receipt_of(&e.get(&c, Some(&t(20, 0))).unwrap()).value(), &num("7"));
    w.verify_all_issued(&e, "faults_caught");
}

#[test]
fn not_covered_is_not_a_fault() {
    let w = world();
    let e = w.engine();
    // s has no version: inverse has no value, without a fault.
    assert_eq!(e.get(&w.cellref(INV, &[]), Some(&t(20, 0))).unwrap(), Answer::NOT_COVERED);
    assert_eq!(e.get(&w.cellref(CATCHER, &[]), Some(&t(20, 0))).unwrap(), Answer::NOT_COVERED);
}
