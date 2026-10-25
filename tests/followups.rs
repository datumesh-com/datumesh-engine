//! The 1.3 follow-ups (0.3.1): a computed slice's members evaluated in key order up to the
//! first without a value; BudgetExceeded before WorkExceeded; E_FORMULA_FUTURE kept when an
//! input has no value; Inputs::binding_peek and Inputs::asof_compare; FnBinding::new.

mod common;

use std::cmp::Ordering as Cmp;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use datumesh_core::{Fault, FunctionDescriptor, Resolver};
use datumesh_engine::{EngineError, EvalError, FnBinding};

const HALF: &str = "calc.example.net/half";
const EXT: &str = r#"{"extent":["1","2","3","4"],"type":"number","unit":"1","valid":false}"#;

#[test]
fn a_computed_slice_stops_at_its_first_member_without_a_value() {
    // half(n) over [1, 2, 3, 4] faults at 2. A reader catching the absence: members 3 and 4
    // are never evaluated (SPEC 15.13, the vector formula-eval/member-fault-caught).
    let mut w = World::new();
    let late = Arc::new(AtomicUsize::new(0));
    let l = late.clone();
    w.native_fn(HALF, &[], &[("n", EXT)], move |c, _, _| match int(c.args["n"].scalar().unwrap()) {
        2 => Err(EvalError::Fault(Fault::DivZero)),
        n => {
            if n > 2 {
                l.fetch_add(1, Ordering::SeqCst);
            }
            Ok(num(&n.to_string()))
        }
    });
    let used = Arc::new(std::sync::Mutex::new(Vec::new()));
    let u = used.clone();
    w.native_fn("calc.example.net/caught", &[HALF], &[], move |_, i, b| {
        let v = match i.read_slice(HALF, &pattern(&[("n", open())])) {
            Ok(ms) => sum(ms.iter().map(|m| &m.input.value)),
            Err(EvalError::Fault(Fault::Absent)) => num("0"),
            Err(e) => return Err(e),
        };
        u.lock().unwrap().push(b.used());
        Ok(v)
    });
    let e = w.engine();
    let r = receipt_of(&e.get(&w.cellref("calc.example.net/caught", &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num("0"));
    assert_eq!(late.load(Ordering::SeqCst), 0, "no member after the faulting one is evaluated");
    assert_eq!(w.calls(HALF), 2, "members 1 and 2");
    // The reader's own fuel: the native call; the read that failed is not charged.
    assert_eq!(*used.lock().unwrap(), [1]);
    // Members 3 and 4 asked for by themselves still have values.
    assert_eq!(receipt_of(&e.get(&w.cellref(HALF, &[("n", num("4"))]), Some(&t(20, 0))).unwrap()).value(), &num("4"));
}

#[test]
fn budget_exceeded_wins_over_work_exceeded() {
    let mut w = World::new();
    w.native_fn("calc.example.net/big", &[], &[], |_, _, b| {
        b.charge(1_000)?;
        Ok(num("1"))
    });
    let e = w.engine_with(|b| b.fuel(10).work_limit(10));
    match e.get(&w.cellref("calc.example.net/big", &[]), Some(&t(20, 0))) {
        Err(EngineError::BudgetExceeded { fuel: 10 }) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn future_is_kept_when_an_input_has_no_value() {
    // The times come before the cell's arguments (SPEC 15.13): an argument that reads a cell
    // with no value, then T found after the asof, is E_FORMULA_FUTURE, not plain not_covered.
    let mut w = World::new();
    const S: &str = "rates.example.org/s";
    w.stored_fn(S, &[]);
    w.native_fn("calc.example.net/later", &[S], &[], |_, i, _| {
        let t = t(25, 0);
        match i.read(S, &args(&[])) {
            Err(EvalError::InputNotCovered { .. }) if i.asof_compare(&t)? == Cmp::Greater => {
                Err(EvalError::Fault(Fault::Future))
            }
            other => other,
        }
    });
    w.native_fn("calc.example.net/sooner", &[S], &[], |_, i, _| {
        let t = t(15, 0);
        match i.read(S, &args(&[])) {
            Err(EvalError::InputNotCovered { .. }) if i.asof_compare(&t)? == Cmp::Greater => {
                Err(EvalError::Fault(Fault::Future))
            }
            other => other,
        }
    });
    // A runtime cannot turn an absence into another fault it did not establish.
    w.native_fn("calc.example.net/claims", &[S], &[], |_, i, _| match i.read(S, &args(&[])) {
        Err(EvalError::InputNotCovered { .. }) => Err(EvalError::Fault(Fault::Future)),
        other => other,
    });
    let e = w.engine();
    assert_eq!(e.get(&w.cellref("calc.example.net/later", &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::Future));
    assert_eq!(e.get(&w.cellref("calc.example.net/sooner", &[]), Some(&t(20, 0))).unwrap(), datumesh_engine::Answer::NOT_COVERED);
    assert_eq!(e.get(&w.cellref("calc.example.net/claims", &[]), Some(&t(20, 0))).unwrap(), datumesh_engine::Answer::NOT_COVERED);
    // read_asof's own FUTURE after an absence is kept too.
    w.native_fn("calc.example.net/at", &[S], &[], |_, i, _| {
        let missing = i.read(S, &args(&[]));
        match i.read_asof(S, &args(&[]), &t(25, 0)) {
            Err(EvalError::Fault(Fault::Future)) => Err(EvalError::Fault(Fault::Future)),
            _ => missing,
        }
    });
    assert_eq!(w.engine().get(&w.cellref("calc.example.net/at", &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::Future));
}

#[test]
fn binding_peek_relies_on_nothing() {
    // late is bound on the 18th, after the input's asof. peek() looks its binding up (and
    // reads s); read() reads late. Only the read makes the binding's start the asof.
    let mut w = World::new();
    const S: &str = "rates.example.org/s";
    const LATE: &str = "rates.example.org/late";
    w.stored_fn(S, &[]);
    let h = w.store.put_descriptor(&descriptor("stored", "", &[], &[])).unwrap();
    w.store.bind_name(LATE, &h, &t(18, 0)).unwrap();
    w.native_fn("calc.example.net/peek", &[S, LATE], &[], |_, i, _| {
        let b = i.binding_peek(LATE, None)?.expect("bound");
        assert_eq!(b.start, t(18, 0));
        assert!(i.binding_peek(LATE, Some(&t(17, 0)))?.is_none());
        i.read(S, &args(&[]))
    });
    w.native_fn("calc.example.net/relied", &[S, LATE], &[], |_, i, _| {
        i.binding(LATE, None)?.expect("bound");
        i.read(S, &args(&[]))
    });
    w.publish(&w.cell(S, &[]), "5", &t(10, 0));
    let e = w.engine();
    let p = receipt_of(&e.get(&w.cellref("calc.example.net/peek", &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(p.asof(), t(10, 0), "a peeked binding is no asof candidate");
    let r = receipt_of(&e.get(&w.cellref("calc.example.net/relied", &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(r.asof(), t(18, 0), "a binding relied on is");
    // Rebinding late later does not drop the peek's answer: it does not depend on it.
    let h2 = w.store.put_descriptor(&descriptor("stored", "v2", &[], &[])).unwrap();
    w.store.bind_name(LATE, &h2, &t(21, 0)).unwrap();
    w.reset_calls();
    assert_eq!(receipt_of(&e.get(&w.cellref("calc.example.net/peek", &[]), Some(&t(22, 0))).unwrap()).hash(), p.hash());
    assert_eq!(w.calls("calc.example.net/peek"), 0);
    w.verify_all_issued(&e, "followups_peek");
}

#[test]
fn asof_compare_records_the_time_not_the_asof() {
    // since() = s + 1 when the 15th is at or before the asof, else s. Asked after the 15th:
    // the comparison holds for every later asof, so the receipt's asof is the 15th (a
    // candidate), one receipt for two asofs, and it recomputes there. Asked before: the value
    // would change at the 15th, so it answers at its asof only (asof-dependent).
    let mut w = World::new();
    const S: &str = "rates.example.org/s";
    w.stored_fn(S, &[]);
    w.native_fn("calc.example.net/since", &[S], &[], |_, i, _| {
        let s = i.read(S, &args(&[]))?;
        Ok(if i.asof_compare(&t(15, 0))? != Cmp::Greater { add(&s, &num("1")) } else { s })
    });
    w.publish(&w.cell(S, &[]), "5", &t(10, 0));
    let e = w.engine();
    let r = w.cellref("calc.example.net/since", &[]);
    let a = receipt_of(&e.get(&r, Some(&t(20, 0))).unwrap());
    let b = receipt_of(&w.engine().get(&r, Some(&t(22, 0))).unwrap());
    assert_eq!((a.value(), a.asof()), (&num("6"), t(15, 0).as_str()));
    assert_eq!(b.hash(), a.hash(), "one receipt");
    assert_eq!(e.evaluate(a.cell(), a.asof()).as_ref(), Some(a.value()));
    let early = receipt_of(&e.get(&r, Some(&t(12, 0))).unwrap());
    assert_eq!((early.value(), early.asof()), (&num("5"), t(12, 0).as_str()));
    let early2 = receipt_of(&e.get(&r, Some(&t(13, 0))).unwrap());
    assert_eq!(early2.asof(), t(13, 0), "before the time: one receipt per asof");
    w.verify_all_issued(&e, "followups_asof_compare");
}

#[test]
fn fn_binding_has_a_constructor() {
    let d = FunctionDescriptor::from_json(&descriptor("stored", "", &[], &[])).unwrap();
    let b = FnBinding::new(d.hash(), BIND_TX, Arc::new(d.clone()));
    assert_eq!((b.fn_hash.as_str(), b.start.as_str()), (d.hash().as_str(), BIND_TX));
    assert_eq!(*b.descriptor, d);
}
