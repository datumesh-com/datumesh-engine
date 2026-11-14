//! Suspension (SPEC 9.9, store 0.4.1; engine 0.4.2): a write the engine makes under a
//! suspended namespace is refused by the store with `Error::NamespaceSuspended`, and the
//! engine surfaces it as the typed `EngineError::NamespaceSuspended` (not wrapped in
//! `EngineError::Store`), signing and storing nothing. Reads, verification and serving the
//! receipts already stored go on. The engine's tests run on `PgStore` only (the `Store` trait
//! is sealed and the tests have no SQLite world), so this is the one backend tested.

mod common;

use common::*;
use datumesh_core::{Fault, Resolver};
use datumesh_engine::{Answer, EngineError};

const S: &str = "rates.example.org/s";
const DOUBLE: &str = "calc.example.net/double";
const READER: &str = "calc.example.net/reader";
const BAD: &str = "calc.example.net/bad";
const SUSPENDED_AT: &str = "2026-02-01T00:00:00.000000Z";
const REINSTATED_AT: &str = "2026-02-02T00:00:00.000000Z";

/// double() = 2 * s; reader() = double() + 1; bad() faults (E_FORMULA_DIV_ZERO).
fn world() -> World {
    let mut w = World::new();
    w.stored_fn(S, &[]);
    w.native_fn(DOUBLE, &[S], &[], |_, i, _| Ok(add(&i.read(S, &args(&[]))?, &i.read(S, &args(&[]))?)));
    w.native_fn(READER, &[DOUBLE], &[], |_, i, _| Ok(add(&i.read(DOUBLE, &args(&[]))?, &num("1"))));
    w.native_fn(BAD, &[], &[], |_, _, _| Err(datumesh_engine::EvalError::Fault(Fault::DivZero)));
    w
}

fn suspend(w: &World, host: &str) {
    let ns = w.store.record_namespace_failure(host, SUSPENDED_AT, 1).unwrap().unwrap();
    assert_eq!(ns.suspended_at.as_deref(), Some(SUSPENDED_AT));
}

fn is_suspended(r: Result<Answer, EngineError>, host: &str) {
    match r {
        Err(EngineError::NamespaceSuspended { host: h, suspended_at }) => {
            assert_eq!((h.as_str(), suspended_at.as_str()), (host, SUSPENDED_AT));
        }
        other => panic!("not EngineError::NamespaceSuspended: {other:?}"),
    }
}

#[test]
fn a_suspended_mesh_namespace_refuses_to_sign_with_a_typed_error_and_stores_nothing() {
    let w = world();
    let e = w.engine();
    w.publish(&w.cell(S, &[]), "5", &t(10, 0));
    suspend(&w, MESH);
    let before = rows(&w);
    is_suspended(e.get(&w.cellref(DOUBLE, &[]), Some(&t(20, 0))), MESH);
    // A reader of a cell that cannot be signed cannot be signed either.
    is_suspended(e.get(&w.cellref(READER, &[]), Some(&t(20, 0))), MESH);
    assert_eq!(rows(&w), before, "no receipt, no version, no manifest");
    assert!(w.issued_receipts().is_empty(), "signs nothing");
    // The error is its own variant, not a store failure in disguise, and it reads well.
    let err = e.get(&w.cellref(DOUBLE, &[]), Some(&t(20, 0))).unwrap_err();
    assert!(!matches!(err, EngineError::Store(_)), "{err:?}");
    assert!(err.to_string().contains(MESH) && err.to_string().contains(SUSPENDED_AT), "{err}");
    // Asked again (nothing was memoised as an answer): the same refusal, not a value.
    is_suspended(e.get(&w.cellref(DOUBLE, &[]), Some(&t(20, 0))), MESH);
    // As a resolver: no value, and nothing signed.
    assert_eq!(e.evaluate(&w.cell(DOUBLE, &[]), &t(20, 0)), None);
    assert_eq!(rows(&w), before);
}

#[test]
fn reads_verification_and_existing_receipts_go_on_under_suspension() {
    let w = world();
    let e = w.engine();
    let s = w.cell(S, &[]);
    w.publish(&s, "5", &t(10, 0));
    let first = bytes_of(&e.get(&w.cellref(DOUBLE, &[]), Some(&t(20, 0))).unwrap());
    let reader = bytes_of(&e.get(&w.cellref(READER, &[]), Some(&t(20, 0))).unwrap());
    let before = rows(&w);
    suspend(&w, MESH);

    // The same engine and a cold one serve the receipts already signed, byte for byte.
    let cold = w.engine();
    for eng in [&e, &cold] {
        assert_eq!(bytes_of(&eng.get(&w.cellref(DOUBLE, &[]), Some(&t(20, 0))).unwrap()), first);
        assert_eq!(bytes_of(&eng.get(&w.cellref(READER, &[]), Some(&t(20, 0))).unwrap()), reader);
        // Stored cells are read as ever, and a fault is still an answer.
        assert_eq!(receipt_of(&eng.get(&w.cellref(S, &[]), Some(&t(20, 0))).unwrap()).value(), &num("5"));
        assert_eq!(eng.get(&w.cellref(BAD, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    }
    assert_eq!(rows(&w), before);
    // Verification at the recomputed level goes on, the engine being the resolver.
    assert_eq!(w.verify_all_issued(&e, "suspension_verify"), 2);

    // A cell that needs a receipt not signed yet is refused; the old asof is still served.
    w.publish(&s, "7", &t(21, 0));
    is_suspended(e.get(&w.cellref(DOUBLE, &[]), Some(&t(22, 0))), MESH);
    assert_eq!(bytes_of(&e.get(&w.cellref(DOUBLE, &[]), Some(&t(20, 0))).unwrap()), first);
    assert_eq!(w.issued_receipts().len(), 2);
}

#[test]
fn another_hosts_suspension_does_not_stop_the_mesh_signing() {
    let w = world();
    let e = w.engine();
    w.publish(&w.cell(S, &[]), "5", &t(10, 0));
    // The publisher of the input is suspended: its receipts are read as ever, and the mesh
    // (whose namespace is not suspended) signs.
    suspend(&w, RATES);
    assert_eq!(receipt_of(&e.get(&w.cellref(DOUBLE, &[]), Some(&t(20, 0))).unwrap()).value(), &num("10"));
    assert_eq!(w.verify_all_issued(&e, "suspension_other_host"), 1);
}

#[test]
fn reinstating_the_namespace_lets_the_engine_sign_again() {
    let w = world();
    let e = w.engine();
    w.publish(&w.cell(S, &[]), "5", &t(10, 0));
    suspend(&w, MESH);
    is_suspended(e.get(&w.cellref(DOUBLE, &[]), Some(&t(20, 0))), MESH);
    w.store.record_namespace_success(MESH, REINSTATED_AT).unwrap();
    let a = e.get(&w.cellref(DOUBLE, &[]), Some(&t(20, 0))).unwrap();
    assert_eq!(receipt_of(&a).value(), &num("10"));
    assert_eq!(w.issued_receipts().len(), 1);
    assert_eq!(w.verify_all_issued(&e, "suspension_reinstated"), 1);
}
