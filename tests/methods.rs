//! Methods are ordinary functions (SPEC 16.3, engine 0.4.2). `bind_class` binds each method
//! `N/m` of a class to its function hash; a cell of `N/m` read through that name is evaluated
//! and memoised as any function's. And a default that depends on the asof (16.7) gives one
//! cell per asof.

mod common;

use std::sync::atomic::Ordering;

use common::*;
use datumesh_core::{ClassEnv, FunctionDescriptor};

const S: &str = "rates.example.org/s";
const CLASS: &str = "calc.example.net/fx_pair";
const RATE: &str = "calc.example.net/fx_pair/rate";
const USER: &str = "calc.example.net/user";
const T_BIND: &str = "2024-03-12T00:00:00.000000Z";

fn key_decl() -> &'static str {
    r#"{"key_normalisation":"upper_ascii","type":"string","valid":false}"#
}

/// A descriptor of a method: the keys `base` and `quote`, returning a number.
fn method_descriptor(body: &str, inputs: &[&str]) -> Vec<u8> {
    let inputs: Vec<String> = inputs.iter().map(|i| format!(r#""{i}""#)).collect();
    let key = key_decl();
    let text = format!(
        r#"{{"code":{{"body":"{body}","lang":"native_test"}},"inputs":[{}],"signature":{{"args":{{"base":{key},"quote":{key}}},"order":["base","quote"],"returns":{{"type":"number","unit":"1"}}}}}}"#,
        inputs.join(",")
    );
    FunctionDescriptor::from_json(text.as_bytes()).unwrap().canonical()
}

/// The class `fx_pair` (keys base, quote; method rate), defined by the core, put in the store
/// and bound at T_BIND with its method `rate` implemented by a native closure that reads the
/// stored s and adds the length of the base. Returns the method's function hash.
fn bind_fx_pair(w: &World) -> String {
    let rate = method_descriptor("rate", &[S]);
    let rate_hash = w.store.put_descriptor(&rate).unwrap();
    let counter = w.calls.lock().unwrap().entry(RATE.to_string()).or_default().clone();
    w.native.write().unwrap().register(rate_hash.clone(), move |c, i, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        let base = c.args["base"].scalar().unwrap().lexical().len() as i128;
        Ok(num(&(int(&i.read(S, &args(&[]))?) + base).to_string()))
    });
    let text = format!(
        r#"{{"steward":{{"process":"https://{CALC}/classes/change","namespace":"{CALC}"}},"order":["base","quote"],"methods":{{"rate":"{rate_hash}"}},"keys":{{"base":{{"type":"string","key_normalisation":"upper_ascii"}},"quote":{{"type":"string","key_normalisation":"upper_ascii"}}}}}}"#
    );
    let mut env = ClassEnv::new(CALC);
    env.insert_function(FunctionDescriptor::from_json(&rate).unwrap());
    let flat = datumesh_core::define_class(CLASS, text.as_bytes(), &env).unwrap();
    let class_hash = w.store.put_class(&flat.canonical()).unwrap();
    w.store.bind_class(CLASS, &class_hash, T_BIND).unwrap();
    rate_hash
}

fn pair() -> datumesh_core::Args {
    args(&[("base", text("EUR")), ("quote", text("USD"))])
}

#[test]
fn a_method_cell_read_through_the_class_binding_is_evaluated_and_memoised_as_any_function() {
    let mut w = World::new();
    w.stored_fn(S, &[]);
    let rate_hash = bind_fx_pair(&w);
    w.native_fn(USER, &[RATE], &[], |_, i, _| Ok(add(&i.read(RATE, &pair())?, &num("1000"))));
    let e = w.engine();
    w.publish(&w.cell(S, &[]), "100", &t(10, 0));

    // The name N/m is bound as a function, to the method's hash, by bind_class.
    assert_eq!(w.store.resolve_name(RATE, &t(20, 0)).unwrap().as_deref(), Some(rate_hash.as_str()));

    // Directly: a computed cell of the method, signed by the mesh, its input the stored s.
    let r = receipt_of(
        &e.get(&w.cellref(RATE, &[("base", text("EUR")), ("quote", text("USD"))]), Some(&t(20, 0))).unwrap(),
    );
    assert_eq!(r.value(), &num("103"));
    assert_eq!(r.cell().fn_hash, rate_hash);
    assert_eq!((r.cell().ns.as_str(), r.cell().fn_name.as_str()), (CLASS, "rate"));
    assert_eq!(r.signer_host(), MESH);
    assert_eq!(r.inputs().len(), 1);
    assert_eq!(w.calls(RATE), 1);

    // Memoised: asked again, and read by another function, it is not evaluated again.
    let eur_usd = w.cellref(RATE, &[("base", text("EUR")), ("quote", text("USD"))]);
    assert_eq!(bytes_of(&e.get(&eur_usd, Some(&t(20, 0))).unwrap()), receipt_json(&r));
    let u = receipt_of(&e.get(&w.cellref(USER, &[]), Some(&t(20, 0))).unwrap());
    assert_eq!(u.value(), &num("1103"));
    assert!(u.inputs().contains(&r.hash().to_string()), "the method's receipt is the reader's input");
    assert_eq!(w.calls(RATE), 1, "evaluated once");

    // A cell of another key is another cell; the input's change is followed.
    let usd = w.cellref(RATE, &[("base", text("USD")), ("quote", text("EUR"))]);
    assert_eq!(receipt_of(&e.get(&usd, Some(&t(20, 0))).unwrap()).value(), &num("103"));
    w.publish(&w.cell(S, &[]), "200", &t(21, 0));
    assert_eq!(receipt_of(&e.get(&w.cellref(USER, &[]), Some(&t(22, 0))).unwrap()).value(), &num("1203"));
    // And as of before, still the old value.
    assert_eq!(receipt_of(&e.get(&w.cellref(USER, &[]), Some(&t(20, 0))).unwrap()).value(), &num("1103"));
    w.verify_all_issued(&e, "methods");
}

const DAILY: &str = "calc.example.net/daily";
const DEFAULTED: &str = "calc.example.net/defaulted";

#[test]
fn a_default_that_depends_on_the_asof_gives_one_cell_per_asof() {
    // defaulted() = daily(day = the day of the asof): the default of `day` is asof()'s day,
    // so the function is asof-dependent (16.7).
    let mut w = World::new();
    let day_arg = r#"{"type":"number","unit":"1","valid":false}"#;
    w.native_fn(DAILY, &[], &[("day", day_arg)], |c, _, _| {
        Ok(num(&(int(c.args["day"].scalar().unwrap()) * 2).to_string()))
    });
    let h = w.native_fn(DEFAULTED, &[DAILY], &[], |_, i, _| {
        let asof = i.asof();
        let day: i128 = asof[8..10].parse().unwrap();
        i.read(DAILY, &args(&[("day", num(&day.to_string()))]))
    });
    w.native.write().unwrap().set_asof_dependent(h);
    let e = w.engine();
    let c = w.cellref(DEFAULTED, &[]);

    let a = receipt_of(&e.get(&c, Some(&t(20, 0))).unwrap());
    let b = receipt_of(&e.get(&c, Some(&t(21, 0))).unwrap());
    assert_eq!((a.value(), b.value()), (&num("40"), &num("42")));
    assert_ne!(a.hash(), b.hash(), "two cells' receipts, one per asof");
    assert_eq!((a.asof(), b.asof()), (t(20, 0).as_str(), t(21, 0).as_str()));
    assert_ne!(a.inputs(), b.inputs(), "each read the daily cell of its own day");
    // Each asof is answered again from its own, not the other's.
    assert_eq!(receipt_of(&e.get(&c, Some(&t(20, 0))).unwrap()).hash(), a.hash());
    assert_eq!(receipt_of(&e.get(&c, Some(&t(21, 0))).unwrap()).hash(), b.hash());
    w.verify_all_issued(&e, "methods_default_asof");
}
