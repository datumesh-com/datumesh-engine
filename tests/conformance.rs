//! Resolver conformance: the core's test kit with `Engine` as a time-aware resolver.
//!
//! One check of the kit differs by design: the kit's time-aware rule is the store's tx-only
//! `version_asof` ("the latest version with tx <= asof"), while the engine answers with a
//! receipt only when its tx **and** its asof are at or before A (the user's decision). The
//! kit's EUR/CHF timeline has a late-signed first version (tx 2024-03-15T16:00, asof
//! 2024-03-16T09:00), and asks for its value as of its tx: the engine answers None there, as
//! it must. Every other check passes; the test pins that exact difference.

mod common;

use std::sync::Arc;

use common::*;
use datumesh_core::testkit::{Fixtures, Kit};
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
fn the_testkit_passes_with_the_engine_except_the_answering_rule_at_a_late_signed_tx() {
    let db = TestDb::migrated();
    let kit = Kit::new().time_aware(true);
    let engine = load(&db, kit.fixtures());
    kit.check_lazy_calls(&engine).unwrap();
    kit.check_unavailable(&engine).unwrap();
    kit.check_bytes(&engine).unwrap();
    // Not time-aware in the kit's sense: everything else of check_asof passes.
    Kit::new().check_asof(&engine).unwrap();
    let report = kit.check_asof(&engine).expect_err("the kit's tx-only rule differs at one point");
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
fn a_store_is_not_the_engine_resolver() {
    // The store's own Resolver::evaluate is tx-only (version_asof): it answers at the
    // late-signed tx where the engine does not.
    let db = TestDb::migrated();
    let kit = Kit::new().time_aware(true);
    let _engine = load(&db, kit.fixtures());
    let store: PgStore = db.store();
    assert!(kit.check_asof(&store).is_ok());
}
