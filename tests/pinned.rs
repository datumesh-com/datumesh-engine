//! A pinned read by hash (SPEC 16.3 `super`, formula finding H8; engine 0.4.2):
//! `Inputs::read_pinned(fn_name, fn_hash, args)` reads the cell `P/m@<fn_hash>(...)`,
//! evaluated with the descriptor stored under that hash and relying on no binding of the name.
//! Its receipt is an input of the reader as for any read.

mod common;

use std::sync::atomic::Ordering;

use common::*;
use datumesh_core::{Fault, Receipt};
use datumesh_engine::{Answer, Budget, EngineError, EvalError, FnBinding, Inputs, Member, SlicePattern};

const S: &str = "rates.example.org/s";
const P: &str = "calc.example.net/p";
const R: &str = "calc.example.net/r";
const BOTH: &str = "calc.example.net/both";
const UNDECLARED: &str = "calc.example.net/undeclared";
const MISSING: &str = "calc.example.net/missing";
const CATCH: &str = "calc.example.net/catch";
const BAD: &str = "calc.example.net/bad";
const BIND_P2: &str = "2024-03-15T00:00:00.000000Z";
const BIND_P3: &str = "2024-03-18T00:00:00.000000Z";

/// Puts a native function's descriptor (inputs `inputs`) in the store and registers its
/// closure, binding no name. Returns its hash.
fn unbound(
    w: &World,
    body: &str,
    inputs: &[&str],
    f: impl Fn(&datumesh_core::Cell, &mut dyn Inputs, &mut Budget) -> Result<datumesh_core::Scalar, EvalError>
        + Send
        + Sync
        + 'static,
) -> String {
    let d = descriptor("native_test", body, inputs, &[]);
    let h = w.store.put_descriptor(&d).unwrap();
    let counter = w.calls.lock().unwrap().entry(body.to_string()).or_default().clone();
    w.native.write().unwrap().register(h.clone(), move |c, i, b| {
        counter.fetch_add(1, Ordering::SeqCst);
        f(c, i, b)
    });
    h
}

/// Three versions of the method p() = s + 1, s + 100, s + 10000, none bound yet; s stored,
/// published 5 at t(10).
struct Methods {
    w: World,
    h1: String,
    h2: String,
    h3: String,
}

fn methods() -> Methods {
    let w = World::new();
    w.stored_fn(S, &[]);
    let h1 = unbound(&w, "p1", &[S], |_, i, _| Ok(add(&i.read(S, &args(&[]))?, &num("1"))));
    let h2 = unbound(&w, "p2", &[S], |_, i, _| Ok(add(&i.read(S, &args(&[]))?, &num("100"))));
    let h3 = unbound(&w, "p3", &[S], |_, i, _| Ok(add(&i.read(S, &args(&[]))?, &num("10000"))));
    w.publish(&w.cell(S, &[]), "5", &t(10, 0));
    Methods { w, h1, h2, h3 }
}

fn pinned_cell(hash: &str) -> datumesh_core::Cell {
    datumesh_core::Cell { ns: "calc.example.net".into(), fn_name: "p".into(), fn_hash: hash.into(), args: args(&[]) }
}

/// r() reads p pinned at `hash`.
fn reader(w: &mut World, name: &str, hash: &str) {
    let hash = hash.to_string();
    w.native_fn(name, &[P], &[], move |_, i, _| Ok(i.read_pinned(P, &hash, &args(&[]))?.value));
}

fn receipt_by_hash(w: &World, hash: &str) -> Receipt {
    Receipt::from_json(&w.stored_bytes(hash)).unwrap()
}

#[test]
fn a_pinned_read_evaluates_the_descriptor_under_the_hash_not_the_one_the_name_means() {
    let Methods { mut w, h1, h2, h3 } = methods();
    // The name now means p2 (from BIND_P2), later p3.
    w.store.bind_name(P, &h2, BIND_P2).unwrap();
    reader(&mut w, R, &h1);
    let both = (h1.clone(), h2.clone());
    w.native_fn(BOTH, &[P], &[], move |_, i, _| {
        let a = i.read_pinned(P, &both.0, &args(&[]))?.value;
        let b = i.read_pinned(P, &both.1, &args(&[]))?.value;
        Ok(add(&a, &b))
    });
    let e = w.engine();

    let r = receipt_of(&e.get(&w.cellref(R, &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("6"), "p1 (s + 1), though the name means p2");
    // Lineage: the receipt of P@h1 is the one input, as for any read.
    assert_eq!(r.inputs().len(), 1);
    let p = receipt_by_hash(&w, &r.inputs()[0]);
    assert_eq!(p.cell(), &pinned_cell(&h1));
    assert_eq!(p.value(), &num("6"));
    assert_eq!(p.signer_host(), MESH);
    assert_eq!(p.inputs().len(), 1, "p1 read the stored s");
    // The plain read of the name gives the bound one, p2: the two are different cells.
    let plain = receipt_of(&e.get(&w.cellref(P, &[]), Some(&t(20, 0))).unwrap());
    assert_eq!((plain.value(), &plain.cell().fn_hash), (&num("105"), &h2));
    // Two pins in one evaluation: two cells, two inputs, 6 + 105.
    let b = receipt_of(&e.get(&w.cellref(BOTH, &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(b.value(), &num("111"));
    let mut hashes: Vec<String> = b.inputs().to_vec();
    hashes.sort();
    let mut want = vec![p.hash().to_string(), plain.hash().to_string()];
    want.sort();
    assert_eq!(hashes, want);
    // A name rebound later does not change a pinned read as of an earlier asof either.
    w.store.bind_name(P, &h3, BIND_P3).unwrap();
    let again = receipt_of(&e.get(&w.cellref(R, &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(again.value(), &num("6"));
    assert!(w.verify_all_issued(&e, "pinned") >= 3);
}

#[test]
fn a_pinned_read_relies_on_no_binding() {
    let Methods { mut w, h1, h2, h3 } = methods();
    // p is not bound at all: a plain read cannot resolve it, a pinned read needs no name.
    reader(&mut w, R, &h1);
    w.native_fn(MISSING, &[P], &[], |_, i, _| i.read(P, &args(&[])));
    let e = w.engine();
    match e.get(&w.cellref(MISSING, &[]), Some(&t(20, 0))) {
        Err(EngineError::NameUnresolved { fn_name, .. }) => assert_eq!(fn_name, P),
        other => panic!("{other:?}"),
    }
    let r = receipt_of(&e.get(&w.cellref(R, &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("6"));
    // No binding start is a candidate for the asof: it is that of the one version of s read,
    // for the reader and for the pinned cell, though p is bound from BIND_P2 on.
    assert_eq!(r.asof(), t(10, 0));
    assert_eq!(receipt_by_hash(&w, &r.inputs()[0]).asof(), t(10, 0));
    w.store.bind_name(P, &h2, BIND_P2).unwrap();
    // Bound or rebound, the answer stands: nothing is evaluated again, by the same engine or
    // by another; and the receipt is the same.
    w.store.bind_name(P, &h3, BIND_P3).unwrap();
    let calls = (w.calls("p1"), w.calls(R));
    // R ran twice for its first request: once suspending on the pinned input, once after it
    // (a restart); nothing runs again below.
    assert_eq!(calls, (1, 2));
    let cold = w.engine();
    for eng in [&e, &cold] {
        let again = eng.get(&w.cellref(R, &[]), Some(&t(21, 0))).unwrap();
        assert_eq!(receipt_of(&again).hash(), r.hash());
    }
    assert_eq!((w.calls("p1"), w.calls(R)), calls);
    w.verify_all_issued(&e, "pinned_no_binding");
}

#[test]
fn a_pinned_read_of_a_stored_function_reads_its_cells_at_that_hash() {
    let Methods { mut w, .. } = methods();
    let hs = w.store.resolve_name(S, "9999-12-31T23:59:59.999999Z").unwrap().unwrap();
    let hh = hs.clone();
    w.native_fn(R, &[S], &[], move |_, i, _| Ok(i.read_pinned(S, &hh, &args(&[]))?.value));
    let e = w.engine();
    let stored = w.store.latest(&w.cell(S, &[]).id()).unwrap().unwrap().receipt_hash;
    let r = receipt_of(&e.get(&w.cellref(R, &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("5"));
    assert_eq!(r.inputs(), [stored]);
}

#[test]
fn a_pinned_read_needs_a_stored_descriptor_and_a_declared_input() {
    let Methods { mut w, h1, .. } = methods();
    // A hash no descriptor is stored under (well formed): a store failure, NotFound.
    let nowhere = "0".repeat(64);
    reader(&mut w, MISSING, &nowhere);
    // The name is not one of the reader's inputs: refused as any undeclared read is.
    let hh = h1.clone();
    w.native_fn(UNDECLARED, &[S], &[], move |_, i, _| Ok(i.read_pinned(P, &hh, &args(&[]))?.value));
    let e = w.engine();
    match e.get(&w.cellref(MISSING, &[]), Some(&t(20, 0))) {
        Err(EngineError::Store(se)) => assert!(
            matches!(se.store_error(), datumesh_store::Error::NotFound { what: "descriptor", key } if *key == nowhere),
            "{se:?}"
        ),
        other => panic!("{other:?}"),
    }
    match e.get(&w.cellref(UNDECLARED, &[]), Some(&t(20, 0))) {
        Err(EngineError::UndeclaredInput { fn_name }) => assert_eq!(fn_name, P),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_pinned_read_of_a_faulting_function_returns_its_fault_which_may_be_caught() {
    let Methods { mut w, .. } = methods();
    let bad = unbound(&w, "bad", &[], |_, _, _| Err(EvalError::Fault(Fault::DivZero)));
    let b = bad.clone();
    w.native_fn(BAD, &[P], &[], move |_, i, _| Ok(i.read_pinned(P, &b, &args(&[]))?.value));
    let b = bad.clone();
    w.native_fn(CATCH, &[P], &[], move |_, i, _| match i.read_pinned(P, &b, &args(&[])) {
        Err(EvalError::Fault(_)) => Ok(num("7")),
        other => Ok(other?.value),
    });
    let e = w.engine();
    assert_eq!(e.get(&w.cellref(BAD, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    assert_eq!(receipt_of(&e.get(&w.cellref(CATCH, &[]), Some(&t(20, 0))).unwrap()).value(), &num("7"));
    // Memoised as any fault is.
    assert_eq!(e.get(&w.cellref(BAD, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    assert_eq!(w.calls("bad"), 1);
    assert_eq!(Answer::NOT_COVERED, Answer::NotCovered { fault: None });
}

/// An `Inputs` of its own (a runtime's test double) that does not know pinned reads: the
/// method is provided, so this compiles unchanged and is answered, not a panic.
struct Plain;

impl Inputs for Plain {
    fn read(&mut self, _: &str, _: &datumesh_core::Args) -> Result<datumesh_core::Scalar, EvalError> {
        unreachable!()
    }
    fn read_cell(&mut self, _: &str, _: &datumesh_core::Args) -> Result<datumesh_engine::Input, EvalError> {
        unreachable!()
    }
    fn read_asof(&mut self, _: &str, _: &datumesh_core::Args, _: &str) -> Result<datumesh_engine::Input, EvalError> {
        unreachable!()
    }
    fn read_slice(&mut self, _: &str, _: &SlicePattern) -> Result<Vec<Member>, EvalError> {
        unreachable!()
    }
    fn read_versions(
        &mut self,
        _: &str,
        _: &datumesh_core::Args,
        _: &str,
        _: &str,
    ) -> Result<Vec<datumesh_engine::Input>, EvalError> {
        unreachable!()
    }
    fn binding(&mut self, _: &str, _: Option<&str>) -> Result<Option<FnBinding>, EvalError> {
        unreachable!()
    }
    fn asof(&mut self) -> String {
        unreachable!()
    }
}

#[test]
fn an_inputs_that_offers_no_pinned_reads_answers_with_an_error() {
    let e = Plain.read_pinned(P, &"0".repeat(64), &args(&[])).unwrap_err();
    assert!(matches!(e, EvalError::Runtime(_)), "{e:?}");
}
