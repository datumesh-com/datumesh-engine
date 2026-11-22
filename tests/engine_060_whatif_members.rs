//! Engine 0.6.0, R3 (CONTRACT-round-0.7; SPEC 5.9): a whatif slice override may replace a
//! stored slice's WHOLE member set (`members`), which may be empty. Tests first.
//!
//! API ASSUMED (the smallest additions; all ADDITIVE to 0.5.0's whatif API, see tests/whatif.rs):
//!
//! ```text
//! Overrides::slice_members(self, slice: CellRef, members: Vec<(CellRef, Scalar)>) -> Overrides
//!     // the slice's member set becomes exactly `members` (cells and values), possibly empty
//! Overrides::slice_full(self, slice: CellRef, members: Option<Vec<(CellRef, Scalar)>>,
//!                       add: Vec<(CellRef, Scalar)>, remove: Vec<CellRef>) -> Overrides
//!     // the general form (`slice` is `slice_full(.., None, add, remove)`); `members: Some`
//!     // together with a non-empty `add` or `remove` is refused when the scenario is built
//! ScenarioOverride::Members { slice: Cell, members: Vec<(Cell, Scalar)> }
//!     // a new variant (ScenarioOverride is #[non_exhaustive]); members sorted by cell id,
//!     // present even when empty; `id()` is the slice's id
//! ```
//!
//! Errors: `EngineError::Override("E_SLICE_MEMBER")` for `members` with `add`/`remove`, and
//! for a member cell that is not a cell of the slice (by its arguments); an empty `members`
//! is fine. An override of a slice with `members` twice, or with any other override of the
//! same slice, is E_CANON_ARG_DUPLICATE as for the other forms. Nothing is written.

mod common;

use common::*;
use datumesh_core::{Arg, CellRef, Fault};
use datumesh_engine::{Engine, EngineError, EvalError, Overrides, ScenarioAnswer, ScenarioOverride};

const TEMP: &str = "rates.example.org/temp";
const TOTAL: &str = "calc.example.net/total";
const MEAN: &str = "calc.example.net/mean";
const COUNT: &str = "calc.example.net/count";
const STR: &str = r#"{"type":"string","valid":false}"#;

fn world() -> World {
    let mut w = World::new();
    w.stored_fn(TEMP, &[("region", STR), ("station", STR)]);
    let members = |c: &datumesh_core::Cell, i: &mut dyn datumesh_engine::Inputs| {
        let region = c.args["region"].scalar().unwrap().clone();
        i.read_slice(TEMP, &pattern(&[("region", bound(region)), ("station", open())]))
    };
    w.native_fn(TOTAL, &[TEMP], &[("region", STR)], move |c, i, _| {
        Ok(sum(members(c, i)?.iter().map(|m| &m.input.value)))
    });
    w.native_fn(COUNT, &[TEMP], &[("region", STR)], move |c, i, _| Ok(num(&members(c, i)?.len().to_string())));
    w.native_fn(MEAN, &[TEMP], &[("region", STR)], move |c, i, _| {
        let ms = members(c, i)?;
        if ms.is_empty() {
            return Err(EvalError::Fault(Fault::Empty));
        }
        let total = int(&sum(ms.iter().map(|m| &m.input.value)));
        Ok(num(&(total / ms.len() as i128).to_string()))
    });
    for (s, v) in [("a", "10"), ("b", "20"), ("c", "30")] {
        w.publish(&w.cell(TEMP, &[("region", text("north")), ("station", text(s))]), v, &t(10, 0));
    }
    w
}

fn temp(w: &World, region: &str, station: &str) -> CellRef {
    w.cellref(TEMP, &[("region", text(region)), ("station", text(station))])
}

fn north(w: &World) -> CellRef {
    let mut c = w.cellref(TEMP, &[("region", text("north"))]);
    c.args.insert("station".into(), Arg::Slice);
    c
}

fn ask(e: &Engine, w: &World, f: &str, o: Overrides) -> Result<ScenarioAnswer, EngineError> {
    e.whatif(&w.cellref(f, &[("region", text("north"))]), None, &o, None)
}

fn code(r: Result<ScenarioAnswer, EngineError>) -> String {
    match r {
        Err(EngineError::Override(c)) => c,
        other => panic!("expected an override error, got {other:?}"),
    }
}

#[test]
fn a_members_override_replaces_the_whole_member_set() {
    let w = world();
    let e = w.engine();
    // Two members, one of them stored (a) and one never published (d): exactly those.
    let o = Overrides::new()
        .slice_members(north(&w), vec![(temp(&w, "north", "d"), num("100")), (temp(&w, "north", "a"), num("1"))]);
    let s = ask(&e, &w, TOTAL, o.clone()).unwrap();
    assert_eq!(s.value, num("101"), "their sum: the stored b and c are not members");
    assert_eq!(ask(&e, &w, COUNT, o.clone()).unwrap().value, num("2"));
    assert_eq!(ask(&e, &w, MEAN, o).unwrap().value, num("50"));
    // The echo: members sorted by cell id.
    match &s.overrides[..] {
        [ScenarioOverride::Members { slice, members }] => {
            assert_eq!(slice.args["station"], Arg::Slice);
            let ids: Vec<String> = members.iter().map(|(c, _)| c.id()).collect();
            assert_eq!(ids.len(), 2);
            assert!(ids.windows(2).all(|p| p[0] < p[1]), "{ids:?}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_empty_members_override_gives_an_empty_slice() {
    let w = world();
    let e = w.engine();
    let o = Overrides::new().slice_members(north(&w), vec![]);
    assert_eq!(ask(&e, &w, TOTAL, o.clone()).unwrap().value, num("0"));
    assert_eq!(ask(&e, &w, COUNT, o.clone()).unwrap().value, num("0"));
    match ask(&e, &w, MEAN, o.clone()) {
        Err(EngineError::NotCovered { fault: Some(Fault::Empty) }) => {}
        other => panic!("E_FORMULA_EMPTY expected, got {other:?}"),
    }
    // The echo is present even when empty.
    match &ask(&e, &w, COUNT, o).unwrap().overrides[..] {
        [ScenarioOverride::Members { members, .. }] => assert!(members.is_empty()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn members_with_add_or_remove_is_refused() {
    let w = world();
    let e = w.engine();
    let one = || vec![(temp(&w, "north", "a"), num("1"))];
    let add = vec![(temp(&w, "north", "d"), num("5"))];
    let remove = vec![temp(&w, "north", "b")];
    for (a, r) in [(add.clone(), vec![]), (vec![], remove.clone()), (add, remove)] {
        let o = Overrides::new().slice_full(north(&w), Some(one()), a, r);
        assert_eq!(code(ask(&e, &w, TOTAL, o)), "E_SLICE_MEMBER");
    }
    // Some(empty members) with nothing else is allowed.
    let o = Overrides::new().slice_full(north(&w), Some(vec![]), vec![], vec![]);
    assert_eq!(ask(&e, &w, COUNT, o).unwrap().value, num("0"));
}

#[test]
fn a_member_that_is_not_a_cell_of_the_slice_is_refused() {
    let w = world();
    let e = w.engine();
    // Another region; another function.
    let o = Overrides::new().slice_members(north(&w), vec![(temp(&w, "south", "a"), num("1"))]);
    assert_eq!(code(ask(&e, &w, TOTAL, o)), "E_SLICE_MEMBER");
    let other = w.cellref(TOTAL, &[("region", text("north"))]);
    let o = Overrides::new().slice_members(north(&w), vec![(other, num("1"))]);
    assert_eq!(code(ask(&e, &w, TOTAL, o)), "E_SLICE_MEMBER");
    // The same member twice.
    let o = Overrides::new()
        .slice_members(north(&w), vec![(temp(&w, "north", "a"), num("1")), (temp(&w, "north", "a"), num("2"))]);
    assert_eq!(code(ask(&e, &w, TOTAL, o)), "E_SLICE_MEMBER");
}

#[test]
fn a_members_override_writes_nothing() {
    let w = world();
    let e = w.engine();
    let real = receipt_of(&e.get(&w.cellref(TOTAL, &[("region", text("north"))]), None).unwrap());
    assert_eq!(real.value(), &num("60"));
    let rows = (w.db().count("versions"), w.db().count("computed_versions"), w.db().count("receipts"));
    let o = Overrides::new().slice_members(north(&w), vec![(temp(&w, "north", "d"), num("100"))]);
    assert_eq!(ask(&e, &w, TOTAL, o).unwrap().value, num("100"));
    let _ = ask(&e, &w, TOTAL, Overrides::new().slice_members(north(&w), vec![(temp(&w, "south", "x"), num("1"))]));
    assert_eq!((w.db().count("versions"), w.db().count("computed_versions"), w.db().count("receipts")), rows);
    // The real answer is unchanged.
    assert_eq!(receipt_of(&e.get(&w.cellref(TOTAL, &[("region", text("north"))]), None).unwrap()).hash(), real.hash());
}
