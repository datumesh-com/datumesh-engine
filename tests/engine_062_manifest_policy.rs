//! Engine 0.6.2, the guard patch G1, G3 and G6 (CONTRACT-guard-patch.md): a manifest policy, one
//! member-set row per slice read (store 0.6.5's `member_sets`), reuse of a stored computed version
//! without reading the slice's members again, and the readers of a manifest working with none held.
//! Failing tests: the policy, the row and the reuse are not written yet.
//!
//! API assumed:
//!
//! ```text
//! // datumesh_engine (re-exported at the crate root)
//! #[non_exhaustive]
//! #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
//! pub enum ManifestPolicy {
//!     #[default]
//!     Store,          // as today: put_manifest for every slice read, plus a member-set row
//!     MemberSetOnly,  // no put_manifest; the member-set row only
//! }
//! impl EngineBuilder { pub fn manifests(self, policy: ManifestPolicy) -> Self; }
//! // The user's decision of 26 Nov: a read as of a past time (an asof before the clock's now) is
//! // answered and signed, and stores nothing: no receipt, no computed version, no member-set row, no
//! // manifest. Default true (stores, as today: datumesh-local); the mesh sets false whatever its
//! // manifest policy. Not named by the contract: the test writer's choice of shape.
//! impl EngineBuilder { pub fn store_past_reads(self, store: bool) -> Self; }
//!
//! // datumesh_store 0.6.5 (see the store's tests/store_065_member_sets.rs)
//! pub struct MemberSet { pub entry, pub slice_id, pub members_hash, pub fn_hash, pub slice: Vec<u8>,
//!                        pub members: u64, pub tx: Option<String>, pub asof: String,
//!                        pub licences: Vec<String> }  // the members' distinct licences, sorted
//!                                                    // (decided 26 Nov); a scoped member counts
//!                                                    // with the mesh's own licence
//! fn put_member_set(&self, set: &MemberSet) -> Result<bool>;
//! fn member_set(&self, entry: &str) -> Result<Option<MemberSet>>;
//! ```
//!
//! The row's `tx` is the read's `SliceRead.tx` (the latest member or withdrawal tx it saw) and its
//! `asof` the read's as-of (the request's asof for a top-level read).
//!
//! `Resolver::manifest` of the engine (verify at level recomputed, SPEC 10.1 step 7.5) gives the
//! held bytes, else the manifest rebuilt from the row as of its asof (G5, G6): the same canonical
//! bytes a stored manifest would hold. `Req::lineage` (the scenario's scope walk) rebuilds too.
//!
//! "Reads no slice member" is observed with the wire counter (tests/common/wire.rs): no statement of
//! `slice_values` (the value queries end `ORDER BY a.name), c.sort_key`), none of `slice_members`
//! (`m.bytes`), no INSERT, and no runtime call.
//!
//! The timeline tests follow the coordinator's flood example (a calc over a catchment's stations; a
//! station found later is recorded with its historical readings): the calc cell is for a past date,
//! the new member's cell is for that past date too, and only its tx is new.

mod common;

use std::sync::Arc;

use common::*;
use datumesh_core::json::{self, Value};
use datumesh_core::{verify, Cell, Level, Manifest, Receipt, ReceiptBuilder, Resolver, SigningKey};
use datumesh_engine::{Engine, ManifestPolicy, Overrides};

const TEMP: &str = "rates.example.org/temp";
const TOTAL: &str = "calc.example.net/total";
const STR: &str = r#"{"type":"string","valid":false}"#;

/// temp(region, station) stored; total(region) = the sum of temp(region, *).
fn world() -> World {
    let mut w = World::new();
    w.stored_fn(TEMP, &[("region", STR), ("station", STR)]);
    w.native_fn(TOTAL, &[TEMP], &[("region", STR)], |c, i, b| {
        let region = c.args["region"].scalar().unwrap().clone();
        let members = i.read_slice(TEMP, &pattern(&[("region", bound(region)), ("station", open())]))?;
        b.charge(members.len() as u64)?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w
}

fn temp(w: &World, region: &str, station: &str) -> Cell {
    w.cell(TEMP, &[("region", text(region)), ("station", text(station))])
}

fn north(w: &World) -> datumesh_core::CellRef {
    w.cellref(TOTAL, &[("region", text("north"))])
}

fn seed(w: &World) -> Vec<String> {
    vec![
        w.publish(&temp(w, "north", "a"), "10", &t(10, 0)),
        w.publish(&temp(w, "north", "b"), "20", &t(11, 0)),
        w.publish(&temp(w, "south", "a"), "1000", &t(11, 0)),
    ]
}

fn with(policy: ManifestPolicy) -> impl FnOnce(datumesh_engine::EngineBuilder) -> datumesh_engine::EngineBuilder {
    move |b| b.manifests(policy)
}

/// An engine on a wire-counted store of `w`'s database, with `policy`.
fn counted(w: &World, wire: &common::wire::Wire, policy: ManifestPolicy) -> Engine {
    let store: Arc<dyn datumesh_store::Store + Send + Sync> = Arc::new(wire.store());
    Engine::builder(store, MESH)
        .signing_key(key_for(MESH))
        .clock(Arc::new(w.clock.clone()))
        .runtime(Arc::new(SharedNative(w.native.clone())))
        .manifests(policy)
        .build()
        .unwrap()
}

/// How many statements read slice members: `slice_values` (cells and versions) and `slice_members`.
fn member_reads(sql: &std::collections::BTreeMap<String, usize>) -> usize {
    common::wire::matching(sql, "ORDER BY a.name), c.sort_key") + common::wire::matching(sql, "m.bytes")
}

// ---- G1: the policy --------------------------------------------------------------------------

#[test]
fn the_policy_is_store_by_default_and_non_exhaustive() {
    assert_eq!(ManifestPolicy::default(), ManifestPolicy::Store);
    assert_ne!(ManifestPolicy::Store, ManifestPolicy::MemberSetOnly);
    // Non-exhaustive: a match outside the crate needs a wildcard arm (this compiles only with it).
    #[allow(unreachable_patterns)]
    let name = |p: ManifestPolicy| match p {
        ManifestPolicy::Store => "store",
        ManifestPolicy::MemberSetOnly => "member-set-only",
        _ => "other",
    };
    assert_eq!((name(ManifestPolicy::Store), name(ManifestPolicy::MemberSetOnly)), ("store", "member-set-only"));
}

#[test]
fn under_store_a_manifest_and_a_member_set_row_are_written() {
    for explicit in [false, true] {
        let w = world();
        let hs = seed(&w);
        let e = if explicit { w.engine_with(with(ManifestPolicy::Store)) } else { w.engine() };
        let r = receipt_of(&e.get(&north(&w), Some(&t(20, 0))).unwrap());
        assert_eq!(r.value(), &num("30"));
        let entry = r.inputs()[0].clone();
        // Today's behaviour: the manifest and its members.
        assert_eq!(w.db().count("manifests"), 1, "explicit = {explicit}");
        assert_eq!(w.db().count("manifest_members"), 2, "explicit = {explicit}");
        let m = Manifest::from_json(&w.store.manifest(&entry).unwrap()).unwrap();
        let mut want = vec![hs[0].clone(), hs[1].clone()];
        want.sort();
        assert_eq!(m.members(), want.as_slice());
        // Plus the row, so that a later policy change loses nothing (G2).
        assert_eq!(w.db().count("member_sets"), 1, "explicit = {explicit}");
        let row = w.store.member_set(&entry).unwrap().expect("the member-set row");
        assert_eq!(row.entry, entry);
        assert_eq!(row.slice_id, m.slice().id());
        assert_eq!(row.members_hash, m.members_hash());
        assert_eq!(row.fn_hash, m.slice().fn_hash());
        assert_eq!(row.slice, m.slice().canonical());
        assert_eq!(row.members, 2);
        assert_eq!(row.tx.as_deref(), Some(t(11, 0).as_str()), "the latest member tx the read saw");
        assert_eq!(row.asof, t(20, 0), "the read's as-of");
    }
}

#[test]
fn under_member_set_only_no_manifest_is_written_and_the_row_is() {
    let w = world();
    seed(&w);
    let e = w.engine_with(with(ManifestPolicy::MemberSetOnly));
    let r = receipt_of(&e.get(&north(&w), Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("30"));
    let entry = r.inputs()[0].clone();
    assert_eq!((w.db().count("manifests"), w.db().count("manifest_members")), (0, 0), "no put_manifest");
    assert_eq!(w.store.manifest(&entry), None, "the store holds no manifest");
    let row = w.store.member_set(&entry).unwrap().expect("the member-set row");
    assert_eq!((row.members, row.tx.as_deref(), row.asof.as_str()), (2, Some(t(11, 0).as_str()), t(20, 0).as_str()));
    // The receipt and its computed version are written as today.
    assert_eq!(w.db().count("computed_versions"), 1);
    assert!(w.store.receipt(r.hash()).is_some());
    // An empty slice: a row with no tx, no manifest.
    let east = receipt_of(&e.get(&w.cellref(TOTAL, &[("region", text("east"))]), Some(&t(20, 0))).unwrap());
    assert_eq!(east.value(), &num("0"));
    let row = w.store.member_set(&east.inputs()[0]).unwrap().expect("the empty slice's row");
    assert_eq!((row.members, row.tx), (0, None));
    assert_eq!(w.db().count("manifests"), 0);
}

#[test]
fn the_receipt_bytes_are_the_same_under_both_policies() {
    // Two worlds built alike (same keys, clock, functions and stored versions): the computed receipts
    // are byte for byte the same, slice entry included; only what is written beside them differs.
    let mut out = Vec::new();
    for policy in [ManifestPolicy::Store, ManifestPolicy::MemberSetOnly] {
        let w = world();
        seed(&w);
        let e = w.engine_with(with(policy));
        let a = bytes_of(&e.get(&north(&w), Some(&t(20, 0))).unwrap());
        let b = bytes_of(&e.get(&w.cellref(TOTAL, &[("region", text("east"))]), Some(&t(20, 0))).unwrap());
        out.push((a, b, w.db().count("manifests"), w.db().count("member_sets")));
    }
    assert_eq!(out[0].0, out[1].0, "the same receipt over the same slice");
    assert_eq!(out[0].1, out[1].1, "the same receipt over the same empty slice");
    assert_eq!((out[0].2, out[0].3), (2, 2), "Store: manifests and rows");
    assert_eq!((out[1].2, out[1].3), (0, 2), "MemberSetOnly: rows only");
}

// ---- G1 / G6 over the three kinds of slices --------------------------------------------------

const SQ: &str = "calc.example.net/sq";
const SUMSQ: &str = "calc.example.net/sumall";
const FX: &str = "rates.example.org/fx";
const WINDOW: &str = "calc.example.net/window";
const N_EXTENT: &str = r#"{"extent":["1","2","3","4"],"type":"number","unit":"1","valid":false}"#;

/// The stored-function slice of `world`, plus a computed-function slice (sumall = the sum of sq(n: *)
/// over the extent 1..4) and a version slice (window = the sum of versions(fx, 11th, 15th)).
fn three_kinds() -> World {
    let mut w = world();
    w.native_fn(SQ, &[], &[("n", N_EXTENT)], |c, _, _| {
        let n = int(c.args["n"].scalar().unwrap());
        Ok(num(&(n * n).to_string()))
    });
    w.native_fn(SUMSQ, &[SQ], &[], |_, i, _| {
        let members = i.read_slice(SQ, &pattern(&[("n", open())]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w.stored_fn(FX, &[]);
    w.native_fn(WINDOW, &[FX], &[], |_, i, _| {
        let vs = i.read_versions(FX, &args(&[]), &t(11, 0), &t(15, 0))?;
        Ok(sum(vs.iter().map(|v| &v.value)))
    });
    seed(&w);
    let fx = w.cell(FX, &[]);
    for (k, d) in [10, 12, 14, 16].iter().enumerate() {
        w.publish(&fx, &(k + 1).to_string(), &t(*d, 0));
    }
    w
}

fn three_cells(w: &World) -> [datumesh_core::CellRef; 3] {
    [north(w), w.cellref(SUMSQ, &[]), w.cellref(WINDOW, &[])]
}

#[test]
fn with_no_manifest_held_the_engine_serves_the_bytes_a_stored_one_would_hold() {
    // G6: Resolver::manifest rebuilds from the row; the bytes are those of the Store-policy run.
    let held: Vec<(String, Vec<u8>)> = {
        let w = three_kinds();
        let e = w.engine_with(with(ManifestPolicy::Store));
        three_cells(&w)
            .iter()
            .map(|c| {
                let r = receipt_of(&e.get(c, Some(&t(20, 0))).unwrap());
                let entry = r.inputs()[0].clone();
                let bytes = w.store.manifest(&entry).expect("held under Store").into_owned();
                (entry, bytes)
            })
            .collect()
    };
    let w = three_kinds();
    let e = w.engine_with(with(ManifestPolicy::MemberSetOnly));
    for (c, (entry, bytes)) in three_cells(&w).iter().zip(&held) {
        let r = receipt_of(&e.get(c, Some(&t(20, 0))).unwrap());
        assert_eq!(&r.inputs()[0], entry, "{}: the same entry", c.fn_name);
        assert_eq!(w.store.manifest(entry), None, "{}: nothing held", c.fn_name);
        let rebuilt = e.manifest(entry).unwrap_or_else(|| panic!("{}: rebuilt from the row", c.fn_name));
        assert_eq!(rebuilt.as_ref(), bytes.as_slice(), "{}: the canonical bytes a stored manifest holds", c.fn_name);
        assert_eq!(Manifest::from_json(&rebuilt).unwrap().entry(), *entry);
        // A fresh engine (no memo, nothing of the request kept) rebuilds the same.
        assert_eq!(
            w.engine_with(with(ManifestPolicy::MemberSetOnly)).manifest(entry).unwrap().as_ref(),
            bytes.as_slice()
        );
    }
    assert_eq!(w.db().count("manifests"), 0, "the rebuild is never stored");
    assert_eq!(w.db().count("manifest_members"), 0);
    // Every receipt verifies at level recomputed through the engine, manifests rebuilt.
    assert!(w.verify_all_issued(&e, "engine_062_three_kinds") >= 3);
    assert_eq!(w.db().count("manifests"), 0, "verification stores nothing either");
}

#[test]
fn an_entry_with_no_row_and_no_manifest_is_none() {
    let w = three_kinds();
    let e = w.engine_with(with(ManifestPolicy::MemberSetOnly));
    let r = receipt_of(&e.get(&north(&w), Some(&t(20, 0))).unwrap());
    let entry = &r.inputs()[0];
    let other = format!("{}:{}", &entry[..entry.len() - 64], "0".repeat(64));
    assert_eq!(e.manifest(&other), None);
    assert_eq!(e.manifest("slice:not-an-entry"), None);
}

#[test]
fn a_scenario_walks_slice_lineage_with_no_manifest_held() {
    // G6, Req::lineage: the scope of a scenario (the dependent computed cells in the lineage of the
    // top cell's stored version) is found through the members, rebuilt; the scenario answers the same
    // under both policies.
    let mut answers = Vec::new();
    for policy in [ManifestPolicy::Store, ManifestPolicy::MemberSetOnly] {
        let w = three_kinds();
        let e = w.engine_with(with(policy));
        let top = w.cellref(SUMSQ, &[]);
        assert_eq!(receipt_of(&e.get(&top, Some(&t(20, 0))).unwrap()).value(), &num("30"));
        let fresh = w.engine_with(with(policy));
        let o = Overrides::new().value(w.cellref(SQ, &[("n", num("2"))]), num("100"));
        let a = fresh.whatif(&top, Some(&t(20, 0)), &o, None).unwrap();
        answers.push(format!("{a:?}"));
        let o =
            Overrides::new().value(w.cellref(TEMP, &[("region", text("north")), ("station", text("a"))]), num("11"));
        let b = fresh.whatif(&north(&w), Some(&t(20, 0)), &o, None).unwrap();
        answers.push(format!("{b:?}"));
        assert_eq!(w.db().count("manifests"), if policy == ManifestPolicy::Store { 1 } else { 0 });
    }
    assert_eq!(answers[0], answers[2], "the computed-slice scenario");
    assert_eq!(answers[1], answers[3], "the stored-slice scenario");
    assert!(answers[0].contains("126"), "1 + 100 + 9 + 16: {}", answers[0]);
}

// ---- G3: reuse without reading the members again ---------------------------------------------

#[test]
fn a_second_get_reads_no_slice_member_and_writes_no_row() {
    for policy in [ManifestPolicy::MemberSetOnly, ManifestPolicy::Store] {
        let w = world();
        for k in 0..20 {
            w.publish(&temp(&w, "north", &format!("s{k:02}")), &k.to_string(), &t(10, 0));
        }
        let first = w.engine_with(with(policy)).get(&north(&w), Some(&t(20, 0))).unwrap();
        assert_eq!(w.calls(TOTAL), 1);
        let before = (w.db().count("member_sets"), w.db().count("receipts"), w.db().count("computed_versions"));
        let wire = common::wire::Wire::start(&w.db().config);
        // A fresh engine: no memo, so the stored computed version is revalidated through the store.
        let e = counted(&w, &wire, policy);
        wire.take();
        assert_eq!(e.get(&north(&w), Some(&t(20, 0))).unwrap(), first, "{policy:?}: the stored version stands");
        let sql = wire.take();
        assert_eq!(member_reads(&sql), 0, "{policy:?}: no member read: {sql:#?}");
        assert_eq!(common::wire::matching(&sql, "INSERT"), 0, "{policy:?}: nothing written: {sql:#?}");
        assert_eq!(w.calls(TOTAL), 1, "{policy:?}: not evaluated again");
        // A later asof with nothing changed in the slice: the same version, still no member read.
        let e = counted(&w, &wire, policy);
        wire.take();
        assert_eq!(e.get(&north(&w), Some(&t(25, 0))).unwrap(), first, "{policy:?}: the version holds later");
        let sql = wire.take();
        assert_eq!(member_reads(&sql), 0, "{policy:?}: {sql:#?}");
        assert_eq!(common::wire::matching(&sql, "INSERT"), 0, "{policy:?}: {sql:#?}");
        assert_eq!(w.calls(TOTAL), 1);
        assert_eq!(
            (w.db().count("member_sets"), w.db().count("receipts"), w.db().count("computed_versions")),
            before,
            "{policy:?}: no row of any kind"
        );
    }
}

#[test]
fn a_new_member_after_the_rows_tx_makes_it_re_evaluate() {
    let w = world();
    seed(&w);
    let first =
        receipt_of(&w.engine_with(with(ManifestPolicy::MemberSetOnly)).get(&north(&w), Some(&t(20, 0))).unwrap());
    assert_eq!(first.value(), &num("30"));
    w.publish(&temp(&w, "north", "c"), "5", &t(21, 0));
    let e = w.engine_with(with(ManifestPolicy::MemberSetOnly));
    let r = receipt_of(&e.get(&north(&w), Some(&t(22, 0))).unwrap());
    assert_eq!(r.value(), &num("35"), "re-evaluated with the new member");
    assert_eq!(w.calls(TOTAL), 2);
    assert_ne!(r.inputs(), first.inputs());
    assert_eq!(w.db().count("member_sets"), 2, "a row for the new member set");
    // A correction of a member (a new version of an existing cell) after the row's tx: the same.
    w.publish(&temp(&w, "north", "a"), "11", &t(23, 0));
    let r = receipt_of(&w.engine_with(with(ManifestPolicy::MemberSetOnly)).get(&north(&w), Some(&t(24, 0))).unwrap());
    assert_eq!(r.value(), &num("36"));
    assert_eq!(w.calls(TOTAL), 3);
}

// A new member with an older signed time: engine_062_recorded.rs (G9), on a store that records at
// its clock; without one the store records the signed time, as before migration 17.

// ---- G3 with licences (decided 26 Nov: the row carries its members' licences) ----------------

const SRC: &str = "rates.example.org/src";
const SRC_TOTAL: &str = "calc.example.net/src_total";
const RESTRICTED: &str = "https://rates.example.org/licences/restricted";
const LT0: &str = "2024-03-15T16:00:00.000000Z";

/// src(i) stored, src_total() = the sum of src(*); src(0) under MIT, src(1) and src(2) under a
/// restricted licence that calc.example.net declares for src_total from 2020 on; the clock at
/// 2026-01-01.
fn licensed_world() -> World {
    let mut w = World::new();
    w.stored_fn(SRC, &[("i", STR)]);
    w.native_fn(SRC_TOTAL, &[SRC], &[], |_, i, _| {
        let members = i.read_slice(SRC, &pattern(&[("i", open())]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w.declare(CALC, &declaration(SRC_TOTAL, RESTRICTED, "2020-01-01T00:00:00.000000Z", None));
    let src = |i: &str| w.cell(SRC, &[("i", text(i))]);
    w.publish_licensed(&src("0"), "1", LT0, "MIT");
    w.publish_licensed(&src("1"), "2", LT0, RESTRICTED);
    w.publish_licensed(&src("2"), "3", LT0, RESTRICTED);
    w.clock.set("2026-01-01T00:00:00.000000Z");
    w
}

#[test]
fn the_row_carries_the_members_licences_and_reuse_reads_no_member() {
    for policy in [ManifestPolicy::MemberSetOnly, ManifestPolicy::Store] {
        let w = licensed_world();
        let total = w.cellref(SRC_TOTAL, &[]);
        let first = receipt_of(&w.engine_with(with(policy)).get(&total, None).unwrap());
        assert_eq!((first.value(), first.licence()), (&num("6"), RESTRICTED));
        let row = w.store.member_set(&first.inputs()[0]).unwrap().expect("the row");
        assert_eq!(row.licences, ["MIT", RESTRICTED], "{policy:?}: the members' licences, distinct and sorted");
        // Entitled (declared): a second GET from a fresh engine is served with no member read.
        let wire = common::wire::Wire::start(&w.db().config);
        let e = counted(&w, &wire, policy);
        wire.take();
        let again = receipt_of(&e.get(&total, None).unwrap());
        assert_eq!(again.hash(), first.hash(), "{policy:?}: the stored version stands");
        let sql = wire.take();
        assert_eq!(member_reads(&sql), 0, "{policy:?}: the licences come from the row: {sql:#?}");
        assert_eq!(common::wire::matching(&sql, "INSERT"), 0, "{policy:?}: {sql:#?}");
        assert_eq!(w.calls(SRC_TOTAL), 1, "{policy:?}");
        // Not entitled: the publisher's key document no longer declares the licence (so the stored
        // version is not entitled at its issued, and the rule refuses now): refused, still with no
        // member read and nothing written.
        // (A declarations document holds 1 to 64 names: it now declares another function only.)
        w.declare(CALC, &declaration("calc.example.net/other", RESTRICTED, "2020-01-01T00:00:00.000000Z", None));
        let e = counted(&w, &wire, policy);
        wire.take();
        match e.get(&total, None) {
            Err(datumesh_engine::EngineError::Unlicensed { reason, .. }) => {
                assert_eq!(reason, datumesh_core::LicenceReason::NotDeclared, "{policy:?}")
            }
            other => panic!("{policy:?}: not entitled must be refused: {other:?}"),
        }
        let sql = wire.take();
        assert_eq!(member_reads(&sql), 0, "{policy:?}: refused from the row's licences: {sql:#?}");
        assert_eq!(common::wire::matching(&sql, "INSERT"), 0, "{policy:?}: {sql:#?}");
    }
}

#[test]
fn a_scenario_writes_no_row_and_leaves_the_rows_licences() {
    // "A scoped member counts with the mesh's own licence": a scenario's members are the scoped
    // ones, and a scenario stores nothing, so no row ever carries that licence; what is checked is
    // that the scenario answers, writes no row, and leaves the stored row's licences as they were.
    let w = licensed_world();
    let total = w.cellref(SRC_TOTAL, &[]);
    let first = receipt_of(&w.engine_with(with(ManifestPolicy::MemberSetOnly)).get(&total, None).unwrap());
    let rows = w.db().count("member_sets");
    let e = w.engine_with(with(ManifestPolicy::MemberSetOnly));
    let o = Overrides::new().value(w.cellref(SRC, &[("i", text("1"))]), num("20"));
    let a = e.whatif(&total, None, &o, None).unwrap();
    assert_eq!(a.value, num("24"));
    assert_eq!(w.db().count("member_sets"), rows, "a scenario writes no row");
    assert_eq!(w.store.member_set(&first.inputs()[0]).unwrap().unwrap().licences, ["MIT", RESTRICTED]);
}

#[test]
fn a_withdrawal_after_the_rows_tx_makes_it_re_evaluate() {
    let x = fx_world();
    let e = x.w.engine_with(with(ManifestPolicy::MemberSetOnly));
    let before = receipt_of(&e.get(&total(), Some(AFTER0)).unwrap());
    assert_eq!(before.value(), &num("3"), "USD 1 + GBP 1 + JPY 1");
    x.withdraw();
    let e = x.w.engine_with(with(ManifestPolicy::MemberSetOnly));
    let after = receipt_of(&e.get(&total(), Some(AFTER)).unwrap());
    assert_eq!(after.value(), &num("2"), "USD withdrawn at TW (after the row's tx T0): left out");
    assert_eq!(x.w.calls(TOTAL_FX), 2);
}

// ---- The flood timeline (coordinator, 26 Nov) ------------------------------------------------

const LEVEL: &str = "rates.example.org/station_level";
const FLOOD: &str = "calc.example.net/flood";
const DAY: &str = "2016-11-20";

/// station_level(station, d) stored; flood(d) = the sum of station_level(*, d). Stations a and b have
/// readings for DAY recorded at the 10th.
fn flood_world() -> World {
    let mut w = World::new();
    w.stored_fn(LEVEL, &[("d", r#"{"type":"date","valid":false}"#), ("station", STR)]);
    w.native_fn(FLOOD, &[LEVEL], &[("d", r#"{"type":"date","valid":false}"#)], |c, i, _| {
        let d = c.args["d"].scalar().unwrap().clone();
        let members = i.read_slice(LEVEL, &pattern(&[("d", bound(d)), ("station", open())]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w.publish(&level(&w, "a", DAY), "100", &t(10, 0));
    w.publish(&level(&w, "b", DAY), "200", &t(10, 0));
    w.publish(&level(&w, "a", "2016-11-21"), "1", &t(10, 0));
    w
}

fn level(w: &World, station: &str, d: &str) -> Cell {
    w.cell(LEVEL, &[("d", date(d)), ("station", text(station))])
}

fn flood(w: &World) -> datumesh_core::CellRef {
    w.cellref(FLOOD, &[("d", date(DAY))])
}

/// T0 (the 12th): flood(DAY) computed and stored with stations a and b. T1 (the 14th): station c's
/// readings for November 2016 recorded (a new series under the slice's pattern, its cell dated in
/// the past, its tx new). Now (the 16th): nobody has read flood since T1.
fn flood_timeline() -> (World, Receipt, Vec<u8>) {
    let w = flood_world();
    w.clock.set(&t(12, 0));
    let e = w.engine_with(mesh());
    let a0 = e.get(&flood(&w), None).unwrap();
    let r0 = receipt_of(&a0);
    assert_eq!(r0.value(), &num("300"));
    drop(e);
    w.publish(&level(&w, "c", DAY), "50", &t(14, 0));
    w.publish(&level(&w, "c", "2016-11-19"), "40", &t(14, 0));
    w.clock.set(&t(16, 0));
    (w, r0, bytes_of(&a0))
}

fn verifies_recomputed(e: &Engine, bytes: &[u8], what: &str) {
    verifies_with(e, bytes, &all_keys(), what)
}

fn verifies_with(e: &Engine, bytes: &[u8], keys: &datumesh_core::Keys, what: &str) {
    let out = verify(bytes, keys, Some(e as &dyn Resolver), None);
    assert!(out.ok, "{what}: {out:?}");
    assert_eq!(out.level, Some(Level::Recomputed), "{what}: {out:?}");
}

#[test]
fn flood_a_read_as_of_between_t0_and_t1_is_the_t0_version() {
    let (w, r0, b0) = flood_timeline();
    let e = w.engine_with(mesh());
    let rows = stored_rows(&w);
    let a = e.get(&flood(&w), Some(&t(13, 0))).unwrap();
    assert_eq!(stored_rows(&w), rows, "a past read stores nothing");
    assert_eq!(bytes_of(&a), b0, "case 1: the T0 version, old members, old value");
    verifies_recomputed(&e, &b0, "the T0 receipt");
    let m = Manifest::from_json(&e.manifest(&r0.inputs()[0]).expect("rebuilt as of the row's asof")).unwrap();
    assert_eq!(m.members().len(), 2, "station c is not a member as of T0");
}

#[test]
fn flood_a_read_as_of_between_t1_and_now_has_the_new_station_and_stores_nothing() {
    let (w, r0, b0) = flood_timeline();
    let before = stored_rows(&w);
    let e = w.engine_with(mesh());
    let a2 = e.get(&flood(&w), Some(&t(15, 0))).unwrap();
    let r = receipt_of(&a2);
    assert_eq!(r.value(), &num("350"), "case 2: the new station counts, not the T0 version");
    assert_ne!(r.inputs()[0], r0.inputs()[0], "another member set");
    assert_eq!((r.tx(), r.asof()), (t(14, 0).as_str(), t(14, 0).as_str()));
    // The user's decision (26 Nov): a read as of a past time is answered and signed, and stores
    // nothing: no receipt, no computed version, no member-set row, no manifest.
    assert_eq!(stored_rows(&w), before, "case 2 writes nothing");
    assert_eq!(w.store.member_set(&r.inputs()[0]).unwrap(), None);
    assert!(w.store.receipt(r.hash()).is_none(), "the receipt is answered, not stored");
    // A plain read now after it: the new answer, and it stores (a version and a row).
    let now = receipt_of(&w.engine_with(mesh()).get(&flood(&w), None).unwrap());
    assert_eq!(now.value(), &num("350"), "case 3 after case 2");
    assert_eq!(now.inputs(), r.inputs(), "the same member set");
    let after = stored_rows(&w);
    assert_eq!(after[1], before[1] + 1, "case 3 writes a computed version");
    assert_eq!(after[2], before[2] + 1, "case 3 writes a member-set row");
    let row = w.store.member_set(&now.inputs()[0]).unwrap().expect("case 3's row");
    assert_eq!((row.tx.as_deref(), row.asof.as_str()), (Some(t(14, 0).as_str()), t(16, 0).as_str()));
    let v = w.store.computed_asof(&now.cell().id(), &t(16, 0)).unwrap().expect("case 3's computed version");
    assert_eq!(
        (v.receipt_hash.as_str(), v.tx.as_str(), v.asof.as_str()),
        (now.hash(), t(14, 0).as_str(), t(14, 0).as_str())
    );
    // After it case 1 is still the T0 version, and stores nothing either.
    let rows = stored_rows(&w);
    assert_eq!(bytes_of(&w.engine_with(mesh()).get(&flood(&w), Some(&t(13, 0))).unwrap()), b0);
    assert_eq!(stored_rows(&w), rows);
    // Every receipt verifies, each manifest rebuilt from a row (case 2's member set is case 3's:
    // its row is written by the current read).
    let fresh = w.engine_with(mesh());
    verifies_recomputed(&fresh, &b0, "the T0 receipt");
    verifies_recomputed(&fresh, &bytes_of(&a2), "the case 2 receipt");
    verifies_recomputed(&fresh, &w.stored_bytes(now.hash()), "the case 3 receipt");
    assert_eq!(stored_rows(&w), rows, "verification stores nothing");
}

#[test]
fn a_past_read_stores_nothing_under_either_policy_and_a_current_read_stores() {
    for policy in [ManifestPolicy::MemberSetOnly, ManifestPolicy::Store] {
        let w = world();
        seed(&w);
        w.clock.set(&t(20, 0));
        let before = stored_rows(&w);
        let e = w.engine_with(move |b| b.manifests(policy).store_past_reads(false));
        let past = receipt_of(&e.get(&north(&w), Some(&t(19, 0))).unwrap());
        assert_eq!(past.value(), &num("30"));
        assert_eq!(stored_rows(&w), before, "{policy:?}: a past read stores nothing");
        assert_eq!(w.calls(TOTAL), 1);
        // Asked again (memo or not), still nothing.
        let fresh = w.engine_with(move |b| b.manifests(policy).store_past_reads(false));
        assert_eq!(receipt_of(&fresh.get(&north(&w), Some(&t(19, 0))).unwrap()).value(), &num("30"));
        assert_eq!(stored_rows(&w), before, "{policy:?}");
        // A current read (no asof, or the clock's now) stores, as today.
        let cur = receipt_of(&fresh.get(&north(&w), None).unwrap());
        assert_eq!(cur.value(), &num("30"));
        let after = stored_rows(&w);
        assert!(
            after[0] > before[0] && after[1] > before[1] && after[2] > before[2],
            "{policy:?}: {before:?} -> {after:?}"
        );
        assert_eq!(after[3] > before[3], policy == ManifestPolicy::Store, "{policy:?}: a manifest only under Store");
    }
    // The default (local's): past reads store as today.
    let w = world();
    seed(&w);
    w.clock.set(&t(20, 0));
    let before = stored_rows(&w);
    receipt_of(&w.engine().get(&north(&w), Some(&t(19, 0))).unwrap());
    assert!(stored_rows(&w)[1] > before[1], "by default an as-of read stores its version, as today");
}

/// [receipts, computed versions, member-set rows, manifests].
fn stored_rows(w: &World) -> [i64; 4] {
    [
        w.db().count("receipts"),
        w.db().count("computed_versions"),
        w.db().count("member_sets"),
        w.db().count("manifests"),
    ]
}

/// The engine the mesh runs at DATUMESH_MANIFEST_STORE_MAX=0: no manifest, and nothing stored by a
/// read as of a past time.
fn mesh() -> impl FnOnce(datumesh_engine::EngineBuilder) -> datumesh_engine::EngineBuilder {
    |b| b.manifests(ManifestPolicy::MemberSetOnly).store_past_reads(false)
}

#[test]
fn flood_a_plain_read_now_has_the_new_station_and_writes_a_new_version() {
    let (w, r0, b0) = flood_timeline();
    let versions = w.db().count("computed_versions");
    let rows = w.db().count("member_sets");
    let e = w.engine_with(mesh());
    let r = receipt_of(&e.get(&flood(&w), None).unwrap());
    assert_eq!(r.value(), &num("350"), "case 3: the new answer");
    assert_eq!(w.db().count("computed_versions"), versions + 1, "a new computed version");
    assert_eq!(w.db().count("member_sets"), rows + 1, "a new member-set row");
    let row = w.store.member_set(&r.inputs()[0]).unwrap().unwrap();
    assert_ne!(row.members_hash, w.store.member_set(&r0.inputs()[0]).unwrap().unwrap().members_hash);
    assert_eq!((row.members, row.tx.as_deref(), row.asof.as_str()), (3, Some(t(14, 0).as_str()), t(16, 0).as_str()));
    let v = w.store.computed_asof(&r.cell().id(), &t(16, 0)).unwrap().unwrap();
    assert_eq!((v.tx.as_str(), v.asof.as_str()), (t(14, 0).as_str(), t(14, 0).as_str()));
    // After it, case 1 still answers the T0 version.
    let fresh = w.engine_with(mesh());
    assert_eq!(bytes_of(&fresh.get(&flood(&w), Some(&t(13, 0))).unwrap()), b0);
    // Case 4: both verify at level recomputed, each manifest rebuilt as of its own row's asof.
    verifies_recomputed(&fresh, &b0, "the T0 receipt");
    verifies_recomputed(&fresh, &w.stored_bytes(r.hash()), "the new receipt");
    assert_eq!(w.db().count("manifests"), 0);
}

#[test]
fn flood_a_backfilled_reading_of_another_date_does_not_change_the_cell() {
    // Station c's reading of the 19th is not under flood(DAY)'s pattern: had only it been recorded,
    // the T0 version would stand, with no member read.
    let w = flood_world();
    w.clock.set(&t(12, 0));
    let r0 = receipt_of(&w.engine_with(with(ManifestPolicy::MemberSetOnly)).get(&flood(&w), None).unwrap());
    w.publish(&level(&w, "c", "2016-11-19"), "40", &t(14, 0));
    w.clock.set(&t(16, 0));
    let r = receipt_of(&w.engine_with(with(ManifestPolicy::MemberSetOnly)).get(&flood(&w), None).unwrap());
    assert_eq!(r.hash(), r0.hash());
    assert_eq!(w.calls(FLOOD), 1);
}

// ---- Withdrawals (step 7.5, D125): the fixture of engine_060_withdrawal ------------------------

const FXR: &str = "rates.example.org/fx_rate";
const TOTAL_FX: &str = "calc.example.net/total_fx";
const T0: &str = "2024-03-10T12:00:00.000000Z";
const AFTER0: &str = "2024-03-12T00:00:00.000000Z";
const TW: &str = "2024-03-15T16:00:00.000000Z";
const AFTER: &str = "2024-03-18T00:00:00.000000Z";

fn vector(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/vectors/receipt/{name}.json"));
    json::parse(&std::fs::read(path).unwrap()).unwrap()
}

fn input_of(v: &Value) -> Vec<u8> {
    v.get("input").and_then(Value::as_str).unwrap().as_bytes().to_vec()
}

struct X {
    w: World,
    base: Receipt,
    wbytes: Vec<u8>,
    /// The mesh's keys and the vectors' rates.example.org key document.
    keys: datumesh_core::Keys,
}

impl X {
    fn cell(&self, quote: &str) -> Cell {
        let mut c = self.base.cell().clone();
        c.args = args(&[("base", text("EUR")), ("date", date("2024-03-15")), ("quote", text(quote))]);
        c
    }

    fn withdraw(&self) {
        let h = self.w.store.put_receipt(&self.wbytes).unwrap();
        self.w.store.put_version(&self.cell("USD").id(), TW, &h).unwrap();
        self.w.notify(&self.cell("USD").id());
    }
}

fn total() -> datumesh_core::CellRef {
    let (ns, f) = TOTAL_FX.rsplit_once('/').unwrap();
    datumesh_core::CellRef { ns: ns.into(), fn_name: f.into(), args: args(&[]), asof: None }
}

/// fx_rate (the vectors' stored function, key document and keypair) with USD, GBP and JPY valued 1
/// at T0; total_fx = the sum of fx_rate(EUR, 2024-03-15, *). The withdrawal of USD is the vector
/// `withdrawn-primary` (tx TW, asof 2024-03-16T09).
fn fx_world() -> X {
    let mut w = World::new();
    let stored = vector("stored-number");
    let base = Receipt::from_json(&input_of(&stored)).unwrap();
    let seed: [u8; 32] = {
        let s = stored.get("keypair").and_then(|k| k.get("seed")).and_then(Value::as_str).unwrap();
        (0..32).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect::<Vec<_>>().try_into().unwrap()
    };
    let key = SigningKey::from_seed(&seed);
    let body = stored.get("keys").and_then(|k| k.get(RATES)).and_then(|k| k.get("https")).unwrap();
    w.store
        .put_namespace(
            RATES,
            body.get("body").and_then(Value::as_str).unwrap().as_bytes(),
            "2024-01-01T00:00:00.000000Z",
        )
        .unwrap();
    let descriptor = stored.get("recompute").and_then(|r| r.get("descriptor")).unwrap().to_jcs();
    let h = w.store.put_descriptor(&descriptor).unwrap();
    w.store.bind_name(FXR, &h, BIND_TX).unwrap();
    w.native_fn(TOTAL_FX, &[FXR], &[], |_, i, _| {
        let p = pattern(&[("base", bound(text("EUR"))), ("date", bound(date("2024-03-15"))), ("quote", open())]);
        let members = i.read_slice(FXR, &p)?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    let mut keys = all_keys();
    keys.insert_response(RATES, 200, Some(body.get("body").and_then(Value::as_str).unwrap().as_bytes()));
    let x = X { w, base, wbytes: input_of(&vector("withdrawn-primary")), keys };
    for quote in ["USD", "GBP", "JPY"] {
        let cell = x.cell(quote);
        let r = ReceiptBuilder::new()
            .cell(cell.clone())
            .value(num("1"))
            .valid(x.base.valid().clone())
            .asof(T0)
            .tx(T0)
            .issued(T0)
            .publisher(x.base.publisher())
            .signer_host(x.base.signer_host())
            .licence(x.base.licence())
            .summary(format!("1 EUR was worth 1 {quote}."))
            .sign(&key)
            .unwrap();
        let h = x.w.store.put_receipt(&receipt_json(&r)).unwrap();
        x.w.store.put_version(&cell.id(), T0, &h).unwrap();
    }
    x
}

/// The manifest bytes of `entry` a Store-policy run of the same fixture holds, at the same asofs.
fn held_fx_manifests(withdraw_first: bool) -> Vec<(String, Vec<u8>)> {
    let x = fx_world();
    let e = x.w.engine_with(with(ManifestPolicy::Store));
    let mut out = Vec::new();
    if withdraw_first {
        x.withdraw();
    }
    for asof in [AFTER0, AFTER] {
        if !withdraw_first && asof == AFTER {
            x.withdraw();
        }
        let r = receipt_of(&e.get(&total(), Some(asof)).unwrap());
        let entry = r.inputs()[0].clone();
        out.push((entry.clone(), x.w.store.manifest(&entry).unwrap().into_owned()));
    }
    out
}

#[test]
fn a_withdrawal_after_the_receipt_keeps_the_member_in_the_rebuild() {
    let held = held_fx_manifests(false);
    let x = fx_world();
    let e = x.w.engine_with(with(ManifestPolicy::MemberSetOnly));
    let a = e.get(&total(), Some(AFTER0)).unwrap();
    let r = receipt_of(&a);
    assert_eq!(r.value(), &num("3"));
    x.withdraw();
    let fresh = x.w.engine_with(with(ManifestPolicy::MemberSetOnly));
    // The receipt was signed before the withdrawal: the rebuild as of its row's asof keeps USD.
    let rebuilt = fresh.manifest(&r.inputs()[0]).expect("rebuilt");
    assert_eq!(rebuilt.as_ref(), held[0].1.as_slice(), "the bytes a stored manifest holds");
    assert_eq!(Manifest::from_json(&rebuilt).unwrap().members().len(), 3, "USD kept");
    verifies_with(&fresh, &bytes_of(&a), &x.keys, "the receipt from before the withdrawal");
    // After the withdrawal, a read leaves USD out, and its own rebuild matches too.
    let r2 = receipt_of(&fresh.get(&total(), Some(AFTER)).unwrap());
    assert_eq!(r2.value(), &num("2"));
    assert_eq!(fresh.manifest(&r2.inputs()[0]).unwrap().as_ref(), held[1].1.as_slice());
    verifies_with(&fresh, &x.w.stored_bytes(r2.hash()), &x.keys, "the receipt after the withdrawal");
    assert_eq!(x.w.db().count("manifests"), 0);
}

#[test]
fn a_withdrawal_before_the_receipt_leaves_the_member_out_of_the_rebuild() {
    let held = held_fx_manifests(true);
    let x = fx_world();
    x.withdraw();
    let e = x.w.engine_with(with(ManifestPolicy::MemberSetOnly));
    let a = e.get(&total(), Some(AFTER)).unwrap();
    let r = receipt_of(&a);
    assert_eq!(r.value(), &num("2"));
    let row = x.w.store.member_set(&r.inputs()[0]).unwrap().unwrap();
    assert_eq!(row.tx.as_deref(), Some(TW), "the withdrawal's tx is the latest change the read saw");
    let fresh = x.w.engine_with(with(ManifestPolicy::MemberSetOnly));
    let rebuilt = fresh.manifest(&r.inputs()[0]).expect("rebuilt");
    assert_eq!(rebuilt.as_ref(), held[1].1.as_slice());
    assert_eq!(Manifest::from_json(&rebuilt).unwrap().members().len(), 2, "USD left out");
    verifies_with(&fresh, &bytes_of(&a), &x.keys, "the receipt over the withdrawn slice");
    // As of before the withdrawal the slice still has USD.
    let early = receipt_of(&fresh.get(&total(), Some(AFTER0)).unwrap());
    assert_eq!(early.value(), &num("3"));
    assert_eq!(fresh.manifest(&early.inputs()[0]).unwrap().as_ref(), held[0].1.as_slice());
}
