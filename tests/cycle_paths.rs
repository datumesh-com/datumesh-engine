//! A cycle names its path (Build 2 Part 3, E3; SPEC 5.5 `cycle`). Tests first.
//!
//! API specified: `EngineError::Cycle { path: Vec<String> }` replaces `Cycle { cell_id }`.
//! `path` holds canonical calls (SPEC 5.8, `<ns>/<fn>(<label>: <literal>, ...)`: arguments
//! labelled in ascending order, strings quoted, numbers/dates bare; no arguments: `()`), in
//! read order: the cell that reappeared, the reads between, the cell again. So the first and
//! the last are equal, and the requested root is in the path only when it is on the cycle.
//! The fixes (`asof`, `lag`, `step`, `solver`) are the http layer's: the path carries the
//! argument types they depend on (a number argument `x: 1`, a date `d: 2024-03-10`).
//!
//! NOTE for the implementer: tests/recursion.rs (`a_cell_reappearing_on_the_stack_is_a_cycle`)
//! matches `Cycle { cell_id }`; with the variant changed it must be updated to `path`.

mod common;

use common::*;
use datumesh_core::Scalar;
use datumesh_engine::EngineError;

const N: &str = r#"{"type":"number","unit":"1","valid":false}"#;
const D: &str = r#"{"type":"date","valid":false}"#;
const S: &str = r#"{"type":"string","valid":false}"#;
const CALC_: &str = "calc.example.net";

fn path_of(r: Result<datumesh_engine::Answer, EngineError>) -> Vec<String> {
    match r {
        Err(EngineError::Cycle { path, .. }) => path,
        other => panic!("expected a cycle, got {other:?}"),
    }
}

fn one_arg(w: &mut World, name: &str, to: &'static str, decl: &'static str, arg: &'static str) {
    w.native_fn(name, &[to], &[(arg, decl)], move |c, i, _| {
        i.read(to, &args(&[(arg, c.args[arg].scalar().unwrap().clone())]))
    });
}

#[test]
fn a_direct_cycle_names_the_cell_twice() {
    let mut w = World::new();
    one_arg(&mut w, "calc.example.net/f", "calc.example.net/f", N, "x");
    let e = w.engine();
    let p = path_of(e.get(&w.cellref("calc.example.net/f", &[("x", num("1"))]), None));
    assert_eq!(p, vec!["calc.example.net/f(x: 1)", "calc.example.net/f(x: 1)"]);
    assert_eq!(w.db().count("receipts"), 0);
}

#[test]
fn a_mutual_cycle_names_the_reads_between_in_read_order() {
    let mut w = World::new();
    one_arg(&mut w, "calc.example.net/g", "calc.example.net/h", N, "x");
    one_arg(&mut w, "calc.example.net/h", "calc.example.net/g", N, "x");
    let e = w.engine();
    let p = path_of(e.get(&w.cellref("calc.example.net/g", &[("x", num("1"))]), None));
    assert_eq!(p, vec!["calc.example.net/g(x: 1)", "calc.example.net/h(x: 1)", "calc.example.net/g(x: 1)"]);
    let p = path_of(e.get(&w.cellref("calc.example.net/h", &[("x", num("2"))]), None));
    assert_eq!(p, vec!["calc.example.net/h(x: 2)", "calc.example.net/g(x: 2)", "calc.example.net/h(x: 2)"]);
}

#[test]
fn the_path_starts_at_the_cell_that_reappeared_not_at_the_root() {
    // root -> p -> q -> r -> q: the cycle is q, r, q; root and p are outside it.
    let mut w = World::new();
    one_arg(&mut w, "calc.example.net/root", "calc.example.net/p", N, "x");
    one_arg(&mut w, "calc.example.net/p", "calc.example.net/q", N, "x");
    one_arg(&mut w, "calc.example.net/q", "calc.example.net/r", N, "x");
    one_arg(&mut w, "calc.example.net/r", "calc.example.net/q", N, "x");
    let e = w.engine();
    let p = path_of(e.get(&w.cellref("calc.example.net/root", &[("x", num("7"))]), None));
    assert_eq!(p, vec!["calc.example.net/q(x: 7)", "calc.example.net/r(x: 7)", "calc.example.net/q(x: 7)"]);
}

#[test]
fn a_path_through_a_recursion_with_other_arguments_is_per_cell() {
    // k(n) reads k(n - 1) and, at n = 0, k(3): a cycle through k(3), k(2), k(1), k(0).
    let mut w = World::new();
    w.native_fn("calc.example.net/k", &["calc.example.net/k"], &[("n", N)], |c, i, _| {
        let n = int(c.args["n"].scalar().unwrap());
        let next = if n == 0 { 3 } else { n - 1 };
        i.read("calc.example.net/k", &args(&[("n", num(&next.to_string()))]))
    });
    let e = w.engine();
    let p = path_of(e.get(&w.cellref("calc.example.net/k", &[("n", num("3"))]), None));
    let want: Vec<String> = [3, 2, 1, 0, 3].iter().map(|n| format!("calc.example.net/k(n: {n})")).collect();
    assert_eq!(p, want);
}

#[test]
fn the_path_shows_the_argument_types_the_fixes_depend_on() {
    let mut w = World::new();
    // A date argument (the fix `lag`), a string argument (quoted), and two arguments in
    // ascending label order (the fix `step` for the number).
    one_arg(&mut w, "calc.example.net/dd", "calc.example.net/de", D, "d");
    one_arg(&mut w, "calc.example.net/de", "calc.example.net/dd", D, "d");
    one_arg(&mut w, "calc.example.net/ss", "calc.example.net/ss", S, "s");
    w.native_fn("calc.example.net/two", &["calc.example.net/two"], &[("z", N), ("a", D)], |c, i, _| {
        i.read(
            "calc.example.net/two",
            &args(&[("z", c.args["z"].scalar().unwrap().clone()), ("a", c.args["a"].scalar().unwrap().clone())]),
        )
    });
    let e = w.engine();
    let p = path_of(e.get(&w.cellref("calc.example.net/dd", &[("d", date("2024-03-10"))]), None));
    assert_eq!(p, vec!["calc.example.net/dd(d: 2024-03-10)", "calc.example.net/de(d: 2024-03-10)", "calc.example.net/dd(d: 2024-03-10)"]);
    let p = path_of(e.get(&w.cellref("calc.example.net/ss", &[("s", text("a\"b"))]), None));
    assert_eq!(p, vec![r#"calc.example.net/ss(s: "a""b")"#, r#"calc.example.net/ss(s: "a""b")"#]);
    let p = path_of(e.get(&w.cellref("calc.example.net/two", &[("z", num("1.5")), ("a", date("2024-03-10"))]), None));
    assert_eq!(p, vec!["calc.example.net/two(a: 2024-03-10, z: 1.5)"; 2]);
}

// ---- the feedback case: A rebound to read B as of T, before the rebind ----

const A: &str = "calc.example.net/a";
const B: &str = "calc.example.net/b";

/// Binds a native function to `name` at `at`; returns its hash.
fn bind_native(
    w: &World,
    name: &str,
    body: &str,
    inputs: &[&str],
    at: &str,
    f: impl Fn(&datumesh_core::Cell, &mut dyn datumesh_engine::Inputs, &mut datumesh_engine::Budget) -> Result<Scalar, datumesh_engine::EvalError>
        + Send
        + Sync
        + 'static,
) -> String {
    let d = descriptor("native_test", body, inputs, &[]);
    let h = w.store.put_descriptor(&d).unwrap();
    w.store.bind_name(name, &h, at).unwrap();
    w.native.write().unwrap().register(h.clone(), f);
    h
}

fn feedback_world(rebound_reads_now: bool) -> (World, String, String) {
    let mut w = World::new();
    // a = 10 from the start; b = a + 1, always.
    w.native_fn_body(A, "a0", &[], &[], |_, _, _| Ok(num("10")));
    w.native_fn(B, &[A], &[], |_, i, _| Ok(add(&i.read(A, &args(&[]))?, &num("1"))));
    let t_before = t(10, 0);
    let rebind = t(15, 0);
    if rebound_reads_now {
        // The naive form: a reads b now, b reads a.
        bind_native(&w, A, "a1-naive", &[B], &rebind, |_, i, _| Ok(add(&i.read(B, &args(&[]))?, &num("5"))));
    } else {
        // a reads b as of T (before the rebind): b then read the old a.
        let at = t_before.clone();
        bind_native(&w, A, "a1-asof", &[B], &rebind, move |_, i, _| Ok(add(&i.read_asof(B, &args(&[]), &at)?.value, &num("5"))));
    }
    (w, t_before, rebind)
}

#[test]
fn a_rebind_reading_b_as_of_before_the_rebind_is_not_a_cycle() {
    let (w, t_before, _rebind) = feedback_world(false);
    let e = w.engine();
    let later = t(20, 0);
    // Before the rebind: the old a.
    assert_eq!(receipt_of(&e.get(&w.cellref(A, &[]), Some(&t_before)).unwrap()).value(), &num("10"));
    // After it: b as of T (a was 10, so b was 11), plus 5.
    let a = receipt_of(&e.get(&w.cellref(A, &[]), Some(&later)).unwrap());
    assert_eq!(a.value(), &num("16"));
    // b as of T is in the lineage: its receipt, the one get gives as of T.
    let b_at_t = receipt_of(&e.get(&w.cellref(B, &[]), Some(&t_before)).unwrap());
    assert_eq!(b_at_t.value(), &num("11"));
    assert!(a.inputs().iter().any(|h| h == b_at_t.hash()), "{:?} lacks {}", a.inputs(), b_at_t.hash());
    // And b now, which reads the new a, is fine too: 16 + 1.
    assert_eq!(receipt_of(&e.get(&w.cellref(B, &[]), Some(&later)).unwrap()).value(), &num("17"));
    // A fresh engine (nothing memoised) gives the same receipt for a.
    let fresh = w.engine();
    assert_eq!(receipt_of(&fresh.get(&w.cellref(A, &[]), Some(&later)).unwrap()).hash(), a.hash());
    w.verify_all_issued(&e, "cycle_feedback");
}

#[test]
fn the_naive_rebind_reading_b_now_is_a_cycle_a_b_a() {
    let (w, t_before, _) = feedback_world(true);
    let e = w.engine();
    let p = path_of(e.get(&w.cellref(A, &[]), Some(&t(20, 0))));
    assert_eq!(p, vec![format!("{CALC_}/a()"), format!("{CALC_}/b()"), format!("{CALC_}/a()")]);
    // Starting from b, the cycle is b, a, b.
    let p = path_of(e.get(&w.cellref(B, &[]), Some(&t(20, 0))));
    assert_eq!(p, vec![format!("{CALC_}/b()"), format!("{CALC_}/a()"), format!("{CALC_}/b()")]);
    // Before the rebind there is no cycle.
    assert_eq!(receipt_of(&e.get(&w.cellref(A, &[]), Some(&t_before)).unwrap()).value(), &num("10"));
}
