//! Faults are memoised (SPEC 15.12, formula finding H9; engine 0.4.2). `E_FORMULA_KEY` and
//! `E_FORMULA_DISPATCH_KEY` (spec-0.5.0) are answered as every fault: `not_covered` with the
//! `fault`, deterministic, an answer and never an error to retry. A faulted cell is evaluated
//! once: asked again, or read by another function, the engine does not run the function again
//! (while nothing it read has changed).

mod common;

use common::*;
use datumesh_core::{Fault, Resolver};
use datumesh_engine::{Answer, EvalError};

const S: &str = "rates.example.org/s";
const INV: &str = "calc.example.net/inverse";
const READER: &str = "calc.example.net/reader";
const KEY: &str = "calc.example.net/key_fault";
const DISPATCH: &str = "calc.example.net/dispatch_key_fault";
const FAULT: &str = "calc.example.net/fault";
const KEY_READER: &str = "calc.example.net/key_reader";
const CODE: &str = r#"{"type":"string","valid":false}"#;

fn world() -> World {
    let mut w = World::new();
    w.stored_fn(S, &[]);
    w.native_fn(INV, &[S], &[], |_, i, _| {
        let s = int(&i.read(S, &args(&[]))?);
        if s == 0 {
            return Err(EvalError::Fault(Fault::DivZero));
        }
        Ok(num(&(1000 / s).to_string()))
    });
    w.native_fn(READER, &[INV], &[], |_, i, _| Ok(add(&i.read(INV, &args(&[]))?, &num("1"))));
    w.native_fn(KEY, &[], &[], |_, _, _| Err(EvalError::Fault(Fault::Key)));
    w.native_fn(DISPATCH, &[], &[], |_, _, _| Err(EvalError::Fault(Fault::DispatchKey)));
    w.native_fn(KEY_READER, &[KEY], &[], |_, i, _| i.read(KEY, &args(&[])));
    w.native_fn(FAULT, &[], &[("code", CODE)], |c, _, _| {
        let code = c.args["code"].scalar().unwrap().lexical().to_string();
        Err(EvalError::Fault(Fault::from_code(&code).unwrap()))
    });
    w
}

#[test]
fn the_key_and_dispatch_key_faults_are_not_covered_with_their_codes() {
    let w = world();
    let e = w.engine();
    let before = rows(&w);
    for (name, fault, code) in
        [(KEY, Fault::Key, "E_FORMULA_KEY"), (DISPATCH, Fault::DispatchKey, "E_FORMULA_DISPATCH_KEY")]
    {
        assert_eq!(fault.as_str(), code);
        let c = w.cellref(name, &[]);
        // An answer, not an error; the same each time, at any asof.
        assert_eq!(e.get(&c, Some(&t(20, 0))).unwrap(), faulted(fault), "{name}");
        assert_eq!(e.get(&c, Some(&t(20, 0))).unwrap(), faulted(fault), "{name} again");
        assert_eq!(e.get(&c, Some(&t(25, 0))).unwrap(), Answer::NotCovered { fault: Some(fault) });
        assert_eq!(e.evaluate(&w.cell(name, &[]), &t(20, 0)), None);
        // Another engine on the same store says the same.
        assert_eq!(w.engine().get(&c, Some(&t(20, 0))).unwrap(), faulted(fault));
    }
    // A reader of a key fault has it, uncaught, as its own.
    assert_eq!(e.get(&w.cellref(KEY_READER, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::Key));
    assert_eq!(rows(&w), before, "a fault stores nothing");
}

#[test]
fn the_key_and_dispatch_key_faults_are_memoised() {
    let w = world();
    let e = w.engine();
    for (name, fault) in [(KEY, Fault::Key), (DISPATCH, Fault::DispatchKey)] {
        let c = w.cellref(name, &[]);
        for _ in 0..3 {
            assert_eq!(e.get(&c, Some(&t(20, 0))).unwrap(), faulted(fault));
        }
        assert_eq!(w.calls(name), 1, "{name} was evaluated once");
    }
    // Read by another function: the fault is already known.
    assert_eq!(e.get(&w.cellref(KEY_READER, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::Key));
    assert_eq!(w.calls(KEY), 1);
    assert_eq!(w.calls(KEY_READER), 1);
    assert_eq!(e.get(&w.cellref(KEY_READER, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::Key));
    assert_eq!((w.calls(KEY), w.calls(KEY_READER)), (1, 1));
}

#[test]
fn every_fault_is_memoised_the_old_ones_too() {
    let w = world();
    let e = w.engine();
    for f in Fault::ALL {
        w.reset_calls();
        let r = w.cellref(FAULT, &[("code", text(f.as_str()))]);
        for _ in 0..3 {
            assert_eq!(e.get(&r, Some(&t(20, 0))).unwrap(), Answer::NotCovered { fault: Some(*f) }, "{f}");
        }
        assert_eq!(w.calls(FAULT), 1, "{f}: evaluated once, not once per request");
    }
}

#[test]
fn a_faulted_input_is_not_evaluated_again_by_its_readers_and_the_fault_follows_its_inputs() {
    let w = world();
    let e = w.engine();
    let s = w.cell(S, &[]);
    w.publish(&s, "0", &t(10, 0));
    // The reader faults with the input's code; the input ran once for both requests.
    assert_eq!(e.get(&w.cellref(READER, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    assert_eq!(e.get(&w.cellref(INV, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    assert_eq!(e.get(&w.cellref(READER, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    // READER runs twice for its first request: it suspends on the uncomputed input and runs again
    // once the input is answered (a restart, as budget.rs counts them); never again after that.
    assert_eq!((w.calls(INV), w.calls(READER)), (1, 2));
    // Once s changes, a later asof is not answered by the memoised fault.
    w.publish(&s, "4", &t(21, 0));
    assert_eq!(receipt_of(&e.get(&w.cellref(INV, &[]), Some(&t(22, 0))).unwrap()).value(), &num("250"));
    assert_eq!(receipt_of(&e.get(&w.cellref(READER, &[]), Some(&t(22, 0))).unwrap()).value(), &num("251"));
    // As of before, still the fault.
    assert_eq!(e.get(&w.cellref(READER, &[]), Some(&t(20, 0))).unwrap(), faulted(Fault::DivZero));
    w.verify_all_issued(&e, "fault_memo");
}
