//! Scoped evaluation (Build 2 Part 3, E1; SPEC 5.9): `Engine::whatif`. Tests first: the API
//! below does not exist yet.
//!
//! The API these tests specify (all exported from the crate root):
//!
//! ```text
//! Engine::whatif(&self, cell: &CellRef, asof: Option<&str>, overrides: &Overrides,
//!                solve: Option<&Solve>) -> Result<ScenarioAnswer, EngineError>
//!
//! Overrides::new() -> Overrides                       (also Default)
//!   .value(self, cell: CellRef, value: Scalar) -> Overrides
//!   .slice(self, slice: CellRef, add: Vec<(CellRef, Scalar)>, remove: Vec<CellRef>) -> Overrides
//!   .len(&self) -> usize, .is_empty(&self) -> bool
//!   (a slice is a CellRef whose open arguments are `Arg::Slice`; no asof in any CellRef)
//!
//! ScenarioAnswer { cell: Cell, ty: Type, value: Scalar, asof: String,
//!                  overrides: Vec<ScenarioOverride>, solved: Option<(Cell, Scalar)> }
//!   #[non_exhaustive], public fields, Clone + Debug + PartialEq. No receipt.
//! ScenarioOverride::Value { cell: Cell, value: Scalar }
//!                 | Slice { slice: Cell, add: Vec<(Cell, Scalar)>, remove: Vec<Cell> }
//!   .id(&self) -> String   the id of its cell or slice; ScenarioAnswer.overrides is sorted by it
//!
//! EngineError::Override(String)       the code: E_CANON_TYPE, E_SLICE_MEMBER,
//!                                     E_CANON_ARG_DUPLICATE (65 or more overrides: any Override)
//! EngineError::NotCovered { fault: Option<Fault> }   whatif's not_covered (a fault in scope,
//!                                     or a cell with no value), as Answer::NotCovered for get
//! EngineError::WhatifNoSolution       E_WHATIF_NO_SOLUTION (see tests/solver.rs)
//! ```

mod common;

use std::sync::{Arc, Barrier};
use std::time::Duration;

use common::*;
use datumesh_core::{Arg, CellRef, Fault, Type};
use datumesh_engine::{Engine, EngineError, EvalError, Overrides, ScenarioOverride};

pub const A: &str = "rates.example.org/a";
pub const B: &str = "rates.example.org/b";
pub const C: &str = "rates.example.org/c";
pub const D: &str = "rates.example.org/d";
pub const SUM: &str = "calc.example.net/sum";
pub const SIDE: &str = "calc.example.net/side";
pub const TOP: &str = "calc.example.net/top";
pub const QUOT: &str = "calc.example.net/quot";
pub const TEMP: &str = "rates.example.org/temp";
pub const TOTAL: &str = "calc.example.net/total";
pub const N: &str = "rates.example.org/n";
const STR: &str = r#"{"type":"string","valid":false}"#;

/// a, b, c, d stored (d never published); sum = a + b; side = c + c; top = sum + side;
/// quot = a / b (div-zero fault when b is 0). Published at t(10, 0): a 1, b 2, c 10.
/// sum sleeps 20 ms (for the concurrency test).
fn world() -> World {
    let mut w = World::new();
    for f in [A, B, C, D] {
        w.stored_fn(f, &[]);
    }
    w.native_fn(SUM, &[A, B], &[], |_, i, _| {
        std::thread::sleep(Duration::from_millis(20));
        Ok(add(&i.read(A, &args(&[]))?, &i.read(B, &args(&[]))?))
    });
    w.native_fn(SIDE, &[C], &[], |_, i, _| {
        let c = i.read(C, &args(&[]))?;
        Ok(add(&c, &c))
    });
    w.native_fn(TOP, &[SUM, SIDE], &[], |_, i, _| Ok(add(&i.read(SUM, &args(&[]))?, &i.read(SIDE, &args(&[]))?)));
    w.native_fn(QUOT, &[A, B], &[], |_, i, _| {
        let a = int(&i.read(A, &args(&[]))?);
        let b = int(&i.read(B, &args(&[]))?);
        if b == 0 {
            return Err(EvalError::Fault(Fault::DivZero));
        }
        Ok(num(&(a / b).to_string()))
    });
    w.publish(&w.cell(A, &[]), "1", &t(10, 0));
    w.publish(&w.cell(B, &[]), "2", &t(10, 0));
    w.publish(&w.cell(C, &[]), "10", &t(10, 0));
    w
}

fn r(w: &World, f: &str) -> CellRef {
    w.cellref(f, &[])
}

#[test]
fn a_value_override_changes_a_dependent_and_only_in_the_scenario() {
    let w = world();
    let e = w.engine();
    let before = receipt_of(&e.get(&r(&w, SUM), None).unwrap());
    assert_eq!(before.value(), &num("3"));
    let o = Overrides::new().value(r(&w, A), num("11"));
    let s = e.whatif(&r(&w, SUM), None, &o, None).unwrap();
    assert_eq!(s.value, num("13"));
    assert_eq!(s.ty, Type::Number);
    assert_eq!(s.asof, NOW, "no asof: the clock's now");
    assert_eq!(s.cell, w.cell(SUM, &[]));
    assert!(s.solved.is_none());
    assert_eq!(s.overrides.len(), 1);
    match &s.overrides[0] {
        ScenarioOverride::Value { cell, value } => {
            assert_eq!(cell, &w.cell(A, &[]));
            assert_eq!(value, &num("11"));
        }
        other => panic!("{other:?}"),
    }
    // The real cell is unchanged: same receipt.
    assert_eq!(receipt_of(&e.get(&r(&w, SUM), None).unwrap()).hash(), before.hash());
    // An overridden cell answers its value, and is never read from the store: d has no
    // version at all.
    let s = e.whatif(&r(&w, D), None, &Overrides::new().value(r(&w, D), num("7")), None).unwrap();
    assert_eq!(s.value, num("7"));
    let s = e.whatif(&r(&w, A), None, &Overrides::new().value(r(&w, A), num("5")), None).unwrap();
    assert_eq!(s.value, num("5"));
    // Two overrides, transitively: top = (a + b) + (c + c).
    let o = Overrides::new().value(r(&w, A), num("100")).value(r(&w, C), num("1"));
    assert_eq!(e.whatif(&r(&w, TOP), None, &o, None).unwrap().value, num("104"));
    // Overriding the cell asked for (a computed one) answers the override.
    let o = Overrides::new().value(r(&w, TOP), num("-1"));
    assert_eq!(e.whatif(&r(&w, TOP), None, &o, None).unwrap().value, num("-1"));
}

#[test]
fn the_scenario_is_evaluated_as_of_the_given_instant() {
    let w = world();
    let e = w.engine();
    w.publish(&w.cell(A, &[]), "5", &t(12, 0));
    let o = Overrides::new().value(r(&w, B), num("100"));
    let early = e.whatif(&r(&w, SUM), Some(&t(11, 0)), &o, None).unwrap();
    assert_eq!((early.value, early.asof.as_str()), (num("101"), t(11, 0).as_str()));
    let late = e.whatif(&r(&w, SUM), Some(&t(13, 0)), &o, None).unwrap();
    assert_eq!((late.value, late.asof.as_str()), (num("105"), t(13, 0).as_str()));
    // As of an instant before the data: a cell with no value (not an override's).
    match e.whatif(&r(&w, SUM), Some(&t(9, 0)), &o, None) {
        Err(EngineError::NotCovered { fault: None }) => {}
        other => panic!("{other:?}"),
    }
    // Overrides sorted as the spec says: by cell id, whatever their order in the request.
    let o = Overrides::new().value(r(&w, C), num("1")).value(r(&w, B), num("2")).value(r(&w, A), num("3"));
    let s = e.whatif(&r(&w, TOP), None, &o, None).unwrap();
    let ids: Vec<String> = s.overrides.iter().map(|o| o.id()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids, sorted);
}

#[test]
fn what_does_not_depend_on_the_overrides_answers_from_the_normal_memo() {
    let w = world();
    let e = w.engine();
    let side_before = receipt_of(&e.get(&r(&w, SIDE), None).unwrap());
    let top_before = receipt_of(&e.get(&r(&w, TOP), None).unwrap());
    assert_eq!(top_before.value(), &num("23"));
    w.reset_calls();
    let o = Overrides::new().value(r(&w, A), num("11"));
    let s = e.whatif(&r(&w, TOP), None, &o, None).unwrap();
    assert_eq!(s.value, num("33"));
    assert_eq!(w.calls(SIDE), 0, "side does not depend on a: from the memo, not evaluated");
    assert_eq!(w.calls(SUM), 1, "sum depends on a: evaluated in the scope");
    assert_eq!(w.calls(TOP), 1);
    // The unchanged part keeps its receipt, and a fresh engine (empty memo) answers a
    // non-dependent from the store without evaluating it.
    assert_eq!(receipt_of(&e.get(&r(&w, SIDE), None).unwrap()).hash(), side_before.hash());
    let fresh = w.engine();
    w.reset_calls();
    let s = fresh.whatif(&r(&w, TOP), None, &o, None).unwrap();
    assert_eq!(s.value, num("33"));
    assert_eq!(w.calls(SIDE), 0, "a stored computed version answers a non-dependent");
    // Not dependent at all: the same value get gives, and the real receipt is unchanged.
    let s = e.whatif(&r(&w, SIDE), None, &o, None).unwrap();
    assert_eq!(&s.value, side_before.value());
    assert_eq!(receipt_of(&e.get(&r(&w, SIDE), None).unwrap()).hash(), side_before.hash());
}

#[test]
fn a_scenario_writes_nothing() {
    let w = world();
    let e = w.engine();
    let o = Overrides::new().value(r(&w, A), num("11")).value(r(&w, C), num("0"));
    let snapshot = |w: &World, e: &Engine| (rows(w), w.issued_receipts().len(), e.memo_stats());
    // Cold: nothing computed yet anywhere; the scenario computes everything in its scope.
    let before = snapshot(&w, &e);
    assert_eq!(before.0[1], 0, "no computed version yet");
    let s = e.whatif(&r(&w, TOP), None, &o, None).unwrap();
    assert_eq!(s.value, num("13"));
    assert_eq!(snapshot(&w, &e), before, "cold: no receipt, version, manifest, memo entry");
    // Warm.
    e.get(&r(&w, TOP), None).unwrap();
    let before = snapshot(&w, &e);
    let warm = before;
    assert!(before.0[1] > 0);
    e.whatif(&r(&w, TOP), None, &o, None).unwrap();
    e.whatif(&r(&w, TOP), Some(&t(11, 0)), &o, None).unwrap();
    assert_eq!(snapshot(&w, &e), before, "warm: nothing written");
    // The same with a memo of capacity 0 (every request validates through the store).
    let bare = w.engine_with(|b| b.memo_capacity(0));
    let before = snapshot(&w, &bare);
    bare.whatif(&r(&w, TOP), None, &o, None).unwrap();
    assert_eq!(snapshot(&w, &bare), before);
    // And a scenario that fails writes nothing either.
    let bad = Overrides::new().value(r(&w, A), text("x"));
    assert!(e.whatif(&r(&w, TOP), None, &bad, None).is_err());
    assert_eq!(snapshot(&w, &e), warm, "a failed scenario: nothing written, warm snapshot");
}

#[test]
fn the_scope_memo_is_dropped_after_the_request() {
    let w = world();
    let e = w.engine();
    let o = Overrides::new().value(r(&w, A), num("11"));
    e.whatif(&r(&w, SUM), None, &o, None).unwrap();
    assert_eq!(w.calls(SUM), 1);
    // Nothing is kept: the same scenario evaluates sum again.
    e.whatif(&r(&w, SUM), None, &o, None).unwrap();
    assert_eq!(w.calls(SUM), 2, "the scope memo did not survive the request");
    // And the real answer is computed from the real inputs, not from the scope.
    let real = receipt_of(&e.get(&r(&w, SUM), None).unwrap());
    assert_eq!(real.value(), &num("3"));
    assert_eq!(w.calls(SUM), 3);
    // Another scenario right after sees nothing of the first.
    let o2 = Overrides::new().value(r(&w, B), num("20"));
    assert_eq!(e.whatif(&r(&w, SUM), None, &o2, None).unwrap().value, num("21"));
}

fn slice_world() -> World {
    let mut w = World::new();
    w.stored_fn(TEMP, &[("region", STR), ("station", STR)]);
    w.native_fn(TOTAL, &[TEMP], &[("region", STR)], |c, i, b| {
        let region = c.args["region"].scalar().unwrap().clone();
        let members = i.read_slice(TEMP, &pattern(&[("region", bound(region)), ("station", open())]))?;
        b.charge(members.len() as u64)?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    for (s, v) in [("a", "10"), ("b", "20"), ("c", "30")] {
        w.publish(&w.cell(TEMP, &[("region", text("north")), ("station", text(s))]), v, &t(10, 0));
    }
    w
}

fn temp(w: &World, station: &str) -> CellRef {
    w.cellref(TEMP, &[("region", text("north")), ("station", text(station))])
}

fn north_slice(w: &World) -> CellRef {
    let mut c = w.cellref(TEMP, &[("region", text("north"))]);
    c.args.insert("station".into(), Arg::Slice);
    c
}

#[test]
fn a_slice_override_adds_and_removes_members_for_a_computed_reader() {
    let w = slice_world();
    let e = w.engine();
    let total = w.cellref(TOTAL, &[("region", text("north"))]);
    let real = receipt_of(&e.get(&total, None).unwrap());
    assert_eq!(real.value(), &num("60"));
    let value = |o: Overrides| e.whatif(&total, None, &o, None).unwrap().value;
    let slice = north_slice(&w);
    assert_eq!(value(Overrides::new().slice(slice.clone(), vec![(temp(&w, "d"), num("100"))], vec![])), num("160"));
    assert_eq!(value(Overrides::new().slice(slice.clone(), vec![], vec![temp(&w, "a")])), num("50"));
    assert_eq!(
        value(Overrides::new().slice(slice.clone(), vec![(temp(&w, "d"), num("100"))], vec![temp(&w, "b")])),
        num("140")
    );
    assert_eq!(
        value(Overrides::new().slice(slice.clone(), vec![], vec![temp(&w, "a"), temp(&w, "b"), temp(&w, "c")])),
        num("0")
    );
    // A value override of a member and a slice override together.
    let o =
        Overrides::new().value(temp(&w, "a"), num("1")).slice(slice.clone(), vec![(temp(&w, "d"), num("100"))], vec![]);
    assert_eq!(value(o), num("151"));
    // An added cell read alone answers its override value.
    let o = Overrides::new().slice(slice.clone(), vec![(temp(&w, "d"), num("100"))], vec![]);
    assert_eq!(e.whatif(&temp(&w, "d"), None, &o, None).unwrap().value, num("100"));
    // The canonical form: the slice override with add and remove sorted by id.
    let s = e
        .whatif(
            &total,
            None,
            &Overrides::new().slice(
                slice,
                vec![(temp(&w, "e"), num("1")), (temp(&w, "d"), num("2"))],
                vec![temp(&w, "c"), temp(&w, "a")],
            ),
            None,
        )
        .unwrap();
    match &s.overrides[0] {
        ScenarioOverride::Slice { add, remove, .. } => {
            let add_ids: Vec<String> = add.iter().map(|(c, _)| c.id()).collect();
            let rem_ids: Vec<String> = remove.iter().map(|c| c.id()).collect();
            assert!(add_ids.windows(2).all(|p| p[0] < p[1]), "{add_ids:?}");
            assert!(rem_ids.windows(2).all(|p| p[0] < p[1]), "{rem_ids:?}");
            assert_eq!((add.len(), remove.len()), (2, 2));
        }
        other => panic!("{other:?}"),
    }
    // Nothing real changed.
    assert_eq!(receipt_of(&e.get(&total, None).unwrap()).hash(), real.hash());
    assert_eq!(w.db().count("versions"), 3, "no version for an added member");
    assert_eq!(w.db().count("computed_versions"), 1);
}

fn code(r: Result<datumesh_engine::ScenarioAnswer, EngineError>) -> String {
    match r {
        Err(EngineError::Override(c)) => c,
        other => panic!("expected an override error, got {other:?}"),
    }
}

#[test]
fn override_validation_codes() {
    let w = world();
    let e = w.engine();
    let go = |w: &World, o: Overrides| e.whatif(&r(w, SUM), None, &o, None);
    // A value of another type than the function's return type (number).
    assert_eq!(code(go(&w, Overrides::new().value(r(&w, A), text("x")))), "E_CANON_TYPE");
    assert_eq!(code(go(&w, Overrides::new().value(r(&w, A), date("2024-03-10")))), "E_CANON_TYPE");
    // The same cell twice.
    assert_eq!(
        code(go(&w, Overrides::new().value(r(&w, A), num("1")).value(r(&w, A), num("2")))),
        "E_CANON_ARG_DUPLICATE"
    );
    assert_eq!(
        code(go(&w, Overrides::new().value(r(&w, A), num("1")).value(r(&w, A), num("1")))),
        "E_CANON_ARG_DUPLICATE"
    );

    let sw = slice_world();
    let e2 = sw.engine();
    let total = sw.cellref(TOTAL, &[("region", text("north"))]);
    let go2 = |o: Overrides| e2.whatif(&total, None, &o, None);
    let slice = north_slice(&sw);
    // A removed cell that is not in the slice.
    assert_eq!(code(go2(Overrides::new().slice(slice.clone(), vec![], vec![temp(&sw, "zzz")]))), "E_SLICE_MEMBER");
    // A member both added and removed.
    assert_eq!(
        code(go2(Overrides::new().slice(slice.clone(), vec![(temp(&sw, "d"), num("1"))], vec![temp(&sw, "d")]))),
        "E_SLICE_MEMBER"
    );
    // An added cell that is not a cell of the slice (another region).
    let south = sw.cellref(TEMP, &[("region", text("south")), ("station", text("a"))]);
    assert_eq!(code(go2(Overrides::new().slice(slice.clone(), vec![(south, num("1"))], vec![]))), "E_SLICE_MEMBER");
    // A member of the wrong type.
    assert_eq!(
        code(go2(Overrides::new().slice(slice.clone(), vec![(temp(&sw, "d"), text("x"))], vec![]))),
        "E_CANON_TYPE"
    );
    // Two overrides of one slice; and one cell added twice.
    assert_eq!(
        code(go2(Overrides::new().slice(slice.clone(), vec![], vec![temp(&sw, "a")]).slice(
            slice.clone(),
            vec![],
            vec![temp(&sw, "b")]
        ))),
        "E_CANON_ARG_DUPLICATE"
    );
    assert_eq!(
        code(go2(Overrides::new().slice(
            slice.clone(),
            vec![(temp(&sw, "d"), num("1")), (temp(&sw, "d"), num("2"))],
            vec![]
        ))),
        "E_CANON_ARG_DUPLICATE"
    );
    assert_eq!(sw.db().count("versions"), 3);
}

#[test]
fn at_most_64_overrides() {
    let mut w = World::new();
    w.stored_fn(N, &[("i", r#"{"type":"number","unit":"1","valid":false}"#)]);
    w.native_fn("calc.example.net/one", &[], &[], |_, _, _| Ok(num("1")));
    let e = w.engine();
    let cell = |i: u32| w.cellref(N, &[("i", num(&i.to_string()))]);
    let build = |n: u32| (0..n).fold(Overrides::new(), |o, i| o.value(cell(i), num("1")));
    let one = w.cellref("calc.example.net/one", &[]);
    let ok = build(64);
    assert_eq!(ok.len(), 64);
    assert_eq!(e.whatif(&one, None, &ok, None).unwrap().value, num("1"));
    match e.whatif(&one, None, &build(65), None) {
        Err(EngineError::Override(_)) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_fault_in_scope_is_not_covered_with_the_fault_and_nothing_is_stored() {
    let w = world();
    let e = w.engine();
    let quot = r(&w, QUOT);
    assert_eq!(e.whatif(&quot, None, &Overrides::new().value(r(&w, B), num("1")), None).unwrap().value, num("1"));
    match e.whatif(&quot, None, &Overrides::new().value(r(&w, B), num("0")), None) {
        Err(EngineError::NotCovered { fault: Some(Fault::DivZero) }) => {}
        other => panic!("{other:?}"),
    }
    // The real cell is fine.
    assert_eq!(receipt_of(&e.get(&quot, None).unwrap()).value(), &num("0"));
    // An input with no value (d never published, not overridden) is not covered, no fault;
    // overriding it gives the value.
    let mut w = w;
    w.native_fn("calc.example.net/readd", &[D], &[], |_, i, _| i.read(D, &args(&[])));
    let e = w.engine();
    let readd = w.cellref("calc.example.net/readd", &[]);
    match e.whatif(&readd, None, &Overrides::new().value(r(&w, A), num("1")), None) {
        Err(EngineError::NotCovered { fault: None }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(e.whatif(&readd, None, &Overrides::new().value(r(&w, D), num("9")), None).unwrap().value, num("9"));
}

#[test]
fn two_concurrent_scenarios_do_not_leak_into_each_other() {
    let w = world();
    let e = w.engine();
    let before = (rows(&w), e.memo_stats());
    let cells = (r(&w, SUM), r(&w, TOP), r(&w, A));
    let n = 6;
    let start = Arc::new(Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|k| {
            let (e, start, cells) = (e.clone(), start.clone(), cells.clone());
            std::thread::spawn(move || {
                start.wait();
                for round in 0..4 {
                    let a = (k * 100 + round) as i64;
                    let o = Overrides::new().value(cells.2.clone(), num(&a.to_string()));
                    let s = e.whatif(&cells.0, None, &o, None).unwrap();
                    assert_eq!(s.value, num(&(a + 2).to_string()), "thread {k} round {round}");
                    let t = e.whatif(&cells.1, None, &o, None).unwrap();
                    assert_eq!(t.value, num(&(a + 22).to_string()), "thread {k} round {round}");
                    // A real read in between is never a scenario's value.
                    let real = e.get(&cells.0, None).unwrap();
                    match real {
                        datumesh_engine::Answer::Receipt(b) => {
                            assert_eq!(datumesh_core::Receipt::from_json(&b).unwrap().value(), &num("3"))
                        }
                        other => panic!("{other:?}"),
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let after = (rows(&w), e.memo_stats());
    // Only the real reads of sum wrote anything: one computed version, one receipt.
    assert_eq!(after.0[1], before.0[1] + 1);
    assert_eq!(receipt_of(&e.get(&r(&w, TOP), None).unwrap()).value(), &num("23"));
}
