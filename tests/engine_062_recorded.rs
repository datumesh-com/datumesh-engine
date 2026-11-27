//! Engine 0.6.2, the guard patch G9 and G10 (CONTRACT-guard-patch.md, "Decided 26 Nov (user)"), the
//! third manifest policy, and a stronger G6 lineage test. Failing tests: nothing of it is written.
//!
//! API assumed (besides engine_062_manifest_policy.rs's):
//!
//! ```text
//! ManifestPolicy::StoreUpTo(usize)
//!     // a manifest of at most that many members is stored, a longer one is not; the member-set
//!     // row always (DATUMESH_MANIFEST_STORE_MAX above 0)
//! Engine::manifest_asof(&self, entry: &str, asof: &str) -> Result<Option<Vec<u8>>, EngineError>
//!     // G10: the manifest of `entry` as of `asof` with no member-set row: the held bytes, else the
//!     // members of the slice's kept definition as of `asof` (by recorded), hash checked.
//!     // Ok(None): no manifest and no definition of the slice (refused at once, no member read);
//!     // Err(EngineError::BudgetExceeded { .. }): the members exceed the request's fuel (the
//!     // rebuild counts against the same budget as an evaluation);
//!     // Err(any other): the hash differs (a false asof) or a member cannot be listed.
//! ```
//!
//! G9 here: the store records at its clock (datumesh-store 0.6.5's `PoolOptions::recording_clock`;
//! see the store's tests), and every as-of selection of the engine follows the store's: a member
//! answers as of A when it was recorded at or before A. Two consequences the tests assert:
//! * a computed receipt's asof is never before the recorded time of a member it read (else a rebuild
//!   as of the receipt's asof would leave that member out): here the receipt over a backfilled member
//!   has asof = the member's recorded time;
//! * the member-set row's `tx` is the latest recorded time the read saw (the reuse check, G3,
//!   compares recorded times, never signed ones).
//!
//! G10: every slice read keeps the slice's definition once (`Store::put_slice_definition`), the past
//! reads included, so `manifest_asof` needs no row.

mod common;

use std::sync::Arc;

use common::*;
use datumesh_core::{verify, Cell, Level, Manifest, Receipt, Resolver};
use datumesh_engine::{Engine, EngineError, ManifestPolicy, Overrides};
use datumesh_store::{PgStore, PoolOptions};

const LEVEL: &str = "rates.example.org/station_level";
const FLOOD: &str = "calc.example.net/flood";
const DAY: &str = "2016-11-20";
const STR: &str = r#"{"type":"string","valid":false}"#;
const DATE: &str = r#"{"type":"date","valid":false}"#;

/// The flood world of engine_062_manifest_policy on a store that records at the world's clock.
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

fn level(w: &World, station: &str, d: &str) -> Cell {
    w.cell(LEVEL, &[("d", date(d)), ("station", text(station))])
}

fn flood(w: &World) -> datumesh_core::CellRef {
    w.cellref(FLOOD, &[("d", date(DAY))])
}

/// The engine the mesh runs: no manifest, nothing stored by a past read.
fn mesh() -> impl FnOnce(datumesh_engine::EngineBuilder) -> datumesh_engine::EngineBuilder {
    |b| b.manifests(ManifestPolicy::MemberSetOnly).store_past_reads(false)
}

fn verifies(e: &dyn Resolver, bytes: &[u8], what: &str) {
    let out = verify(bytes, &all_keys(), Some(e), None);
    assert!(out.ok, "{what}: {out:?}");
    assert_eq!(out.level, Some(Level::Recomputed), "{what}: {out:?}");
}

/// [receipts, computed versions, member-set rows, manifests, slice definitions].
fn stored_rows(w: &World) -> [i64; 5] {
    [
        w.db().count("receipts"),
        w.db().count("computed_versions"),
        w.db().count("member_sets"),
        w.db().count("manifests"),
        w.db().count("slice_definitions"),
    ]
}

/// Stations a (100) and b (200) recorded at the 10th; flood(DAY) read now at the 12th (T0).
fn t0(w: &World) -> (Receipt, Vec<u8>) {
    w.clock.set(&t(10, 0));
    w.publish(&level(w, "a", DAY), "100", &t(10, 0));
    w.publish(&level(w, "b", DAY), "200", &t(10, 0));
    w.clock.set(&t(12, 0));
    let a = w.engine_with(mesh()).get(&flood(w), None).unwrap();
    (receipt_of(&a), bytes_of(&a))
}

// ---- G9: the recorded time -------------------------------------------------------------------

#[test]
fn a_backfill_with_an_old_signed_time_is_seen_now_and_left_out_before_its_arrival() {
    let w = recording_world();
    let (r0, b0) = t0(&w);
    assert_eq!(r0.value(), &num("300"));
    // Station c's reading for DAY, signed on the 9th (before every T0 member), recorded on the 14th.
    w.clock.set(&t(14, 0));
    w.publish(&level(&w, "c", DAY), "50", &t(9, 0));
    let v = w.store.latest(&level(&w, "c", DAY).id()).unwrap().unwrap();
    assert_eq!((v.tx.as_str(), v.recorded.as_str()), (t(9, 0).as_str(), t(14, 0).as_str()));
    w.clock.set(&t(16, 0));
    // (a) The next current read sees it: re-evaluated, new value.
    let calls = w.calls(FLOOD);
    let e = w.engine_with(mesh());
    let now = receipt_of(&e.get(&flood(&w), None).unwrap());
    assert_eq!(now.value(), &num("350"), "the backfilled member counts now");
    assert_eq!(w.calls(FLOOD), calls + 1, "re-evaluated: the reuse check compares recorded times");
    assert_eq!(now.asof(), t(14, 0), "the receipt's asof is not before the member's recorded time");
    let row = w.store.member_set(&now.inputs()[0]).unwrap().expect("the current read's row");
    assert_eq!(
        row.tx.as_deref(),
        Some(t(14, 0).as_str()),
        "the row's tx is the latest recorded time, not the signed 9th"
    );
    // (b) A read as of before its arrival leaves it out: the T0 version.
    assert_eq!(bytes_of(&w.engine_with(mesh()).get(&flood(&w), Some(&t(13, 0))).unwrap()), b0);
    // The old receipt still rebuilds to its hash (the member is left out as of its asof) and verifies.
    let fresh = w.engine_with(mesh());
    let m = Manifest::from_json(&fresh.manifest(&r0.inputs()[0]).expect("rebuilt")).unwrap();
    assert_eq!(m.entry(), r0.inputs()[0]);
    assert_eq!(m.members().len(), 2);
    verifies(&*fresh, &b0, "the T0 receipt");
    verifies(&*fresh, &w.stored_bytes(now.hash()), "the receipt over the backfill");
}

#[test]
fn a_correction_signed_earlier_is_refused_and_changes_nothing() {
    // Decided 27 Nov (user), "backdating on the same cell is refused": a version of an existing cell
    // signed not later than its latest signed tx is NotLater, whatever its recorded time; the answers
    // and the T0 receipt are unchanged. (Before: accepted when recorded later.)
    let w = recording_world();
    let (r0, b0) = t0(&w);
    // A correction of station a, signed on the 9th (before its version of the 10th), offered on the 14th.
    w.clock.set(&t(14, 0));
    let a = level(&w, "a", DAY);
    let bytes = w.sign_stored(&a, num("110"), &t(9, 0), &t(9, 0));
    let h = w.store.put_receipt(&bytes).unwrap();
    match w.store.put_version(&a.id(), &t(9, 0), &h) {
        Err(datumesh_store::Error::NotLater { .. }) => {}
        other => panic!("a correction signed before the cell's latest: {other:?}"),
    }
    w.clock.set(&t(16, 0));
    let e = w.engine_with(mesh());
    assert_eq!(bytes_of(&e.get(&flood(&w), None).unwrap()), b0, "nothing changes: the T0 version stands");
    assert_eq!(bytes_of(&e.get(&flood(&w), Some(&t(13, 0))).unwrap()), b0);
    verifies(&*e, &b0, "the T0 receipt");
    assert_eq!(r0.value(), &num("300"));
}

// ---- G10: a past read's list, with nothing stored --------------------------------------------

/// A resolver for verifying a receipt made by a past read: the engine, with manifests rebuilt as of
/// the receipt's asof (what the mesh's /verify and an outside verifier's `?asof=` do).
struct AsOf<'a>(&'a Engine, String);

impl Resolver for AsOf<'_> {
    fn descriptor(&self, h: &str) -> Option<std::borrow::Cow<'_, [u8]>> {
        self.0.descriptor(h)
    }
    fn receipt(&self, h: &str) -> Option<std::borrow::Cow<'_, [u8]>> {
        self.0.receipt(h)
    }
    fn manifest(&self, entry: &str) -> Option<std::borrow::Cow<'_, [u8]>> {
        self.0.manifest_asof(entry, &self.1).ok().flatten().map(std::borrow::Cow::Owned)
    }
    fn evaluate(&self, cell: &Cell, asof: &str) -> Option<datumesh_core::Scalar> {
        self.0.evaluate(cell, asof)
    }
    fn evaluation(&self, cell: &Cell, asof: &str) -> Option<datumesh_core::Evaluation> {
        self.0.evaluation(cell, asof)
    }
}

#[test]
fn a_past_read_keeps_the_slice_definition_once_and_its_list_rebuilds_with_no_row() {
    let w = recording_world();
    w.clock.set(&t(10, 0));
    w.publish(&level(&w, "a", DAY), "100", &t(10, 0));
    w.publish(&level(&w, "b", DAY), "200", &t(10, 0));
    w.clock.set(&t(14, 0));
    w.publish(&level(&w, "c", DAY), "50", &t(14, 0));
    w.clock.set(&t(16, 0));
    let before = stored_rows(&w);
    // A past read (as of the 15th), no current read ever.
    let e = w.engine_with(mesh());
    let a = e.get(&flood(&w), Some(&t(15, 0))).unwrap();
    let r = receipt_of(&a);
    assert_eq!(r.value(), &num("350"));
    let after = stored_rows(&w);
    assert_eq!(after[..4], before[..4], "no receipt, version, row or manifest");
    assert_eq!(after[4], before[4] + 1, "the slice's definition, once");
    assert_eq!(w.store.member_set(&r.inputs()[0]).unwrap(), None);
    // Varying the date stores nothing new.
    for h in [11, 13, 15] {
        e.get(&flood(&w), Some(&t(h, 12))).unwrap();
        w.engine_with(mesh()).get(&flood(&w), Some(&t(h, 0))).unwrap();
    }
    assert_eq!(stored_rows(&w), after, "nothing new per date");
    // The list as of the receipt's asof, from the definition: the bytes a stored manifest holds.
    let held = {
        let s = recording_world();
        s.clock.set(&t(10, 0));
        s.publish(&level(&s, "a", DAY), "100", &t(10, 0));
        s.publish(&level(&s, "b", DAY), "200", &t(10, 0));
        s.clock.set(&t(14, 0));
        s.publish(&level(&s, "c", DAY), "50", &t(14, 0));
        s.clock.set(&t(15, 0));
        let r = receipt_of(&s.engine_with(|b| b.manifests(ManifestPolicy::Store)).get(&flood(&s), None).unwrap());
        s.store.manifest(&r.inputs()[0]).unwrap().into_owned()
    };
    let fresh = w.engine_with(mesh());
    let bytes = fresh.manifest_asof(&r.inputs()[0], r.asof()).unwrap().expect("rebuilt from the definition");
    assert_eq!(bytes, held);
    assert_eq!(fresh.manifest(&r.inputs()[0]), None, "no row: the plain resolver has none");
    // It verifies at level recomputed with the list as of its asof.
    verifies(&AsOf(&fresh, r.asof().to_string()), &bytes_of(&a), "the past read's receipt");
    assert_eq!(stored_rows(&w), after, "the rebuild and the verification store nothing");
}

#[test]
fn a_false_asof_an_unknown_slice_and_the_budget() {
    let w = recording_world();
    w.clock.set(&t(10, 0));
    for k in 0..20 {
        w.publish(&level(&w, &format!("s{k:02}"), DAY), "1", &t(10, 0));
    }
    w.clock.set(&t(16, 0));
    let e = w.engine_with(mesh());
    let r = receipt_of(&e.get(&flood(&w), Some(&t(15, 0))).unwrap());
    let entry = r.inputs()[0].clone();
    // A false asof (before every member): the hash differs, refused, never guessed.
    match e.manifest_asof(&entry, &t(9, 0)) {
        Err(EngineError::BudgetExceeded { .. }) | Ok(_) => panic!("a false asof must be refused"),
        Err(_) => {}
    }
    // A slice the mesh has no definition for: nothing, at once.
    let unknown = format!("slice:{}:{}", "a".repeat(64), "b".repeat(64));
    assert_eq!(e.manifest_asof(&unknown, &t(15, 0)).unwrap(), None);
    // The rebuild counts against the request's fuel: 20 members over a fuel of 5.
    let small = w.engine_with(|b| b.manifests(ManifestPolicy::MemberSetOnly).store_past_reads(false).fuel(5));
    match small.manifest_asof(&entry, r.asof()) {
        Err(EngineError::BudgetExceeded { .. }) => {}
        other => panic!("over the budget: {other:?}"),
    }
    assert_eq!(
        e.manifest_asof(&entry, r.asof()).unwrap().map(|b| Manifest::from_json(&b).unwrap().members().len()),
        Some(20)
    );
    assert_eq!(w.db().count("manifests"), 0);
}

// ---- The third policy: store up to N members -------------------------------------------------

#[test]
fn store_up_to_n_stores_short_lists_only() {
    let w = recording_world();
    w.clock.set(&t(10, 0));
    w.publish(&level(&w, "a", DAY), "100", &t(10, 0));
    w.publish(&level(&w, "b", DAY), "200", &t(10, 0));
    w.publish(&level(&w, "a", "2016-11-21"), "1", &t(10, 0));
    for k in 0..3 {
        w.publish(&level(&w, &format!("s{k}"), "2016-11-22"), "1", &t(10, 0));
    }
    w.clock.set(&t(12, 0));
    let e = w.engine_with(|b| b.manifests(ManifestPolicy::StoreUpTo(2)));
    let cell = |d: &str| w.cellref(FLOOD, &[("d", date(d))]);
    let two = receipt_of(&e.get(&cell(DAY), None).unwrap());
    let one = receipt_of(&e.get(&cell("2016-11-21"), None).unwrap());
    let three = receipt_of(&e.get(&cell("2016-11-22"), None).unwrap());
    assert!(w.store.manifest(&two.inputs()[0]).is_some(), "two members: stored");
    assert!(w.store.manifest(&one.inputs()[0]).is_some(), "one member: stored");
    assert!(w.store.manifest(&three.inputs()[0]).is_none(), "three members: not stored");
    for r in [&two, &one, &three] {
        assert!(w.store.member_set(&r.inputs()[0]).unwrap().is_some(), "a row for every read");
    }
    assert_eq!(w.db().count("manifests"), 2);
    // The one not stored is rebuilt like any other.
    assert_eq!(Manifest::from_json(&e.manifest(&three.inputs()[0]).unwrap()).unwrap().members().len(), 3);
    assert_ne!(ManifestPolicy::StoreUpTo(2), ManifestPolicy::StoreUpTo(3));
}

// ---- G6, stronger: a scenario's scope found through slice members --------------------------

const BASE: &str = "rates.example.org/base";
const CHAIN: &str = "calc.example.net/chain";
const DEPTH: usize = 40;

/// base() stored; chain(n) over the extent 0..=40: chain(0) = base(), chain(n) = the sum of the
/// one-member slice chain(n: [n-1..n-1]) plus 1. A chain of 41 computed cells linked by slices only.
fn chain_world() -> World {
    let mut w = World::new();
    w.stored_fn(BASE, &[]);
    let extent: Vec<String> = (0..=DEPTH).map(|k| format!("\"{k}\"")).collect();
    let decl = format!(r#"{{"extent":[{}],"type":"number","unit":"1","valid":false}}"#, extent.join(","));
    let decl: &'static str = Box::leak(decl.into_boxed_str());
    w.native_fn(CHAIN, &[BASE, CHAIN], &[("n", decl)], |c, i, _| {
        let n = int(c.args["n"].scalar().unwrap());
        if n == 0 {
            return i.read(BASE, &args(&[]));
        }
        let prev = num(&(n - 1).to_string());
        let ms = i.read_slice(CHAIN, &pattern(&[("n", range(prev.clone(), prev))]))?;
        Ok(add(&sum(ms.iter().map(|m| &m.input.value)), &num("1")))
    });
    w.publish(&w.cell(BASE, &[]), "1", &t(10, 0));
    w
}

#[test]
fn a_scenario_finds_its_scope_through_slice_members_with_no_manifest_held() {
    // The scope walk (Req::lineage) goes from the top cell's stored receipt through each slice entry
    // to its members, 41 levels down, with no manifest held under MemberSetOnly: through the rebuild.
    // Checked by the test writer: the engine evaluates such a chain correctly even with no scope at all
    // (a fresh store, depth limit 12), so this test shows the same answer, not the walk itself.
    let mut answers = Vec::new();
    for policy in [ManifestPolicy::Store, ManifestPolicy::MemberSetOnly] {
        let w = chain_world();
        let e = w.engine_with(move |b| b.manifests(policy));
        for k in 0..=DEPTH {
            let r = receipt_of(&e.get(&w.cellref(CHAIN, &[("n", num(&k.to_string()))]), Some(&t(20, 0))).unwrap());
            assert_eq!(r.value(), &num(&(k + 1).to_string()));
        }
        assert_eq!(w.db().count("manifests") > 0, policy == ManifestPolicy::Store);
        let shallow = w.engine_with(move |b| b.manifests(policy).depth_limit(12));
        let o = Overrides::new().value(w.cellref(BASE, &[]), num("2"));
        let top = w.cellref(CHAIN, &[("n", num(&DEPTH.to_string()))]);
        let a = shallow.whatif(&top, Some(&t(20, 0)), &o, None).unwrap_or_else(|e| panic!("{policy:?}: {e}"));
        assert_eq!(a.value, num(&(DEPTH + 2).to_string()), "{policy:?}");
        answers.push(a.value);
    }
    assert_eq!(answers[0], answers[1]);
}
