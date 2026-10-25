//! Slice reads (SPEC 15.13, slice-as-lineage): over stored functions, one `inputs` entry
//! `slice:<slice id>:<members hash>` whose manifest the engine stores and serves; members in
//! key order; the receipt verifies at recomputed through the engine; a correction to a member
//! or a new member dirties the readers whose pattern it falls into, and nothing else; the old
//! receipt still verifies, and after a backdated correction recomputation reports
//! E_INPUT_SUPERSEDED with the evaluation's lineage. A running total is one cell per day with
//! slice lineage. Slices over computed functions enumerate the declared extent, evaluate and
//! memoise each member, and detect cycles; a member without a value makes the read
//! E_FORMULA_ABSENT.

mod common;

use common::*;
use datumesh_core::{verify, ErrorCode, Fault, Level, Manifest, Receipt, Resolver, Slice};
use datumesh_engine::{EngineError, EvalError, Pattern};

const TEMP: &str = "rates.example.org/temp";
const TOTAL: &str = "calc.example.net/total";
const FIRST: &str = "calc.example.net/first";
const STATION: &str = r#"{"type":"string","valid":false}"#;
const REGION: &str = r#"{"type":"string","valid":false}"#;

/// temp(region, station), stored; total(region) = the sum of temp(region, *); first(region)
/// = the value of the first member in key order.
fn world() -> World {
    let mut w = World::new();
    w.stored_fn(TEMP, &[("region", REGION), ("station", STATION)]);
    w.native_fn(TOTAL, &[TEMP], &[("region", REGION)], |c, i, b| {
        let region = c.args["region"].scalar().unwrap().clone();
        let members = i.read_slice(TEMP, &pattern(&[("region", bound(region)), ("station", open())]))?;
        b.charge(members.len() as u64)?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w.native_fn(FIRST, &[TEMP], &[("region", REGION)], |c, i, _| {
        let region = c.args["region"].scalar().unwrap().clone();
        let members = i.read_slice(TEMP, &pattern(&[("region", bound(region)), ("station", open())]))?;
        match members.first() {
            Some(m) => Ok(m.input.value.clone()),
            None => Err(EvalError::Fault(Fault::Absent)),
        }
    });
    w
}

fn temp(w: &World, region: &str, station: &str) -> datumesh_core::Cell {
    w.cell(TEMP, &[("region", text(region)), ("station", text(station))])
}

fn north(w: &World, name: &str) -> datumesh_core::CellRef {
    w.cellref(name, &[("region", text("north"))])
}

#[test]
fn a_slice_read_is_one_input_entry_with_its_manifest_and_recomputes() {
    let w = world();
    let e = w.engine();
    // Published out of key order; one late-signed.
    let hc = w.publish(&temp(&w, "north", "c"), "30", &t(12, 0));
    let ha = w.publish(&temp(&w, "north", "a"), "10", &t(10, 0));
    let hb = w.publish_late(&temp(&w, "north", "b"), "20", &t(11, 0), &t(13, 0));
    w.publish(&temp(&w, "south", "a"), "1000", &t(10, 0));
    let r = receipt_of(&e.get(&north(&w, TOTAL), Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("60"));
    assert_eq!(r.inputs().len(), 1, "one entry for the slice");
    let entry = &r.inputs()[0];
    assert!(entry.starts_with("slice:"), "{entry}");
    // The members' latest tx; the latest of their asofs (b is late-signed).
    assert_eq!((r.tx(), r.asof()), (t(12, 0).as_str(), t(13, 0).as_str()));
    // The manifest: stored, served by the engine as resolver, the members ascending.
    let bytes = e.manifest(entry).expect("the engine serves the manifest");
    assert_eq!(bytes.as_ref(), w.store.manifest(entry).unwrap().as_ref());
    let m = Manifest::from_json(&bytes).unwrap();
    assert_eq!(&m.entry(), entry);
    let mut want = vec![ha.clone(), hb.clone(), hc.clone()];
    want.sort();
    assert_eq!(m.members(), want.as_slice());
    let fn_hash = w.cell(TEMP, &[]).fn_hash;
    let slice = Slice::from_json(
        format!(
            r#"{{"args":{{"region":{{"type":"string","value":"north"}},"station":"*"}},"fn":"temp","fn_hash":"{fn_hash}","ns":"rates.example.org"}}"#
        )
        .as_bytes(),
    )
    .unwrap();
    assert_eq!(m.slice(), &slice);
    // Members in key order: station a first, whatever the publishing order.
    assert_eq!(receipt_of(&e.get(&north(&w, FIRST), Some(&t(20, 0))).unwrap()).value(), &num("10"));
    // Before b's asof, b is not a member: 10 + 30.
    assert_eq!(receipt_of(&e.get(&north(&w, TOTAL), Some(&t(12, 30))).unwrap()).value(), &num("40"));
    // An empty slice: sum 0; first has no member.
    let east = w.cellref(TOTAL, &[("region", text("east"))]);
    let empty = receipt_of(&e.get(&east, Some(&t(20, 0))).unwrap());
    assert_eq!(empty.value(), &num("0"));
    assert_eq!(empty.inputs().len(), 1);
    assert_eq!(Manifest::from_json(&e.manifest(&empty.inputs()[0]).unwrap()).unwrap().members().len(), 0);
    // No input receipt: tx is the earliest instant; asof the start of temp's binding.
    assert_eq!((empty.tx(), empty.asof()), (MIN_INSTANT, BIND_TX));
    assert_eq!(e.get(&w.cellref(FIRST, &[("region", text("east"))]), Some(&t(20, 0))).unwrap(), faulted(Fault::Absent));
    let out = verify(&bytes_of(&e.get(&north(&w, TOTAL), Some(&t(20, 0))).unwrap()), &all_keys(), Some(&*e as &dyn Resolver), None);
    assert_eq!(out.level, Some(Level::Recomputed), "{out:?}");
    w.verify_all_issued(&e, "slices_stored");
}

#[test]
fn a_correction_to_a_member_dirties_the_reader_and_the_old_receipt_still_verifies() {
    let w = world();
    let e = w.engine();
    w.publish(&temp(&w, "north", "a"), "10", &t(10, 0));
    let b = temp(&w, "north", "b");
    w.publish(&b, "20", &t(11, 0));
    w.publish(&temp(&w, "south", "a"), "5", &t(11, 0));
    let old = e.get(&north(&w, TOTAL), Some(&t(20, 0))).unwrap();
    assert_eq!(receipt_of(&old).value(), &num("30"));
    assert_eq!(w.calls(TOTAL), 1);
    // A correction of a south member dirties nothing north.
    w.publish(&temp(&w, "south", "a"), "6", &t(12, 0));
    assert_eq!(e.memo_stats().stale, 0, "nothing north was dropped");
    w.publish(&b, "25", &t(21, 0));
    assert_eq!(e.memo_stats().stale, 1, "the reader was dropped");
    let new = receipt_of(&e.get(&north(&w, TOTAL), Some(&t(22, 0))).unwrap());
    assert_eq!(new.value(), &num("35"));
    assert_eq!(w.calls(TOTAL), 2);
    // The same slice, other members: the same slice id.
    let id = |r: &Receipt| r.inputs()[0].split(':').nth(1).unwrap().to_string();
    assert_eq!(id(&new), id(&receipt_of(&old)));
    assert_ne!(new.inputs(), receipt_of(&old).inputs());
    // Earlier asofs: the old receipt, from the memo or a fresh engine validating it.
    assert_eq!(e.get(&north(&w, TOTAL), Some(&t(20, 0))).unwrap(), old);
    assert_eq!(w.engine().get(&north(&w, TOTAL), Some(&t(20, 30))).unwrap(), old);
    assert_eq!(w.engine().get(&north(&w, TOTAL), Some(&t(23, 0))).unwrap(), e.get(&north(&w, TOTAL), Some(&t(23, 0))).unwrap());
    assert_eq!(w.calls(TOTAL), 2, "validated, not recomputed");
    // The old receipt still recomputes, at its own asof (before the correction).
    let out = verify(&bytes_of(&old), &all_keys(), Some(&*w.engine() as &dyn Resolver), None);
    assert_eq!(out.level, Some(Level::Recomputed), "{out:?}");
    w.verify_all_issued(&e, "slices_correction");
}

#[test]
fn after_a_backdated_correction_of_a_member_the_old_receipt_is_superseded() {
    // a at the 5th, b at the 10th: the receipt's asof is the 10th. a corrected with tx the
    // 8th: as of the 10th the slice's members are now others, so recomputation reads another
    // members hash of the same slice id: E_INPUT_SUPERSEDED, at level signed.
    let w = world();
    let e = w.engine();
    let a = temp(&w, "north", "a");
    w.publish(&a, "10", &t(5, 0));
    w.publish(&temp(&w, "north", "b"), "20", &t(10, 0));
    let old = receipt_of(&e.get(&north(&w, TOTAL), Some(&t(20, 0))).unwrap());
    assert_eq!(old.asof(), t(10, 0));
    w.publish(&a, "11", &t(8, 0));
    let fresh = w.engine();
    let out = verify(&w.stored_bytes(old.hash()), &all_keys(), Some(&*fresh as &dyn Resolver), None);
    assert!(out.ok, "{out:?}");
    assert_eq!((out.level, out.code), (Some(Level::Signed), Some(ErrorCode::InputSuperseded)), "{out:?}");
    // The evaluation's lineage: the same slice id, the new members hash, its manifest served.
    let ev = fresh.evaluation(old.cell(), old.asof()).unwrap();
    assert_eq!(ev.value, num("31"));
    let lineage = ev.lineage.expect("a lineage");
    assert_eq!(lineage.len(), 1);
    let (old_entry, new_entry) = (&old.inputs()[0], &lineage[0]);
    assert_ne!(old_entry, new_entry);
    assert_eq!(old_entry[..71], new_entry[..71], "slice: and the slice id");
    assert!(fresh.manifest(new_entry).is_some());
    // The answer as of the 20th now is 31, the old receipt's value 30 is historical.
    assert_eq!(receipt_of(&e.get(&north(&w, TOTAL), Some(&t(20, 0))).unwrap()).value(), &num("31"));
    w.verify_all_issued(&fresh, "slices_backdated");
}

#[test]
fn a_new_member_dirties_the_readers_whose_pattern_it_falls_into_and_nothing_else() {
    let w = world();
    let e = w.engine();
    w.publish(&temp(&w, "north", "a"), "10", &t(10, 0));
    w.publish(&temp(&w, "south", "a"), "1", &t(10, 0));
    let south = w.cellref(TOTAL, &[("region", text("south"))]);
    assert_eq!(receipt_of(&e.get(&north(&w, TOTAL), Some(&t(20, 0))).unwrap()).value(), &num("10"));
    assert_eq!(receipt_of(&e.get(&south, Some(&t(20, 0))).unwrap()).value(), &num("1"));
    // A new cell of another region, and of no slice read so far.
    w.publish(&temp(&w, "west", "a"), "7", &t(21, 0));
    assert_eq!(e.memo_stats().stale, 0);
    // A new north station: the north reader only.
    w.publish(&temp(&w, "north", "z"), "5", &t(21, 0));
    assert_eq!(e.memo_stats().stale, 1);
    w.reset_calls();
    assert_eq!(receipt_of(&e.get(&north(&w, TOTAL), Some(&t(22, 0))).unwrap()).value(), &num("15"));
    assert_eq!(receipt_of(&e.get(&south, Some(&t(22, 0))).unwrap()).value(), &num("1"));
    assert_eq!(w.calls(TOTAL), 1, "north recomputed, south from the memo");
    // A member that answers only later (stored now, with a later tx) was not a member then,
    // and is at its time: the memo does not answer past it.
    w.publish(&temp(&w, "north", "y"), "100", &t(25, 0));
    assert_eq!(receipt_of(&e.get(&north(&w, TOTAL), Some(&t(24, 0))).unwrap()).value(), &num("15"));
    assert_eq!(receipt_of(&e.get(&north(&w, TOTAL), Some(&t(26, 0))).unwrap()).value(), &num("115"));
    w.verify_all_issued(&e, "slices_new_member");
}

#[test]
fn a_running_total_is_one_cell_per_day_and_a_correction_dirties_only_the_days_after_it() {
    // x(day), stored daily for 30 days; running(day) = sum of x([first..day]): one slice read
    // per day, not a chain of days. A correction to day 10 dirties days 10 to 30 only.
    let mut w = World::new();
    const X: &str = "rates.example.org/x";
    const RUN: &str = "calc.example.net/running";
    let day_decl = r#"{"type":"date","valid":true}"#;
    w.stored_fn(X, &[("day", day_decl)]);
    w.native_fn(RUN, &[X], &[("day", day_decl)], |c, i, _| {
        let day = c.args["day"].scalar().unwrap().clone();
        let members = i.read_slice(X, &pattern(&[("day", range(date("2024-01-01"), day))]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    let e = w.engine();
    let day = |d: u32| format!("2024-01-{d:02}");
    for d in 1..=30 {
        w.publish(&w.cell(X, &[("day", date(&day(d)))]), &d.to_string(), &format!("2024-02-01T00:00:{d:02}.000000Z"));
    }
    let asof = "2024-03-01T00:00:00.000000Z";
    let run = |d: u32| w.cellref(RUN, &[("day", date(&day(d)))]);
    for d in 1..=30 {
        let r = receipt_of(&e.get(&run(d), Some(asof)).unwrap());
        assert_eq!(r.value(), &num(&(d * (d + 1) / 2).to_string()), "day {d}");
        assert_eq!(r.inputs().len(), 1, "day {d}: one slice entry");
        assert!(r.inputs()[0].starts_with("slice:"));
    }
    assert_eq!(w.calls(RUN), 30);
    // Correct day 10: 10 -> 110.
    w.publish(&w.cell(X, &[("day", date(&day(10)))]), "110", "2024-02-02T00:00:00.000000Z");
    assert_eq!(e.memo_stats().stale, 21, "days 10 to 30");
    w.reset_calls();
    for d in 1..=30 {
        let want = d * (d + 1) / 2 + if d >= 10 { 100 } else { 0 };
        assert_eq!(receipt_of(&e.get(&run(d), Some(asof)).unwrap()).value(), &num(&want.to_string()), "day {d}");
    }
    assert_eq!(w.calls(RUN), 21, "only the days on or after the correction");
    w.verify_all_issued(&e, "slices_running_total");
}

const SQ: &str = "calc.example.net/sq";
const CUBE: &str = "calc.example.net/cube";
const SUMSQ: &str = "calc.example.net/sumsq";
const N: &str = r#"{"type":"number","unit":"1","valid":false}"#;
const N_EXTENT: &str = r#"{"extent":["1","2","3","4"],"type":"number","unit":"1","valid":false}"#;

/// sq(n) = n * n over the extent 1..4; cube(n) = n^3 with no extent; sumsq(lo, hi) =
/// the sum of sq(n: [lo..hi]); sumall() = the sum of sq(n: *); sumcube = the sum of
/// cube(n: [1..3]).
fn computed_world() -> World {
    let mut w = World::new();
    w.native_fn(SQ, &[], &[("n", N_EXTENT)], |c, _, _| {
        let n = int(c.args["n"].scalar().unwrap());
        Ok(num(&(n * n).to_string()))
    });
    w.native_fn(CUBE, &[], &[("n", N)], |c, _, _| {
        let n = int(c.args["n"].scalar().unwrap());
        Ok(num(&(n * n * n).to_string()))
    });
    w.native_fn(SUMSQ, &[SQ], &[("hi", N), ("lo", N)], |c, i, _| {
        let (lo, hi) = (c.args["lo"].scalar().unwrap().clone(), c.args["hi"].scalar().unwrap().clone());
        let members = i.read_slice(SQ, &pattern(&[("n", range(lo, hi))]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w.native_fn("calc.example.net/sumall", &[SQ], &[], |_, i, _| {
        let members = i.read_slice(SQ, &pattern(&[("n", open())]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w.native_fn("calc.example.net/sumcube", &[CUBE], &[], |_, i, _| {
        let members = i.read_slice(CUBE, &pattern(&[("n", range(num("1"), num("3")))]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w
}

#[test]
fn a_slice_over_a_computed_function_evaluates_and_memoises_its_extent() {
    let w = computed_world();
    let e = w.engine();
    let all = receipt_of(&e.get(&w.cellref("calc.example.net/sumall", &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(all.value(), &num("30"));
    assert_eq!(w.calls(SQ), 4, "each member evaluated once");
    assert_eq!(all.inputs().len(), 1);
    let m = Manifest::from_json(&e.manifest(&all.inputs()[0]).unwrap()).unwrap();
    assert_eq!(m.members().len(), 4);
    // Each member is a computed cell with its own receipt, listed in the manifest.
    let sq2 = receipt_of(&e.get(&w.cellref(SQ, &[("n", num("2"))]), Some(&t(20, 0))).unwrap());
    assert!(m.members().contains(&sq2.hash().to_string()));
    // A range within the extent: the members are memoised.
    let r = receipt_of(&e.get(&w.cellref(SUMSQ, &[("hi", num("3")), ("lo", num("2"))]), Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("13"));
    // A range wider than the extent enumerates the extent only; one below it is empty.
    let wide = receipt_of(&e.get(&w.cellref(SUMSQ, &[("hi", num("100")), ("lo", num("-5"))]), Some(&t(20, 0))).unwrap());
    assert_eq!(wide.value(), &num("30"));
    let none = receipt_of(&e.get(&w.cellref(SUMSQ, &[("hi", num("0")), ("lo", num("-5"))]), Some(&t(20, 0))).unwrap());
    assert_eq!(none.value(), &num("0"));
    assert_eq!(w.calls(SQ), 4, "members memoised");
    // A range over a number argument with no extent: by 1.
    let cubes = receipt_of(&e.get(&w.cellref("calc.example.net/sumcube", &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(cubes.value(), &num("36"));
    assert_eq!(w.calls(CUBE), 3);
    // A fresh engine validates the stored versions: no runtime call.
    w.reset_calls();
    let fresh = w.engine();
    assert_eq!(receipt_of(&fresh.get(&w.cellref("calc.example.net/sumall", &[]), Some(&t(21, 0))).unwrap()).hash(), all.hash());
    assert_eq!(w.calls(SQ) + w.calls("calc.example.net/sumall"), 0);
    w.verify_all_issued(&e, "slices_computed");
}

#[test]
fn an_open_slice_over_a_computed_function_without_an_extent_is_refused() {
    let mut w = computed_world();
    w.native_fn("calc.example.net/allcubes", &[CUBE], &[], |_, i, _| {
        let members = i.read_slice(CUBE, &pattern(&[("n", open())]))?;
        Ok(num(&members.len().to_string()))
    });
    let e = w.engine();
    match e.get(&w.cellref("calc.example.net/allcubes", &[]), Some(&t(20, 0))) {
        Err(EngineError::Invalid(err)) => assert_eq!(err.code(), ErrorCode::Slice, "{err}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_member_on_the_evaluation_stack_is_a_cycle() {
    // loop(n) over the extent [1, 2]: loop(2) reads the sum of loop(*), which reads loop(2).
    let mut w = World::new();
    const LOOP: &str = "calc.example.net/loop";
    const SUM: &str = "calc.example.net/loopsum";
    w.native_fn(LOOP, &[SUM], &[("n", r#"{"extent":["1","2"],"type":"number","unit":"1","valid":false}"#)], |c, i, _| {
        let n = int(c.args["n"].scalar().unwrap());
        if n == 2 {
            return i.read(SUM, &args(&[]));
        }
        Ok(num("1"))
    });
    w.native_fn(SUM, &[LOOP], &[], |_, i, _| {
        let members = i.read_slice(LOOP, &pattern(&[("n", open())]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    let e = w.engine();
    match e.get(&w.cellref(SUM, &[]), Some(&t(20, 0))) {
        Err(EngineError::Cycle { cell_id }) => assert_eq!(cell_id, w.cell(SUM, &[]).id()),
        other => panic!("{other:?}"),
    }
    assert_eq!(w.db().count("receipts"), 0);
}

#[test]
fn a_computed_member_without_a_value_makes_the_read_absent() {
    // half(n) over [1, 2, 3] faults at 2; the sum over half(*) is E_FORMULA_ABSENT; a reader
    // that catches it (as `otherwise` does) has a value.
    let mut w = World::new();
    const HALF: &str = "calc.example.net/half";
    w.native_fn(HALF, &[], &[("n", r#"{"extent":["1","2","3"],"type":"number","unit":"1","valid":false}"#)], |c, _, _| {
        match int(c.args["n"].scalar().unwrap()) {
            2 => Err(EvalError::Fault(Fault::DivZero)),
            n => Ok(num(&n.to_string())),
        }
    });
    w.native_fn("calc.example.net/halfsum", &[HALF], &[], |_, i, _| {
        let members = i.read_slice(HALF, &pattern(&[("n", open())]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w.native_fn("calc.example.net/otherwise", &[HALF], &[], |_, i, _| {
        match i.read_slice(HALF, &pattern(&[("n", open())])) {
            Ok(members) => Ok(sum(members.iter().map(|m| &m.input.value))),
            Err(EvalError::Fault(Fault::Absent)) => Ok(num("0")),
            Err(e) => Err(e),
        }
    });
    w.native_fn("calc.example.net/one", &[HALF], &[], |_, i, _| {
        let members = i.read_slice(HALF, &pattern(&[("n", range(num("1"), num("1")))]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    let e = w.engine();
    assert_eq!(e.get(&w.cellref("calc.example.net/halfsum", &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::Absent));
    let caught = receipt_of(&e.get(&w.cellref("calc.example.net/otherwise", &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(caught.value(), &num("0"));
    assert!(caught.inputs().is_empty(), "the slice was not read to the end");
    assert_eq!(caught.asof(), BIND_TX, "the binding of half was relied on");
    // A slice without the faulting member is fine.
    assert_eq!(receipt_of(&e.get(&w.cellref("calc.example.net/one", &[]), Some(&t(20, 0))).unwrap()).value(), &num("1"));
    w.verify_all_issued(&e, "slices_absent");
}

#[test]
fn versions_and_transaction_time_reads() {
    // fx has versions at the 10th, 12th, 14th and 16th (values 1, 2, 3, 4).
    // window() = the sum of versions(fx, 11th, 15th): the 12th and 14th.
    // past() = asof(fx, 12th 12:00): the version of the 12th.
    let mut w = World::new();
    const FX: &str = "rates.example.org/fx";
    w.stored_fn(FX, &[]);
    w.native_fn("calc.example.net/window", &[FX], &[], |_, i, _| {
        let vs = i.read_versions(FX, &args(&[]), &t(11, 0), &t(15, 0))?;
        Ok(sum(vs.iter().map(|v| &v.value)))
    });
    w.native_fn("calc.example.net/past", &[FX], &[], |_, i, _| Ok(i.read_asof(FX, &args(&[]), &t(12, 12))?.value));
    w.native_fn("calc.example.net/ancient", &[FX], &[], |_, i, _| {
        Ok(i.read_asof(FX, &args(&[]), "2019-06-01T00:00:00.000000Z")?.value)
    });
    let e = w.engine();
    let fx = w.cell(FX, &[]);
    let mut h = Vec::new();
    for (k, d) in [10, 12, 14, 16].iter().enumerate() {
        h.push(w.publish(&fx, &(k + 1).to_string(), &t(*d, 0)));
    }
    let window = w.cellref("calc.example.net/window", &[]);
    let r = receipt_of(&e.get(&window, Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("5"));
    assert_eq!(r.inputs().len(), 1);
    let m = Manifest::from_json(&e.manifest(&r.inputs()[0]).unwrap()).unwrap();
    assert!(m.slice().is_version_slice());
    let mut want = vec![h[1].clone(), h[2].clone()];
    want.sort();
    assert_eq!(m.members(), want.as_slice());
    // tx: the latest member's; asof: `to`, later than every member's asof.
    assert_eq!((r.tx(), r.asof()), (t(14, 0).as_str(), t(15, 0).as_str()));
    // `to` after the evaluation's asof: E_FORMULA_FUTURE.
    assert_eq!(e.get(&window, Some(&t(14, 30))).unwrap(), faulted(Fault::Future));
    // asof(fx, T): the version answering as of T, an ordinary input; T is an asof candidate.
    let past = w.cellref("calc.example.net/past", &[]);
    let p = receipt_of(&e.get(&past, Some(&t(20, 0))).unwrap());
    assert_eq!(p.value(), &num("2"));
    assert_eq!(p.inputs(), [h[1].clone()].as_slice());
    assert_eq!((p.tx(), p.asof()), (t(12, 0).as_str(), t(12, 12).as_str()));
    assert_eq!(e.get(&past, Some(&t(12, 11))).unwrap(), faulted(Fault::Future));
    // A later version of fx does not change it, from the memo or a fresh engine.
    w.publish(&fx, "5", &t(21, 0));
    assert_eq!(receipt_of(&e.get(&past, Some(&t(22, 0))).unwrap()).value(), &num("2"));
    assert_eq!(receipt_of(&w.engine().get(&past, Some(&t(22, 0))).unwrap()).value(), &num("2"));
    // T before fx's name was bound: the read finds no binding.
    match e.get(&w.cellref("calc.example.net/ancient", &[]), Some(&t(20, 0))) {
        Err(EngineError::NameUnresolved { fn_name, asof }) => {
            assert_eq!((fn_name.as_str(), asof.as_str()), (FX, "2019-06-01T00:00:00.000000Z"))
        }
        other => panic!("{other:?}"),
    }
    w.verify_all_issued(&e, "slices_versions_asof");
}

#[test]
fn a_transaction_time_read_is_validated_at_its_time() {
    // past() = asof(s, 12th): s's answer as of the 12th. s has a version at the 10th (1) and
    // the 14th (2); asked on the 20th. Then u, a second cell read normally, changes: the
    // dropped answer is rechecked, s as of the 12th still gives the same receipt.
    let mut w = World::new();
    const S: &str = "rates.example.org/s";
    const U: &str = "rates.example.org/u";
    w.stored_fn(S, &[]);
    w.stored_fn(U, &[]);
    w.native_fn("calc.example.net/past", &[S, U], &[], |_, i, _| {
        let s = i.read_asof(S, &args(&[]), &t(12, 0))?;
        let u = i.read(U, &args(&[]))?;
        Ok(add(&s.value, &u))
    });
    let e = w.engine();
    let s = w.cell(S, &[]);
    let hs = w.publish(&s, "1", &t(10, 0));
    w.publish(&s, "2", &t(14, 0));
    w.publish(&w.cell(U, &[]), "100", &t(10, 0));
    let past = w.cellref("calc.example.net/past", &[]);
    let r = receipt_of(&e.get(&past, Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("101"));
    assert!(r.inputs().contains(&hs), "s as of the 12th");
    w.publish(&w.cell(U, &[]), "100", &t(21, 0));
    w.reset_calls();
    let r2 = receipt_of(&e.get(&past, Some(&t(22, 0))).unwrap());
    assert_eq!(r2.value(), &num("101"));
    assert_eq!(w.calls("calc.example.net/past"), 0, "rechecked: same values");
    // A fresh engine (the version is not kept for validation: it read at a transaction
    // time) recomputes, and gets the same value.
    assert_eq!(receipt_of(&w.engine().get(&past, Some(&t(23, 0))).unwrap()).value(), &num("101"));
    w.verify_all_issued(&e, "slices_tx_read");
}

#[test]
fn malformed_slice_reads_are_the_runtimes_mistake() {
    let mut w = world();
    // A pattern with no open or ranged argument; a missing argument; a boolean range.
    w.native_fn("calc.example.net/closed", &[TEMP], &[], |_, i, _| {
        i.read_slice(TEMP, &pattern(&[("region", bound(text("n"))), ("station", bound(text("a")))]))?;
        Ok(num("0"))
    });
    w.native_fn("calc.example.net/short", &[TEMP], &[], |_, i, _| {
        i.read_slice(TEMP, &pattern(&[("station", open())]))?;
        Ok(num("0"))
    });
    w.native_fn("calc.example.net/undeclared", &[], &[], |_, i, _| {
        i.read_slice(TEMP, &pattern(&[("region", open()), ("station", open())]))?;
        Ok(num("0"))
    });
    w.native_fn("calc.example.net/badrange", &[TEMP], &[], |_, i, _| {
        i.read_slice(TEMP, &pattern(&[("region", Pattern::Range { lo: num("1"), hi: num("2") }), ("station", open())]))?;
        Ok(num("0"))
    });
    let e = w.engine();
    let get = |n: &str| e.get(&w.cellref(n, &[]), Some(&t(20, 0)));
    match get("calc.example.net/closed") {
        Err(EngineError::Invalid(err)) => assert_eq!(err.code(), ErrorCode::Slice),
        other => panic!("{other:?}"),
    }
    match get("calc.example.net/short") {
        Err(EngineError::Invalid(err)) => assert_eq!(err.code(), ErrorCode::ArgMissing),
        other => panic!("{other:?}"),
    }
    assert!(matches!(get("calc.example.net/undeclared"), Err(EngineError::UndeclaredInput { .. })));
    match get("calc.example.net/badrange") {
        Err(EngineError::Invalid(err)) => assert_eq!(err.code(), ErrorCode::ArgType),
        other => panic!("{other:?}"),
    }
    assert_eq!(w.db().count("computed_versions"), 0);
}
