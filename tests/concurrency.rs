//! Concurrency: two threads getting the same uncomputed cell evaluate it once and store one
//! version; under a concurrent stream of new stored versions, the memo (fed by the dirty
//! set) and validation agree: every answer has the value a fresh engine with no memo gives,
//! and the history says, and a receipt that answers as of the asof and recomputes there.
//!
//! Not the same receipt in every case: when a cell's value changes and changes back
//! (2 at 07:00, 3 at 09:00, 2 at 11:00), the version of 07:00 validates at 11:00 (its inputs
//! give the same values again, early cutoff), so whether 11:00 is answered with it or with
//! a version of 11:00 depends on which versions earlier requests happened to store. Both
//! state the cell's value as of 11:00 correctly.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use common::*;
use datumesh_core::Scalar;
use datumesh_engine::{Answer, Engine};

const S1: &str = "rates.example.org/s1";
const S2: &str = "rates.example.org/s2";
const CHILD: &str = "calc.example.net/child";
const TOP: &str = "calc.example.net/top";

#[test]
fn the_engine_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<Engine>();
}

#[test]
fn two_threads_getting_the_same_uncomputed_cell_evaluate_it_once() {
    let mut w = World::new();
    w.stored_fn(S1, &[]);
    w.native_fn("calc.example.net/slow", &[S1], &[], |_, i, _| {
        let v = i.read(S1, &args(&[]))?;
        std::thread::sleep(Duration::from_millis(300));
        Ok(add(&v, &num("1")))
    });
    w.publish(&w.cell(S1, &[]), "1", &t(10, 0));
    let e = w.engine();
    let r = w.cellref("calc.example.net/slow", &[]);
    for asof in [Some(t(20, 0)), None] {
        let start = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (e, r, start, asof) = (e.clone(), r.clone(), start.clone(), asof.clone());
                std::thread::spawn(move || {
                    start.wait();
                    e.get(&r, asof.as_deref()).unwrap()
                })
            })
            .collect();
        let answers: Vec<Answer> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(answers[0], answers[1]);
    }
    assert_eq!(w.calls("calc.example.net/slow"), 1, "one evaluation");
    assert_eq!(w.db().count("computed_versions"), 1, "one stored version");
    w.verify_all_issued(&e, "concurrency_single_flight");
}

/// child = 1 if s1 >= 10 else 0; top = child + s2.
fn graph() -> World {
    let mut w = World::new();
    w.stored_fn(S1, &[]);
    w.stored_fn(S2, &[]);
    w.native_fn(CHILD, &[S1], &[], |_, i, _| Ok(num(if int(&i.read(S1, &args(&[]))?) >= 10 { "1" } else { "0" })));
    w.native_fn(TOP, &[CHILD, S2], &[], |_, i, _| Ok(add(&i.read(CHILD, &args(&[]))?, &i.read(S2, &args(&[]))?)));
    w
}

/// The versions published: (tx, cell, value).
type History = Vec<(String, &'static str, i128)>;

/// The value of top as of `asof` from the history.
fn oracle(history: &[(String, &'static str, i128)], asof: &str) -> Option<i128> {
    let latest = |c: &str| {
        history.iter().filter(|(tx, cell, _)| *cell == c && tx.as_str() <= asof).map(|(_, _, v)| *v).next_back()
    };
    let (s1, s2) = (latest(S1)?, latest(S2)?);
    Some(i128::from(s1 >= 10) + s2)
}

#[test]
fn memo_and_validation_agree_under_a_stream_of_new_versions() {
    let w = Arc::new(graph());
    let e = w.engine();
    let s1 = w.cell(S1, &[]);
    let s2 = w.cell(S2, &[]);
    let top = w.cellref(TOP, &[]);
    // Each step publishes one version, at the next hour; s1 moves within and across 10, so
    // some steps change the child and some do not (early cutoff).
    let steps: Vec<(&'static str, i128)> = (0..40)
        .map(|k| match k % 4 {
            0 => (S1, [3, 12, 15, 7, 20][k % 5]),
            1 => (S2, k as i128),
            2 => (S1, [11, 4, 30, 2][k % 4 / 2 + (k / 4) % 3]),
            _ => (S2, (k as i128) % 3),
        })
        .collect();
    let history: Arc<Mutex<History>> = Arc::new(Mutex::new(Vec::new()));
    let watermark = Arc::new(Mutex::new(String::new()));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tx_of = |k: usize| format!("2024-04-{:02}T{:02}:00:00.000000Z", 1 + k / 24, k % 24);
    let readers: Vec<_> = (0..4)
        .map(|n| {
            let (e, top, history, watermark, done) =
                (e.clone(), top.clone(), history.clone(), watermark.clone(), done.clone());
            std::thread::spawn(move || {
                let mut seen: Vec<(String, Answer)> = Vec::new();
                let mut i = n;
                while !done.load(std::sync::atomic::Ordering::SeqCst) || seen.len() < 20 {
                    let w = watermark.lock().unwrap().clone();
                    if w.is_empty() {
                        std::thread::yield_now();
                        continue;
                    }
                    // The latest published time, or an earlier step's.
                    let asof = {
                        let h = history.lock().unwrap();
                        if i % 3 == 0 {
                            w.clone()
                        } else {
                            h[i % h.len()].0.clone()
                        }
                    };
                    i += 7;
                    seen.push((asof.clone(), e.get(&top, Some(&asof)).unwrap()));
                }
                seen
            })
        })
        .collect();
    for (k, (cell, value)) in steps.iter().enumerate() {
        let tx = tx_of(k);
        let c = if *cell == S1 { &s1 } else { &s2 };
        w.publish(c, &value.to_string(), &tx);
        history.lock().unwrap().push((tx.clone(), cell, *value));
        *watermark.lock().unwrap() = tx;
        std::thread::sleep(Duration::from_millis(5));
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let seen: Vec<(String, Answer)> = readers.into_iter().flat_map(|h| h.join().unwrap()).collect();
    assert!(seen.len() >= 80, "{}", seen.len());
    let history = history.lock().unwrap().clone();
    let fresh = w.engine_with(|b| b.memo_capacity(0));
    let mut by_asof: BTreeMap<String, Answer> = BTreeMap::new();
    let keys = all_keys();
    let value = |a: &Answer| match a {
        Answer::Receipt(_) => Some(receipt_of(a).value().clone()),
        Answer::NotCovered => None,
    };
    for (asof, answer) in &seen {
        let want = fresh.get(&top, Some(asof)).unwrap();
        assert_eq!(value(answer), value(&want), "as of {asof}");
        assert_eq!(value(answer), oracle(&history, asof).map(|v| Scalar::Number(v.to_string())), "as of {asof}");
        if let Answer::Receipt(bytes) = answer {
            let r = receipt_of(answer);
            assert!(r.asof() <= asof.as_str(), "as of {asof}: a receipt as of {}", r.asof());
            let out = datumesh_core::verify(bytes, &keys, Some(&*fresh as &dyn datumesh_core::Resolver), None);
            assert_eq!(out.level, Some(datumesh_core::Level::Recomputed), "{out:?}");
        }
        if let Some(prev) = by_asof.insert(asof.clone(), answer.clone()) {
            assert_eq!(value(&prev), value(answer), "two values as of {asof}");
        }
    }
    w.verify_all_issued(&e, "concurrency_stream");
}
