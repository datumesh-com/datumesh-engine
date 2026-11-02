//! Blob content for runtimes (spec-0.4.2, 15.7 `text_of`): `Inputs::read_blob` gives a blob's
//! content from the store, checked against the blob's length and SHA-256. Content the engine
//! cannot obtain, or that does not match, is the evaluator's failure (`E_EVAL_UNAVAILABLE`,
//! `EngineError::BlobUnavailable`), never `not_covered`, whatever the runtime does with it. The
//! content is no input entry; recomputation through the engine as resolver obtains it again.
//! And `Engine::resolve`: an unbound name told apart from other `not_covered` answers.

mod common;

use common::*;
use datumesh_core::{verify, Level, Resolver, Scalar, Type};
use datumesh_engine::{EngineError, EvalError};

const LEN: &str = "calc.example.net/textlen";
const CATCH: &str = "calc.example.net/catch";
const DOC: &str = r#"{"type":"blob","valid":false}"#;

/// textlen(doc) = the number of bytes of the blob's content; it charges 1 + bytes / 64, as
/// SPEC 15.12 charges `text_of`. catch(doc) = the same, or 0 when the content is unavailable
/// (a runtime must not be able to turn the evaluator's failure into a value).
fn world() -> World {
    let mut w = World::new();
    let read = |c: &datumesh_core::Cell, i: &mut dyn datumesh_engine::Inputs| {
        let blob = c.args["doc"].scalar().unwrap().lexical().to_string();
        let mut parts = blob.splitn(3, ':');
        let (sha, len) = (parts.next().unwrap().to_string(), parts.next().unwrap().parse::<u64>().unwrap());
        i.read_blob(&sha, len)
    };
    w.native_fn(LEN, &[], &[("doc", DOC)], move |c, i, b| {
        let bytes = read(c, i)?;
        b.charge(1 + bytes.len() as u64 / 64)?;
        Ok(num(&bytes.len().to_string()))
    });
    w.native_fn(CATCH, &[], &[("doc", DOC)], move |c, i, _| match read(c, i) {
        Ok(bytes) => Ok(num(&bytes.len().to_string())),
        Err(EvalError::BlobUnavailable { .. }) => Ok(num("0")),
        Err(e) => Err(e),
    });
    w
}

fn blob(sha: &str, len: usize) -> Scalar {
    Scalar::from_lexical(Type::Blob, &format!("{sha}:{len}:text/plain")).unwrap()
}

#[test]
fn a_runtime_reads_blob_content_and_the_receipt_recomputes() {
    let w = world();
    let e = w.engine();
    let text = "בְּרֵאשִׁית בָּרָא אֱלֹהִים".repeat(5);
    let sha = w.store.put_blob(text.as_bytes()).unwrap();
    let doc = blob(&sha, text.len());
    let r = receipt_of(&e.get(&w.cellref(LEN, &[("doc", doc.clone())]), Some(&t(20, 0))).unwrap());
    assert_eq!(r.value(), &num(&text.len().to_string()));
    assert!(r.inputs().is_empty(), "the content is no input entry");
    // Recomputation through the engine (a fresh one) as resolver obtains the content again.
    let fresh = w.engine();
    let out = verify(
        &bytes_of(&e.get(&w.cellref(LEN, &[("doc", doc.clone())]), Some(&t(20, 0))).unwrap()),
        &all_keys(),
        Some(&*fresh as &dyn Resolver),
        None,
    );
    assert_eq!(out.level, Some(Level::Recomputed), "{out:?}");
    let ev = fresh.evaluation(r.cell(), r.asof()).unwrap();
    assert_eq!((ev.value, ev.lineage), (num(&text.len().to_string()), Some(vec![])));
    // The fuel the runtime charges: 1 (the call) + 1 + bytes / 64.
    let need = 2 + text.len() as u64 / 64;
    let w2 = world();
    w2.store.put_blob(text.as_bytes()).unwrap();
    match w2.engine_with(|b| b.fuel(need - 1)).get(&w2.cellref(LEN, &[("doc", doc.clone())]), Some(&t(20, 0))) {
        Err(EngineError::BudgetExceeded { .. }) => {}
        other => panic!("{other:?}"),
    }
    assert!(w2.engine_with(|b| b.fuel(need)).get(&w2.cellref(LEN, &[("doc", doc)]), Some(&t(20, 0))).is_ok());
    w.verify_all_issued(&e, "blobs_read");
}

#[test]
fn missing_content_is_the_evaluators_failure() {
    let w = world();
    let e = w.engine();
    // A blob value whose content the store does not have.
    let sha = "a".repeat(64);
    let before = rows(&w);
    for f in [LEN, CATCH] {
        match e.get(&w.cellref(f, &[("doc", blob(&sha, 10))]), Some(&t(20, 0))) {
            Err(EngineError::BlobUnavailable { sha256, .. }) => assert_eq!(sha256, sha, "{f}"),
            other => panic!("{f}: {other:?}"),
        }
    }
    assert_eq!(rows(&w), before, "nothing stored");
    // As a resolver: no value (a verifier reports E_EVAL_UNAVAILABLE).
    assert_eq!(e.evaluate(&w.cell(LEN, &[("doc", blob(&sha, 10))]), &t(20, 0)), None);
}

#[test]
fn content_that_does_not_match_the_blob_is_the_evaluators_failure() {
    let w = world();
    let e = w.engine();
    let sha = w.store.put_blob(b"In the beginning").unwrap();
    // The right hash with another length.
    match e.get(&w.cellref(LEN, &[("doc", blob(&sha, 17))]), Some(&t(20, 0))) {
        Err(EngineError::BlobUnavailable { sha256, .. }) => assert_eq!(sha256, sha),
        other => panic!("{other:?}"),
    }
    match e.get(&w.cellref(CATCH, &[("doc", blob(&sha, 3))]), Some(&t(20, 0))) {
        Err(EngineError::BlobUnavailable { .. }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(w.db().count("computed_versions"), 0);
}

#[test]
fn an_unbound_name_is_told_apart() {
    let w = world();
    let e = w.engine();
    let bound = w.cellref(LEN, &[("doc", blob(&"b".repeat(64), 1))]);
    let cell = e.resolve(&bound, Some(&t(20, 0))).unwrap().expect("bound");
    assert_eq!(cell.fn_hash, w.cell(LEN, &[]).fn_hash);
    assert_eq!(e.resolve(&bound, Some("2019-01-01T00:00:00.000000Z")).unwrap(), None, "before the binding");
    assert_eq!(e.resolve(&w.cellref("calc.example.net/nope", &[]), Some(&t(20, 0))).unwrap(), None);
    // get answers both NotCovered, as before.
    assert_eq!(
        e.get(&w.cellref("calc.example.net/nope", &[]), Some(&t(20, 0))).unwrap(),
        datumesh_engine::Answer::NOT_COVERED
    );
}
