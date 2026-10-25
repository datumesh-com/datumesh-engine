//! Resolver conformance: the core's test kit with `Engine` as a time-aware resolver, under
//! the kit's default answering rule `AnswerRule::TxAndAsof` (datumesh-core 0.3.3): a version
//! answers as of A only when its tx and its receipt's asof are both at or before A, the
//! engine's rule. Every check passes. Under the tx-only rule (`AnswerRule::TxOnly`) the kit
//! differs at the late-signed first version of its EUR/CHF timeline, as of its tx, where the
//! engine answers None, as it must.

mod common;

use std::sync::Arc;

use common::*;
use datumesh_core::testkit::{AnswerRule, Fixtures, Kit};
use datumesh_core::{verify, Level, Receipt, Resolver};
use datumesh_engine::Engine;
use datumesh_store::{PgStore, Store};

fn load(db: &TestDb, fx: &Fixtures) -> Engine {
    let s = Arc::new(db.store());
    for (host, doc) in &fx.key_documents {
        s.put_namespace(host, doc, "2024-01-01T00:00:00.000000Z").unwrap();
    }
    s.put_namespace(MESH, &key_doc(MESH, &key_for(MESH)), "2024-01-01T00:00:00.000000Z").unwrap();
    for (h, b) in &fx.descriptors {
        assert_eq!(&s.put_descriptor(b).unwrap(), h);
    }
    for (h, b) in &fx.receipts {
        assert_eq!(&s.put_receipt(b).unwrap(), h);
    }
    for v in &fx.versions {
        s.put_version(&v.cell_id, &v.tx, &v.receipt_hash).unwrap();
    }
    let store: Arc<dyn Store + Send + Sync> = s;
    Engine::builder(store, MESH).signing_key(key_for(MESH)).build().unwrap()
}

#[test]
fn the_testkit_passes_with_the_engine() {
    let db = TestDb::migrated();
    let kit = Kit::new().time_aware(true);
    let engine = load(&db, kit.fixtures());
    kit.check(&engine).unwrap();
    // Each check by itself, with the rule named (the default is TxAndAsof).
    let explicit = Kit::new().answer_rule(AnswerRule::TxAndAsof).time_aware(true);
    explicit.check_lazy_calls(&engine).unwrap();
    explicit.check_unavailable(&engine).unwrap();
    explicit.check_bytes(&engine).unwrap();
    explicit.check_asof(&engine).unwrap();
}

/// `load`, and the kit's slice fixtures (core 0.3.4): key documents, descriptors (formula
/// descriptors with `signature.order`), receipts (citing and members) and manifests.
fn load_with_slices(db: &TestDb, fx: &Fixtures) -> Engine {
    let engine = load(db, fx);
    let s = db.store();
    let sf = &fx.slices;
    for (host, doc) in &sf.key_documents {
        s.put_namespace(host, doc, "2024-01-01T00:00:00.000000Z").unwrap();
    }
    for (h, b) in &sf.descriptors {
        assert_eq!(&s.put_descriptor(b).unwrap(), h);
    }
    for (h, b) in &sf.receipts {
        assert_eq!(&s.put_receipt(b).unwrap(), h);
    }
    for (e, b) in &sf.manifests {
        assert_eq!(&s.put_manifest(b).unwrap(), e);
    }
    engine
}

#[test]
fn the_testkit_passes_with_the_engine_and_slices() {
    // The slice checks (Kit::slices): manifests by entry exactly as loaded, members from the
    // store, None for every entry not loaded; the receipts citing slices pass step 7.5 (their
    // formula functions have no runtime here: E_EVAL_UNAVAILABLE).
    let db = TestDb::migrated();
    let kit = Kit::new().time_aware(true).slices(true);
    assert!(!kit.fixtures().slices.manifests.is_empty());
    let engine = load_with_slices(&db, kit.fixtures());
    kit.check(&engine).unwrap();
    kit.check_slices(&engine).unwrap();
}

#[test]
fn under_the_tx_only_rule_the_kit_differs_at_a_late_signed_tx() {
    let db = TestDb::migrated();
    let kit = Kit::new().answer_rule(AnswerRule::TxOnly).time_aware(true);
    let engine = load(&db, kit.fixtures());
    let report = kit.check_asof(&engine).expect_err("the tx-only rule answers at the late-signed tx");
    assert_eq!(
        report.failures(),
        ["asof: timeline: evaluate as of 2024-03-15T16:00:00.000000Z gave None, want Some(Number(\"0.9581\"))"],
        "{report}"
    );
    // The point itself: the first version is late-signed and answers from its asof on.
    let v1 = kit
        .fixtures()
        .versions
        .iter()
        .find(|v| v.tx == "2024-03-15T16:00:00.000000Z" && v.value == datumesh_core::Scalar::Number("0.9581".into()))
        .unwrap();
    let r = Receipt::from_json(&db.store().receipt(&v1.receipt_hash).unwrap()).unwrap();
    assert!(r.asof() > r.tx());
    assert_eq!(engine.evaluate(r.cell(), r.tx()), None);
    assert_eq!(engine.evaluate(r.cell(), r.asof()).as_ref(), Some(r.value()));
}

#[test]
fn the_engine_recomputes_the_kits_stored_receipts_at_their_asof() {
    let db = TestDb::migrated();
    let kit = Kit::new();
    let engine = load(&db, kit.fixtures());
    let keys = kit.fixtures().keys();
    let mut seen = 0;
    for (_, bytes) in &kit.fixtures().receipts {
        let r = Receipt::from_json(bytes).unwrap();
        if r.inputs().is_empty() {
            let out = verify(bytes, &keys, Some(&engine as &dyn Resolver), None);
            assert_eq!(out.level, Some(Level::Recomputed), "{out:?}");
            seen += 1;
        }
    }
    assert!(seen >= 3, "{seen}");
}

#[test]
fn the_store_and_the_engine_answer_alike() {
    // Since datumesh-store 0.3.0 the store's own Resolver::evaluate answers with tx and asof
    // too: both pass the kit under TxAndAsof, and both differ from TxOnly at the same point.
    let db = TestDb::migrated();
    let kit = Kit::new().time_aware(true);
    let engine = load(&db, kit.fixtures());
    let store: PgStore = db.store();
    kit.check_asof(&store).unwrap();
    let tx_only = Kit::new().answer_rule(AnswerRule::TxOnly).time_aware(true);
    let (s, e) = (tx_only.check_asof(&store).unwrap_err(), tx_only.check_asof(&engine).unwrap_err());
    assert_eq!(s.failures(), e.failures());
}
