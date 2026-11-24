//! Engine 0.6.1, E1: a local acceptance policy for licence inheritance (CONTRACT-protocol-part1,
//! DECISIONS-round-protocol D4, D4.1, D4.2). Tests first; the API is fixed by the contract:
//!
//! ```text
//! EngineBuilder::accepted_licences(impl IntoIterator<Item = String>) -> Self
//! Engine::set_accepted_licences(&self, BTreeSet<String>)
//! pub const datumesh_engine::LOCAL_DERIVATION_LICENCE: &str = "LicenseRef-datumesh-local-derivation"
//! ```
//!
//! The policy applies only when the engine's own host is unrooted (ends in `.invalid`); on a
//! rooted host it is ignored. When SPEC 8.5 refuses (`not_declared`, `declaration_not_open`, or a
//! conflict of restricted licences) and every restricted input licence is accepted, the result
//! carries the one restricted licence, or the LicenseRef when there are two or more distinct
//! ones. A refusal for a missing acceptance is `EngineError::Unlicensed`; these tests assert on
//! the licence strings appearing in the error, not on the reason variant.

mod common;

use common::*;
use datumesh_core::Cell;
use datumesh_engine::{Answer, Engine, EngineError, Overrides, LOCAL_DERIVATION_LICENCE};
use std::collections::BTreeSet;
use std::sync::Arc;

const SRC: &str = "rates.example.org/src";
const CONVERT: &str = "calc.example.net/convert";
const R1: &str = "https://rates.example.org/licences/restricted";
const R2: &str = "https://rates.example.org/licences/other-restricted";
const OPEN: &str = "CC-BY-4.0";
const T0: &str = "2024-03-15T16:00:00.000000Z";
const LOCAL: &str = "local.invalid";

fn n_arg(cell: &Cell) -> usize {
    cell.args["n"].scalar().unwrap().lexical().parse().unwrap()
}

fn world() -> World {
    let mut w = World::new();
    w.stored_fn(SRC, &[("i", r#"{"type":"string","valid":false}"#)]);
    w.native_fn(CONVERT, &[SRC], &[("n", number_arg())], |cell, inputs, _| {
        let mut values = Vec::new();
        for i in 0..n_arg(cell) {
            values.push(inputs.read(SRC, &args(&[("i", text(&i.to_string()))]))?);
        }
        Ok(sum(&values))
    });
    w.store.put_namespace(LOCAL, &key_doc(LOCAL, &key_for(LOCAL)), "2020-01-01T00:00:00.000000Z").unwrap();
    w
}

fn src(w: &World, i: usize) -> Cell {
    w.cell(SRC, &[("i", text(&i.to_string()))])
}

/// Inputs `src(0..)` with the given licences, values 1, 2, 3, ...
fn publish(w: &World, licences: &[&str]) -> Vec<String> {
    licences
        .iter()
        .enumerate()
        .map(|(i, l)| w.publish_licensed(&src(w, i), &(i + 1).to_string(), T0, l))
        .collect()
}

/// An engine on the unrooted host `local.invalid`, accepting `accepted`.
fn local(w: &World, accepted: &[&str]) -> Arc<Engine> {
    let acc: Vec<String> = accepted.iter().map(|s| s.to_string()).collect();
    let e = Arc::new(
        Engine::builder(w.store.clone(), LOCAL)
            .signing_key(key_for(LOCAL))
            .clock(Arc::new(w.clock.clone()))
            .runtime(Arc::new(SharedNative(w.native.clone())))
            .accepted_licences(acc)
            .build()
            .unwrap(),
    );
    w.engines.lock().unwrap().push(Arc::downgrade(&e));
    e
}

fn set(accepted: &[&str]) -> BTreeSet<String> {
    accepted.iter().map(|s| s.to_string()).collect()
}

fn get(w: &World, e: &Engine, n: usize) -> Result<Answer, EngineError> {
    e.get(&w.cellref(CONVERT, &[("n", num(&n.to_string()))]), None)
}

/// A refusal for a licence that is not accepted: `Unlicensed`, naming every licence of `names`.
fn refused_naming(r: Result<Answer, EngineError>, names: &[&str]) {
    match r {
        Err(e @ EngineError::Unlicensed { .. }) => {
            let text = format!("{e} {e:?}");
            for n in names {
                assert!(text.contains(n), "the refusal names {n}: {text}");
            }
        }
        other => panic!("not unlicensed: {other:?}"),
    }
}

#[test]
fn the_constant_is_the_local_derivation_licence() {
    assert_eq!(LOCAL_DERIVATION_LICENCE, "LicenseRef-datumesh-local-derivation");
}

#[test]
fn one_accepted_restricted_licence_is_carried_by_the_result() {
    let w = world();
    let hashes = publish(&w, &[R1, R1]);
    let e = local(&w, &[R1]);
    let r = receipt_of(&get(&w, &e, 2).unwrap());
    assert_eq!(r.licence(), R1);
    assert_eq!(r.value(), &num("3"));
    assert_eq!(r.signer_host(), LOCAL);
    for h in &hashes {
        assert!(r.inputs().contains(h), "lineage keeps {h}");
    }
}

#[test]
fn two_accepted_distinct_restricted_licences_give_the_local_derivation_licence() {
    let w = world();
    let hashes = publish(&w, &[R1, R2]);
    let e = local(&w, &[R1, R2]);
    let r = receipt_of(&get(&w, &e, 2).unwrap());
    assert_eq!(r.licence(), LOCAL_DERIVATION_LICENCE);
    assert_eq!(r.value(), &num("3"));
    // The lineage holds both input receipts, each with its own licence.
    assert_eq!(r.inputs().len(), 2);
    let mut got = Vec::new();
    for h in &hashes {
        assert!(r.inputs().contains(h));
        got.push(datumesh_core::Receipt::from_json(&w.stored_bytes(h)).unwrap().licence().to_string());
    }
    assert_eq!(got, vec![R1.to_string(), R2.to_string()]);
}

#[test]
fn one_of_two_accepted_is_refused_naming_the_missing_licence() {
    let w = world();
    publish(&w, &[R1, R2]);
    let e = local(&w, &[R1]);
    refused_naming(get(&w, &e, 2), &[R2]);
    assert!(w.db().count("computed_versions") == 0);
    let e = local(&w, &[R2]);
    refused_naming(get(&w, &e, 2), &[R1]);
    // None accepted: both named.
    let e = local(&w, &[]);
    refused_naming(get(&w, &e, 2), &[R1, R2]);
}

#[test]
fn an_open_licence_alongside_accepted_restricted_ones_changes_nothing() {
    let w = world();
    publish(&w, &[OPEN, R1, R2]);
    let e = local(&w, &[R1, R2]);
    let r = receipt_of(&get(&w, &e, 3).unwrap());
    assert_eq!(r.licence(), LOCAL_DERIVATION_LICENCE);
    // One restricted licence among open ones: that licence.
    let e = local(&w, &[R1]);
    let r = receipt_of(&get(&w, &e, 2).unwrap());
    assert_eq!(r.licence(), R1);
    // Open licences are never part of the acceptance: the missing restricted one is named.
    refused_naming(get(&w, &e, 3), &[R2]);
}

#[test]
fn a_rooted_host_ignores_the_policy() {
    let w = world();
    publish(&w, &[OPEN, R1]);
    // The same refusal as without the policy: not_declared.
    let plain = w.engine();
    let want = match get(&w, &plain, 2) {
        Err(EngineError::Unlicensed { reason, .. }) => reason,
        other => panic!("{other:?}"),
    };
    let e = w.engine_with(|b| b.accepted_licences(vec![R1.to_string()]));
    match get(&w, &e, 2) {
        Err(EngineError::Unlicensed { reason, .. }) => assert_eq!(reason, want),
        other => panic!("{other:?}"),
    }
    e.set_accepted_licences(set(&[R1]));
    assert!(matches!(get(&w, &e, 2), Err(EngineError::Unlicensed { .. })));
    assert!(w.issued_receipts().is_empty());
}

#[test]
fn set_accepted_licences_changes_the_next_request() {
    let w = world();
    publish(&w, &[R1, R2]);
    let e = local(&w, &[]);
    refused_naming(get(&w, &e, 2), &[R1, R2]);
    e.set_accepted_licences(set(&[R1]));
    refused_naming(get(&w, &e, 2), &[R2]);
    e.set_accepted_licences(set(&[R1, R2]));
    assert_eq!(receipt_of(&get(&w, &e, 2).unwrap()).licence(), LOCAL_DERIVATION_LICENCE);
    e.set_accepted_licences(BTreeSet::new());
    refused_naming(get(&w, &e, 2), &[R1, R2]);
}

#[test]
fn whatif_over_two_accepted_inputs_computes_under_the_local_derivation_licence() {
    let w = world();
    // src(2) is open and overridden; src(0) and src(1) are the restricted inputs.
    publish(&w, &[R1, R2, OPEN]);
    let target = w.cellref(CONVERT, &[("n", num("3"))]);
    let o = Overrides::new().value(w.cellref(SRC, &[("i", text("2"))]), num("10"));
    let e = local(&w, &[R1, R2]);
    let s = e.whatif(&target, None, &o, None).unwrap();
    assert_eq!(s.value, num("13"));
    // One of the two accepted: refused, naming the other.
    let e = local(&w, &[R1]);
    match e.whatif(&target, None, &o, None) {
        Err(err @ EngineError::Unlicensed { .. }) => {
            let text = format!("{err} {err:?}");
            assert!(text.contains(R2), "{text}");
        }
        other => panic!("{other:?}"),
    }
    // Nothing accepted: refused.
    let e = local(&w, &[]);
    assert!(matches!(e.whatif(&target, None, &o, None), Err(EngineError::Unlicensed { .. })));
    // A rooted host ignores the policy.
    let e = w.engine_with(|b| b.accepted_licences(vec![R1.to_string(), R2.to_string()]));
    assert!(matches!(e.whatif(&target, None, &o, None), Err(EngineError::Unlicensed { .. })));
    assert!(w.issued_receipts().is_empty());
}

#[test]
fn a_receipt_computed_under_acceptance_is_not_reused_after_revocation() {
    let w = world();
    publish(&w, &[R1, R2]);
    let e = local(&w, &[R1, R2]);
    let first = receipt_of(&get(&w, &e, 2).unwrap());
    assert_eq!(first.licence(), LOCAL_DERIVATION_LICENCE);
    // Memoised: the same receipt again.
    assert_eq!(receipt_of(&get(&w, &e, 2).unwrap()).hash(), first.hash());
    // Revoked (R2 no longer in the set): refused, not answered from the memo or the store.
    e.set_accepted_licences(set(&[R1]));
    refused_naming(get(&w, &e, 2), &[R2]);
    e.set_accepted_licences(BTreeSet::new());
    refused_naming(get(&w, &e, 2), &[R1, R2]);
    // Accepted again: answers (the same or a new receipt, with the same licence).
    e.set_accepted_licences(set(&[R1, R2]));
    assert_eq!(receipt_of(&get(&w, &e, 2).unwrap()).licence(), LOCAL_DERIVATION_LICENCE);
}
