//! Asof-dependent functions (SPEC 15.12, 5.5): a function whose value may change with the
//! evaluation asof alone (`asof()`, a bounded `latest`). Its receipt's asof is the evaluation
//! asof: one receipt per requested asof, never reused for another, from the memo or the
//! store. A reader of one is not itself asof-dependent.

mod common;

use common::*;
use datumesh_core::Resolver;
use datumesh_engine::Inputs;

const S: &str = "rates.example.org/s";
const AGE: &str = "calc.example.net/age";
const SEEN: &str = "calc.example.net/seen";
const PLUS: &str = "calc.example.net/plus";

/// age() = s + 1000 when asked after the 15th, else s; declared asof-dependent. seen() = the
/// same, not declared (it reads `asof()` all the same). plus() = age() + 1.
fn age(i: &mut dyn Inputs) -> Result<datumesh_core::Scalar, datumesh_engine::EvalError> {
    let s = i.read(S, &args(&[]))?;
    let late = i.asof() > t(15, 0);
    Ok(if late { add(&s, &num("1000")) } else { s })
}

fn world() -> World {
    let mut w = World::new();
    w.stored_fn(S, &[]);
    let h = w.native_fn(AGE, &[S], &[], |_, i, _| age(i));
    w.native.write().unwrap().set_asof_dependent(h);
    w.native_fn_body(SEEN, "seen", &[S], &[], |_, i, _| age(i));
    w.native_fn(PLUS, &[AGE], &[], |_, i, _| Ok(add(&i.read(AGE, &args(&[]))?, &num("1"))));
    w.publish(&w.cell(S, &[]), "5", &t(10, 0));
    w
}

fn versions_of(w: &World, name: &str) -> i64 {
    w.db()
        .client()
        .query_one("SELECT count(*) FROM computed_versions WHERE cell_id = $1", &[&w.cell(name, &[]).id()])
        .unwrap()
        .get(0)
}

#[test]
fn one_receipt_per_requested_asof() {
    let w = world();
    let e = w.engine();
    let r = w.cellref(AGE, &[]);
    let mut hashes = Vec::new();
    for (asof, value) in [(t(14, 0), "5"), (t(16, 0), "1005"), (t(17, 0), "1005")] {
        let a = receipt_of(&e.get(&r, Some(&asof)).unwrap());
        assert_eq!(a.value(), &num(value), "as of {asof}");
        assert_eq!(a.asof(), asof, "the evaluation asof");
        assert_eq!(a.tx(), t(10, 0), "the latest input tx");
        hashes.push(a.hash().to_string());
    }
    hashes.dedup();
    assert_eq!(hashes.len(), 3);
    assert_eq!(w.calls(AGE), 3);
    // Again at one of them: the memo, the same receipt.
    assert_eq!(receipt_of(&e.get(&r, Some(&t(16, 0))).unwrap()).hash(), hashes[1]);
    assert_eq!(w.calls(AGE), 3);
    assert_eq!(versions_of(&w, AGE), 3);
    w.verify_all_issued(&e, "asof_dependent");
}

#[test]
fn never_validated_for_another_asof() {
    let w = world();
    let r = w.cellref(AGE, &[]);
    let first = receipt_of(&w.engine().get(&r, Some(&t(16, 0))).unwrap());
    // A fresh engine, a later asof: the stored version (asof the 16th) answers as of the 17th
    // by its asof, but it is not reused: a new receipt as of the 17th.
    let fresh = w.engine();
    let second = receipt_of(&fresh.get(&r, Some(&t(17, 0))).unwrap());
    assert_eq!(second.asof(), t(17, 0));
    assert_ne!(second.hash(), first.hash());
    // As of its own asof, it is reused: no new version.
    assert_eq!(receipt_of(&w.engine().get(&r, Some(&t(16, 0))).unwrap()).hash(), first.hash());
    assert_eq!(versions_of(&w, AGE), 2);
    assert_eq!(w.calls(AGE), 2);
    // Each recomputes at its asof.
    assert_eq!(fresh.evaluate(first.cell(), first.asof()).as_ref(), Some(first.value()));
    w.verify_all_issued(&fresh, "asof_dependent_validation");
}

#[test]
fn a_function_reading_the_asof_undeclared_is_asof_dependent_and_not_kept() {
    let w = world();
    let e = w.engine();
    let r = w.cellref(SEEN, &[]);
    let a = receipt_of(&e.get(&r, Some(&t(16, 0))).unwrap());
    assert_eq!((a.value(), a.asof()), (&num("1005"), t(16, 0).as_str()));
    assert_eq!(versions_of(&w, SEEN), 0, "its runtime did not declare it: not kept as a version");
    let b = receipt_of(&w.engine().get(&r, Some(&t(17, 0))).unwrap());
    assert_eq!(b.asof(), t(17, 0));
    w.verify_all_issued(&e, "asof_dependent_undeclared");
}

#[test]
fn a_reader_of_an_asof_dependent_cell() {
    let w = world();
    let e = w.engine();
    let p = w.cellref(PLUS, &[]);
    let a = receipt_of(&e.get(&p, Some(&t(16, 0))).unwrap());
    assert_eq!(a.value(), &num("1006"));
    assert_eq!(a.asof(), t(16, 0), "its input's asof");
    // Later: the input is evaluated again (a new receipt), the value is the same, so the
    // reader's version validates by value.
    w.reset_calls();
    let b = receipt_of(&w.engine().get(&p, Some(&t(17, 0))).unwrap());
    assert_eq!(b.hash(), a.hash());
    assert_eq!((w.calls(AGE), w.calls(PLUS)), (1, 0));
    w.verify_all_issued(&e, "asof_dependent_reader");
}
