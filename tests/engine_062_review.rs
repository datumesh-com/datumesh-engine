//! Engine 0.6.2, the fixes from the independent review of Part G (CONTRACT-guard-patch.md, "Decided
//! 26 Nov: fixes from the independent review of Part G"), items 1, 2, 5 and 7. Failing tests.
//!
//! * Item 1: a stored version's memo entry starts at max(receipt asof, recorded), not at its asof.
//! * Item 2: the engine calls `Store::barrier()` once per request before its first store read, so a
//!   current read made while a writer waits for the version lock sees what that writer commits, or
//!   signs before it is recorded; either way its receipt rebuilds and verifies after the commit.
//! * Item 5: a rebuild over the fuel is refused while it reads a hash-only listing bounded at fuel + 1:
//!   no member's value is loaded (no slice_values statement) and the rows read are bounded.
//! * Item 7: the memo bound reads only the next version recorded later (LIMIT 1), not the history.
//!
//! Rows are counted on the wire (tests/common/wire.rs, `Wire::take_rows`: DataRow messages).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use datumesh_core::{verify, Cell, Level, Resolver};
use datumesh_engine::{Engine, EngineError, ManifestPolicy};
use datumesh_store::{PgStore, PoolOptions};

const LEVEL: &str = "rates.example.org/station_level";
const FLOOD: &str = "calc.example.net/flood";
const DAY: &str = "2016-11-20";
const STR: &str = r#"{"type":"string","valid":false}"#;
const DATE: &str = r#"{"type":"date","valid":false}"#;
const LOCK_KEY: i64 = 7412030007;

/// station_level(station, d) stored and flood(d) = the sum of station_level(*, d), on a store that
/// records at the world's clock.
fn recording_world() -> World {
    let mut w = World::new();
    let clock = w.clock.clone();
    let pg = Arc::new(
        PgStore::connect_with(
            &w.db().url(),
            PoolOptions::new().max_size(8).recording_clock(Arc::new(move || datumesh_engine::Clock::now(&clock))),
        )
        .unwrap(),
    );
    w.store = pg.clone();
    w.pg = pg;
    w.stored_fn(LEVEL, &[("d", DATE), ("station", STR)]);
    w.native_fn(FLOOD, &[LEVEL], &[("d", DATE)], |c, i, _| {
        let d = c.args["d"].scalar().unwrap().clone();
        let members = i.read_slice(LEVEL, &pattern(&[("d", bound(d)), ("station", open())]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w
}

fn level(w: &World, station: &str) -> Cell {
    w.cell(LEVEL, &[("d", date(DAY)), ("station", text(station))])
}

fn level_ref(w: &World, station: &str) -> datumesh_core::CellRef {
    w.cellref(LEVEL, &[("d", date(DAY)), ("station", text(station))])
}

fn flood(w: &World) -> datumesh_core::CellRef {
    w.cellref(FLOOD, &[("d", date(DAY))])
}

fn mesh() -> impl FnOnce(datumesh_engine::EngineBuilder) -> datumesh_engine::EngineBuilder {
    |b| b.manifests(ManifestPolicy::MemberSetOnly).store_past_reads(false)
}

fn value_of(a: &datumesh_engine::Answer) -> datumesh_core::Scalar {
    receipt_of(a).value().clone()
}

// ---- Item 1: the memo starts at max(asof, recorded) --------------------------------------------

#[test]
fn a_correction_recorded_later_answers_in_the_memo_from_its_recorded_time() {
    let w = recording_world();
    // Station a: 100 signed and recorded the 10th; b: 200 likewise.
    w.clock.set(&t(10, 0));
    w.publish(&level(&w, "a"), "100", &t(10, 0));
    w.publish(&level(&w, "b"), "200", &t(10, 0));
    // A correction of a, signed the 11th, recorded the 14th.
    w.clock.set(&t(14, 0));
    w.publish(&level(&w, "a"), "110", &t(11, 0));
    w.clock.set(&t(16, 0));
    // One engine: current reads warm its memo with the correction (and flood over it).
    let e = w.engine_with(mesh());
    assert_eq!(value_of(&e.get(&level_ref(&w, "a"), None).unwrap()), num("110"));
    assert_eq!(value_of(&e.get(&flood(&w), None).unwrap()), num("310"));
    // As of the 13th the correction was not yet recorded here: 100, and a flood of 300.
    assert_eq!(value_of(&e.get(&level_ref(&w, "a"), Some(&t(13, 0))).unwrap()), num("100"), "the stored cell");
    assert_eq!(value_of(&e.get(&flood(&w), Some(&t(13, 0))).unwrap()), num("300"), "a computed cell reading it");
    // A fresh engine (no memo) agrees.
    let f = w.engine_with(mesh());
    assert_eq!(value_of(&f.get(&level_ref(&w, "a"), Some(&t(13, 0))).unwrap()), num("100"));
}

// ---- Item 2: a current read while a writer waits ----------------------------------------------

#[test]
fn a_current_read_while_a_writer_waits_for_the_lock_verifies_after_the_commit() {
    let w = recording_world();
    w.clock.set(&t(10, 0));
    w.publish(&level(&w, "a"), "100", &t(10, 0));
    w.publish(&level(&w, "b"), "200", &t(10, 0));
    // Another session holds the version lock; station c's writer waits behind it (clock: the 14th).
    let mut holder = w.db().client();
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let hold = std::thread::spawn(move || {
        let mut t = holder.transaction().unwrap();
        t.execute("SELECT pg_advisory_xact_lock($1)", &[&LOCK_KEY]).unwrap();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
        t.commit().unwrap();
    });
    held_rx.recv().unwrap();
    w.clock.set(&t(14, 0));
    let store = w.store.clone();
    let c = level(&w, "c");
    let bytes = w.sign_stored(&c, num("50"), &t(14, 0), &t(14, 0));
    let writer = std::thread::spawn(move || {
        let h = store.put_receipt(&bytes).unwrap();
        store.put_version(&c.id(), &t(14, 0), &h).unwrap();
    });
    std::thread::sleep(Duration::from_millis(300));
    // A current read at the 16th, while the writer waits.
    w.clock.set(&t(16, 0));
    let engine = w.engine_with(mesh());
    let reader = {
        let e = engine.clone();
        let f = flood(&w);
        std::thread::spawn(move || e.get(&f, None).unwrap())
    };
    std::thread::sleep(Duration::from_millis(400));
    release_tx.send(()).unwrap();
    hold.join().unwrap();
    writer.join().unwrap();
    let a = reader.join().unwrap();
    let r = receipt_of(&a);
    // After the commit the receipt rebuilds to its hash and verifies at level recomputed.
    let fresh = w.engine_with(mesh());
    assert!(fresh.manifest(&r.inputs()[0]).is_some(), "its list rebuilds: {}", r.value().lexical());
    let out = verify(&bytes_of(&a), &all_keys(), Some(&*fresh as &dyn Resolver), None);
    assert!(out.ok, "{out:?}");
    assert_eq!(out.level, Some(Level::Recomputed), "{out:?}");
}

// ---- Item 5: a rebuild over the fuel loads no member's value ----------------------------------

#[test]
fn a_rebuild_over_the_fuel_is_refused_without_loading_the_members_values() {
    let w = recording_world();
    w.clock.set(&t(10, 0));
    for k in 0..300 {
        w.publish(&level(&w, &format!("s{k:03}")), "1", &t(10, 0));
    }
    w.clock.set(&t(16, 0));
    let r = receipt_of(&w.engine_with(mesh()).get(&flood(&w), Some(&t(15, 0))).unwrap());
    let entry = r.inputs()[0].clone();
    let wire = common::wire::Wire::start(&w.db().config);
    let store: Arc<dyn datumesh_store::Store + Send + Sync> = Arc::new(wire.store());
    let small: Engine = Engine::builder(store, MESH)
        .signing_key(key_for(MESH))
        .clock(Arc::new(w.clock.clone()))
        .runtime(Arc::new(SharedNative(w.native.clone())))
        .manifests(ManifestPolicy::MemberSetOnly)
        .store_past_reads(false)
        .fuel(10)
        .build()
        .unwrap();
    wire.take();
    wire.take_rows();
    match small.manifest_asof(&entry, r.asof()) {
        Err(EngineError::BudgetExceeded { .. }) => {}
        other => panic!("over the fuel: {other:?}"),
    }
    let sql = wire.take();
    let rows = wire.take_rows();
    assert_eq!(
        common::wire::matching(&sql, "ORDER BY a.name), c.sort_key"),
        0,
        "no member's value is loaded (a hash-only listing): {sql:#?}"
    );
    assert!(rows <= 40, "the listing is bounded at fuel + 1 (10 + 1), not 300 members: {rows} rows");
}

// ---- Item 7: the memo bound reads only the next version --------------------------------------

#[test]
fn the_memo_bound_reads_only_the_next_version() {
    let w = World::new();
    w.stored_fn(LEVEL, &[("d", DATE), ("station", STR)]);
    let c = level(&w, "a");
    for d in 1..=28 {
        for h in [0, 6, 12, 18] {
            w.publish(&c, &format!("{d}{h}"), &format!("2024-02-{d:02}T{h:02}:00:00.000000Z"));
        }
    }
    let wire = common::wire::Wire::start(&w.db().config);
    let store: Arc<dyn datumesh_store::Store + Send + Sync> = Arc::new(wire.store());
    let e = Engine::builder(store, MESH)
        .signing_key(key_for(MESH))
        .clock(Arc::new(w.clock.clone()))
        .runtime(Arc::new(SharedNative(w.native.clone())))
        .build()
        .unwrap();
    wire.take();
    wire.take_rows();
    // The first version answers; 111 versions follow it.
    let a = e.get(&level_ref(&w, "a"), Some("2024-02-01T03:00:00.000000Z")).unwrap();
    assert_eq!(value_of(&a), num("10"));
    let rows = wire.take_rows();
    assert!(rows <= 10, "the bound reads the next version only, not the 111 after it: {rows} rows");
}

// ---- Second review (27 Nov), A1: the memo bound is the least later start ----------------------

#[test]
fn a_warmed_memo_ends_at_the_least_start_of_the_later_versions() {
    // v1 at the 10th; v2 signed the 11th with asof the 20th (late-signed), recorded the 11th; v3 at
    // the 12th. As of the 12th 06:00 v3 answers (3): v2 does not answer before its asof.
    let w = World::new();
    w.stored_fn(LEVEL, &[("d", DATE), ("station", STR)]);
    let c = level(&w, "a");
    w.publish(&c, "1", &t(10, 0));
    w.publish_late(&c, "2", &t(11, 0), &t(20, 0));
    w.publish(&c, "3", &t(12, 0));
    let fresh = w.engine();
    assert_eq!(value_of(&fresh.get(&level_ref(&w, "a"), Some(&t(12, 6))).unwrap()), num("3"));
    // One engine, its memo warmed with v1 as of the 10th 06:00: as of the 12th 06:00 still 3.
    let e = w.engine();
    assert_eq!(value_of(&e.get(&level_ref(&w, "a"), Some(&t(10, 6))).unwrap()), num("1"));
    assert_eq!(
        value_of(&e.get(&level_ref(&w, "a"), Some(&t(12, 6))).unwrap()),
        num("3"),
        "v1's memo entry ends at v3's start (the least later start), not at the next version v2's asof"
    );
}
