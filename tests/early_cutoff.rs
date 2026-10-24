//! Early cutoff: an input change that leaves a child's value equal causes no recomputation of
//! the parent, whose receipt is unchanged, and a fresh engine (no in-memory records) gives
//! the same: the records need not be persisted. And a backdated correction, whose new
//! computed version shares (tx, asof) with the old one.

mod common;

use common::*;
use datumesh_core::Resolver;

const S: &str = "rates.example.org/s";
const CHILD: &str = "calc.example.net/child";
const PARENT: &str = "calc.example.net/parent";

/// child = 1 if s >= 10 else 0; parent = child + 100.
fn world() -> World {
    let mut w = World::new();
    w.stored_fn(S, &[]);
    w.native_fn(CHILD, &[S], &[], |_, i, _| Ok(num(if int(&i.read(S, &args(&[]))?) >= 10 { "1" } else { "0" })));
    w.native_fn(PARENT, &[CHILD], &[], |_, i, _| Ok(add(&i.read(CHILD, &args(&[]))?, &num("100"))));
    w
}

#[test]
fn an_equal_child_value_does_not_recompute_the_parent() {
    let w = world();
    let e = w.engine();
    let s = w.cell(S, &[]);
    let parent = w.cellref(PARENT, &[]);
    let child = w.cellref(CHILD, &[]);
    w.publish(&s, "20", &t(10, 0));
    let p1 = e.get(&parent, Some(&t(11, 0))).unwrap();
    let c1 = e.get(&child, Some(&t(11, 0))).unwrap();
    assert_eq!(receipt_of(&p1).value(), &num("101"));
    let receipts = w.db().count("receipts");
    // s changes, child's value does not.
    w.publish(&s, "30", &t(12, 0));
    w.reset_calls();
    assert_eq!(e.get(&parent, Some(&t(13, 0))).unwrap(), p1, "the parent's receipt is unchanged");
    assert_eq!(w.calls(CHILD), 1, "the child is recomputed");
    assert_eq!(w.calls(PARENT), 0, "the parent is not");
    // The child keeps its version (its recomputed value equals it): nothing new is signed.
    assert_eq!(e.get(&child, Some(&t(13, 0))).unwrap(), c1);
    assert_eq!(w.db().count("receipts"), receipts + 1, "only the new stored receipt");
    assert_eq!(w.db().count("computed_versions"), 2);
    // A fresh engine, with none of the first one's in-memory records, answers the same and
    // again does not run the parent: early cutoff needs no persisted record.
    let fresh = w.engine();
    w.reset_calls();
    assert_eq!(fresh.get(&parent, Some(&t(13, 0))).unwrap(), p1);
    assert_eq!(w.calls(PARENT), 0);
    assert_eq!(w.calls(CHILD), 1);
    assert_eq!(w.db().count("receipts"), receipts + 1);
    // The parent's receipt is still correct as of later asofs: recomputed through the engine.
    let out = datumesh_core::verify(&bytes_of(&p1), &all_keys(), Some(&*fresh as &dyn Resolver), None);
    assert_eq!(out.level, Some(datumesh_core::Level::Recomputed), "{out:?}");
    // A change that does reach the child: the parent recomputes.
    w.publish(&s, "5", &t(14, 0));
    w.reset_calls();
    let p2 = receipt_of(&e.get(&parent, Some(&t(15, 0))).unwrap());
    assert_eq!(p2.value(), &num("100"));
    assert_eq!(w.calls(CHILD), 1);
    assert!(w.calls(PARENT) >= 1);
    w.verify_all_issued(&e, "early_cutoff");
}

#[test]
fn a_backdated_correction_is_answered_and_its_receipt_reused() {
    // p = a + b. a recorded at the 10th; b at the 5th, then corrected with tx the 7th: the
    // new computed version has the old one's tx (the 10th) and asof (the 10th), which the
    // store's key (cell, tx, asof) cannot hold twice.
    let mut w = World::new();
    w.stored_fn("rates.example.org/a", &[]);
    w.stored_fn("rates.example.org/b", &[]);
    w.native_fn("calc.example.net/p", &["rates.example.org/a", "rates.example.org/b"], &[], |_, i, _| {
        Ok(add(&i.read("rates.example.org/a", &args(&[]))?, &i.read("rates.example.org/b", &args(&[]))?))
    });
    let e = w.engine();
    let a = w.cell("rates.example.org/a", &[]);
    let b = w.cell("rates.example.org/b", &[]);
    w.publish(&a, "1", &t(10, 0));
    w.publish(&b, "2", &t(5, 0));
    let p = w.cellref("calc.example.net/p", &[]);
    let old = receipt_of(&e.get(&p, Some(&t(11, 0))).unwrap());
    assert_eq!(old.value(), &num("3"));
    w.publish(&b, "4", &t(7, 0));
    let new = receipt_of(&e.get(&p, Some(&t(11, 0))).unwrap());
    assert_eq!(new.value(), &num("5"));
    assert_eq!((new.tx(), new.asof()), (old.tx(), old.asof()));
    assert_eq!(w.db().count("computed_versions"), 1, "the store keeps the old version");
    let receipts = w.db().count("receipts");
    // A fresh engine, later (a new signature would have another issued, so another hash),
    // finds the receipt already signed for these inputs instead of signing another.
    w.clock.set("2026-02-01T00:00:00.000000Z");
    w.reset_calls();
    assert_eq!(receipt_of(&w.engine().get(&p, Some(&t(11, 0))).unwrap()).hash(), new.hash());
    assert_eq!(w.db().count("receipts"), receipts);
    let out = datumesh_core::verify(&w.stored_bytes(new.hash()), &all_keys(), Some(&*e as &dyn Resolver), None);
    assert_eq!(out.level, Some(datumesh_core::Level::Recomputed), "{out:?}");
}

#[test]
fn a_kept_version_counts_from_when_its_value_is_known_again() {
    // child = 1 if s >= 10 else 0; top = child + u. The child is 1 from the 10th, 0 from the
    // 12th (nobody asks), 1 again from the 14th. u changes on the 13th. Asked on the 16th,
    // the child keeps its version of the 10th (its value is 1 again), but that version's
    // value did not hold on the 13th: top's new receipt must not answer as of the 13th,
    // where its value was 0 + 5, but from the 14th, where the child is known to be 1 again.
    let mut w = World::new();
    w.stored_fn(S, &[]);
    w.stored_fn("rates.example.org/u", &[]);
    w.native_fn(CHILD, &[S], &[], |_, i, _| Ok(num(if int(&i.read(S, &args(&[]))?) >= 10 { "1" } else { "0" })));
    w.native_fn("calc.example.net/top", &[CHILD, "rates.example.org/u"], &[], |_, i, _| {
        Ok(add(&i.read(CHILD, &args(&[]))?, &i.read("rates.example.org/u", &args(&[]))?))
    });
    let e = w.engine();
    let s = w.cell(S, &[]);
    let u = w.cell("rates.example.org/u", &[]);
    let top = w.cellref("calc.example.net/top", &[]);
    w.publish(&s, "12", &t(10, 0));
    w.publish(&u, "0", &t(10, 6));
    assert_eq!(receipt_of(&e.get(&top, Some(&t(11, 0))).unwrap()).value(), &num("1"));
    w.publish(&s, "4", &t(12, 0));
    w.publish(&u, "5", &t(13, 0));
    w.publish(&s, "15", &t(14, 0));
    let r = receipt_of(&e.get(&top, Some(&t(16, 0))).unwrap());
    assert_eq!(r.value(), &num("6"));
    assert_eq!(r.asof(), t(14, 0), "from when the kept child is known to be 1 again");
    assert_eq!(r.tx(), t(13, 0), "tx is still the latest input tx");
    // As of the 13th the value was 5, and the engine says so.
    assert_eq!(receipt_of(&w.engine().get(&top, Some(&t(13, 12))).unwrap()).value(), &num("5"));
    w.verify_all_issued(&w.engine(), "early_cutoff_kept");
}
