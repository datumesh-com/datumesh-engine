//! Licence inheritance (SPEC 8.5, spec-0.4.3), through the evaluator: a computed receipt's
//! `licence` is the result of 8.5 over the licences of the input receipts it read (slice
//! members included), with the declarations of the function's publisher (the key document
//! of the host of `cell.ns`, from the store; none when the store has none). A refusal is
//! `EngineError::Unlicensed` (the `unlicensed` envelope), nothing is signed, and a cell that
//! reads a refused cell is refused too. A stored computed version is reused only when it is
//! entitled (judged at its own `issued`, as key validity is); otherwise a new receipt is
//! signed, or the cell is refused.
//!
//! `tests/vectors/licence/` holds the 50 `licence` vectors of spec-0.4.3 (the spec repository
//! at tag `spec-0.4.3`, commit 99123b2, `vectors/licence/`, unchanged): each runs through the
//! engine's own path, its input licences on stored receipts read by a native function.

mod common;

use common::*;
use datumesh_core::json::{self, Value};
use datumesh_core::{check_entitlement, Cell, KeyDocument, LicenceReason, Receipt, ReceiptBuilder};
use datumesh_engine::{Answer, EngineError};

/// The stored inputs: `src(i)`, one cell per input licence.
const SRC: &str = "rates.example.org/src";
/// The function of the `licence` vectors: reads `src(0)` .. `src(n - 1)` and sums them.
const CONVERT: &str = "calc.example.net/convert";
/// Reads `convert(n)`.
const TOP: &str = "calc.example.net/top";
/// Reads the slice of every `src` cell.
const TOTAL: &str = "calc.example.net/total";
/// A function whose publisher's host has no key document in the store.
const ELSEWHERE: &str = "other.example.org/convert";
const RESTRICTED: &str = "https://rates.example.org/licences/restricted";
/// When the inputs are recorded.
const T0: &str = "2024-03-15T16:00:00.000000Z";

fn n_arg(cell: &Cell) -> usize {
    cell.args["n"].scalar().unwrap().lexical().parse().unwrap()
}

fn world() -> World {
    let mut w = World::new();
    w.stored_fn(SRC, &[("i", r#"{"type":"string","valid":false}"#)]);
    for f in [CONVERT, ELSEWHERE] {
        w.native_fn(f, &[SRC], &[("n", number_arg())], |cell, inputs, _| {
            let mut values = Vec::new();
            for i in 0..n_arg(cell) {
                values.push(inputs.read(SRC, &args(&[("i", text(&i.to_string()))]))?);
            }
            Ok(sum(&values))
        });
    }
    w.native_fn(TOP, &[CONVERT], &[("n", number_arg())], |cell, inputs, _| {
        inputs.read(CONVERT, &args(&[("n", num(&n_arg(cell).to_string()))]))
    });
    w.native_fn(TOTAL, &[SRC], &[], |_, inputs, _| {
        let members = inputs.read_slice(SRC, &pattern(&[("i", open())]))?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w
}

fn src(w: &World, i: usize) -> Cell {
    w.cell(SRC, &[("i", text(&i.to_string()))])
}

fn get(w: &World, e: &datumesh_engine::Engine, f: &str, n: usize) -> Result<Answer, EngineError> {
    e.get(&w.cellref(f, &[("n", num(&n.to_string()))]), None)
}

/// The input licences of a receipt the mesh signed, from the store (members of slices
/// included).
fn input_licences(w: &World, r: &Receipt) -> Vec<String> {
    let mut out = Vec::new();
    for h in r.inputs() {
        let hashes: Vec<String> = if h.starts_with("slice:") {
            let m = datumesh_core::Manifest::from_json(&w.store.manifest(h).unwrap()).unwrap();
            m.members().to_vec()
        } else {
            vec![h.clone()]
        };
        for h in hashes {
            out.push(Receipt::from_json(&w.stored_bytes(&h)).unwrap().licence().to_string());
        }
    }
    out
}

/// The entitlement check (core's, SPEC 10.3) on a receipt the mesh signed, with the
/// publisher's key document as the store holds it.
fn entitlement(w: &World, r: &Receipt) -> datumesh_core::Entitlement {
    let host = r.cell().ns.split('/').next().unwrap();
    let doc = w.store.namespace(host).unwrap().unwrap();
    let k = KeyDocument::from_json(host, &doc.keydoc).unwrap();
    check_entitlement(r, input_licences(w, r), &k).unwrap()
}

fn unlicensed(r: Result<Answer, EngineError>) -> (String, LicenceReason) {
    match r {
        Err(EngineError::Unlicensed { cell_id, reason }) => (cell_id, reason),
        other => panic!("not unlicensed: {other:?}"),
    }
}

#[test]
fn the_licence_vectors_of_spec_0_4_3_through_the_engine() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/licence");
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    assert_eq!(files.len(), 50);
    let mut ran = 0;
    for p in &files {
        let v = json::parse(&std::fs::read(p).unwrap()).unwrap();
        let id = v.get("id").and_then(Value::as_str).unwrap().to_string();
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap().to_string();
        assert_eq!(s("fn"), CONVERT, "{id}");
        let Some(Value::Array(ls)) = v.get("licences") else { panic!("{id}: licences") };
        let licences: Vec<String> = ls
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        let decls = v.get("declarations").unwrap();
        let w = world();
        if decls.as_object().is_some_and(|m| !m.is_empty()) {
            w.declare(CALC, &String::from_utf8(decls.to_jcs()).unwrap());
        }
        w.clock.set(&s("issued"));
        let mesh_licence = s("mesh_licence");
        let e = w.engine_with(|b| b.licence(mesh_licence.clone()));
        for (i, l) in licences.iter().enumerate() {
            w.publish_licensed(&src(&w, i), "1", T0, l);
        }
        let want = v.get("licence").unwrap();
        let reason = LicenceReason::from_name(&s("reason")).unwrap();
        let got = get(&w, &e, CONVERT, licences.len());
        match want.as_str() {
            Some(l) => {
                let r = receipt_of(&got.unwrap_or_else(|e| panic!("{id}: {e}")));
                assert_eq!(r.licence(), l, "{id}");
                assert_eq!(r.issued(), s("issued"), "{id}");
                let ent = entitlement(&w, &r);
                assert!(ent.entitled, "{id}: {ent:?}");
                assert_eq!(ent.reason, reason, "{id}");
                w.verify_all_issued(&e, "licence_vectors");
            }
            None => {
                let (cell_id, why) = unlicensed(got);
                assert_eq!(why, reason, "{id}");
                assert_eq!(cell_id, w.cell(CONVERT, &[("n", num(&licences.len().to_string()))]).id(), "{id}");
                assert!(w.issued_receipts().is_empty(), "{id}: nothing is signed");
                assert_eq!(w.db().count("computed_versions"), 0, "{id}");
            }
        }
        ran += 1;
    }
    assert_eq!(ran, 50);
}

#[test]
fn a_refused_cell_signs_nothing_and_its_readers_are_refused_too() {
    let w = world();
    let e = w.engine();
    w.publish_licensed(&src(&w, 0), "1", T0, "CC-BY-4.0");
    w.publish_licensed(&src(&w, 1), "2", T0, RESTRICTED);
    let convert = w.cell(CONVERT, &[("n", num("2"))]).id();
    let (id, why) = unlicensed(get(&w, &e, CONVERT, 2));
    assert_eq!((id.as_str(), why), (convert.as_str(), LicenceReason::NotDeclared));
    // The reader of a refused cell: refused, for the cell it read.
    let (id, why) = unlicensed(get(&w, &e, TOP, 2));
    assert_eq!((id.as_str(), why), (convert.as_str(), LicenceReason::NotDeclared));
    assert!(w.issued_receipts().is_empty());
    assert_eq!(w.db().count("computed_versions"), 0);
    // Again (nothing memoised): the same.
    assert!(matches!(get(&w, &e, TOP, 2), Err(EngineError::Unlicensed { .. })));
    // Over the open input alone, both compute.
    let r = receipt_of(&get(&w, &e, TOP, 1).unwrap());
    assert_eq!(r.licence(), "CC-BY-4.0");
}

#[test]
fn declared_for_the_function_read_but_not_for_its_reader() {
    let w = world();
    w.declare(CALC, &declaration(CONVERT, RESTRICTED, "2020-01-01T00:00:00.000000Z", None));
    let e = w.engine();
    w.publish_licensed(&src(&w, 0), "1", T0, "CC0-1.0");
    w.publish_licensed(&src(&w, 1), "2", T0, RESTRICTED);
    let r = receipt_of(&get(&w, &e, CONVERT, 2).unwrap());
    assert_eq!(r.licence(), RESTRICTED);
    assert_eq!(entitlement(&w, &r).reason, LicenceReason::Declared);
    // top reads convert, whose receipt carries the restricted licence: top needs its own
    // declaration.
    let (id, why) = unlicensed(get(&w, &e, TOP, 2));
    assert_eq!((id, why), (w.cell(TOP, &[("n", num("2"))]).id(), LicenceReason::NotDeclared));
    // Declared for both: computed, the restricted licence carried up.
    let both = format!(
        r#"{{"{CONVERT}":[{{"licence":"{RESTRICTED}","valid_from":"2020-01-01T00:00:00.000000Z"}}],"{TOP}":[{{"licence":"{RESTRICTED}","valid_from":"2020-01-01T00:00:00.000000Z"}}]}}"#
    );
    w.declare(CALC, &both);
    let r = receipt_of(&get(&w, &e, TOP, 2).unwrap());
    assert_eq!(r.licence(), RESTRICTED);
    assert!(entitlement(&w, &r).entitled);
    w.verify_all_issued(&e, "licence_declared_chain");
}

#[test]
fn slice_members_count_as_inputs() {
    let w = world();
    let e = w.engine();
    w.publish_licensed(&src(&w, 0), "1", T0, "MIT");
    w.publish_licensed(&src(&w, 1), "2", T0, "CC-BY-SA-4.0");
    let r = receipt_of(&e.get(&w.cellref(TOTAL, &[]), None).unwrap());
    assert_eq!(r.value(), &num("3"));
    assert!(r.inputs()[0].starts_with("slice:"));
    assert_eq!(r.licence(), "CC-BY-SA-4.0");
    assert!(entitlement(&w, &r).entitled);
    w.verify_all_issued(&e, "licence_slice");
    // A member under a different share-alike licence: refused.
    w.publish_licensed(&src(&w, 2), "3", T0, "ODbL-1.0");
    let (_, why) = unlicensed(e.get(&w.cellref(TOTAL, &[]), None));
    assert_eq!(why, LicenceReason::ShareAlikeConflict);
}

#[test]
fn a_publisher_without_a_key_document_in_the_store_declares_nothing() {
    let w = world();
    // A declaration of another host's function does not count for this one.
    w.declare(CALC, &declaration(CONVERT, RESTRICTED, "2020-01-01T00:00:00.000000Z", None));
    let e = w.engine();
    w.publish_licensed(&src(&w, 0), "1", T0, "CC-BY-4.0");
    w.publish_licensed(&src(&w, 1), "2", T0, RESTRICTED);
    assert!(w.store.namespace("other.example.org").unwrap().is_none());
    // Open inputs need no declaration, and no key document.
    let r = receipt_of(&get(&w, &e, ELSEWHERE, 1).unwrap());
    assert_eq!(r.licence(), "CC-BY-4.0");
    // A restricted one: not declared.
    let (_, why) = unlicensed(get(&w, &e, ELSEWHERE, 2));
    assert_eq!(why, LicenceReason::NotDeclared);
    // The same inputs for convert, whose publisher declared the licence: computed.
    assert_eq!(receipt_of(&get(&w, &e, CONVERT, 2).unwrap()).licence(), RESTRICTED);
}

/// A computed receipt signed as engines before 0.4.1 did, under the engine's configured
/// licence whatever its inputs, stored as the cell's computed version.
fn legacy(w: &World, cell: &Cell, value: &str, inputs: &[&str], tx: &str, licence: &str) -> String {
    let r = ReceiptBuilder::new()
        .cell(cell.clone())
        .value(num(value))
        .valid(datumesh_core::Args::new())
        .asof(tx)
        .tx(tx)
        .issued("2024-03-16T00:00:00.000000Z")
        .publisher(cell.ns.clone())
        .signer_host(MESH)
        .inputs(inputs.iter().map(|s| s.to_string()))
        .licence(licence)
        .summary(format!("{} computed by {MESH}.", cell.function_name()))
        .sign(&key_for(MESH))
        .unwrap();
    let h = w.store.put_receipt(&receipt_json(&r)).unwrap();
    w.store.put_computed(&cell.id(), tx, &h).unwrap();
    h
}

#[test]
fn a_stored_version_with_another_licence_is_signed_again() {
    let w = world();
    w.declare(CALC, &declaration(CONVERT, RESTRICTED, "2020-01-01T00:00:00.000000Z", None));
    let h = w.publish_licensed(&src(&w, 0), "1", T0, RESTRICTED);
    let cell = w.cell(CONVERT, &[("n", num("1"))]);
    let old = legacy(&w, &cell, "1", &[&h], T0, "CC-BY-4.0");
    let e = w.engine();
    let r = receipt_of(&get(&w, &e, CONVERT, 1).unwrap());
    assert_ne!(r.hash(), old);
    assert_eq!(r.licence(), RESTRICTED);
    assert_eq!((r.tx(), r.asof(), r.inputs()), (T0, T0, [h.clone()].as_slice()));
    assert!(entitlement(&w, &r).entitled);
    // A fresh engine finds the receipt it signed again (no third receipt).
    let e2 = w.engine();
    let r2 = receipt_of(&get(&w, &e2, CONVERT, 1).unwrap());
    assert_eq!(r2.hash(), r.hash());
    assert_eq!(w.issued_receipts().len(), 2, "the old receipt and the new one");
    w.verify_all_issued(&e2, "licence_legacy_resigned");
}

#[test]
fn a_stored_version_the_rule_now_refuses_is_not_served() {
    let w = world();
    let h = w.publish_licensed(&src(&w, 0), "1", T0, RESTRICTED);
    let cell = w.cell(CONVERT, &[("n", num("1"))]);
    legacy(&w, &cell, "1", &[&h], T0, "CC-BY-4.0");
    let e = w.engine();
    let (id, why) = unlicensed(get(&w, &e, CONVERT, 1));
    assert_eq!((id, why), (cell.id(), LicenceReason::NotDeclared));
    // As of an earlier time too: the stored version is not served.
    let (_, why) =
        unlicensed(e.get(&w.cellref(CONVERT, &[("n", num("1"))]), Some("2024-03-20T00:00:00.000000Z")));
    assert_eq!(why, LicenceReason::NotDeclared);
}

#[test]
fn a_version_entitled_at_its_issued_stays_reusable_after_the_declaration_closes() {
    let w = world();
    let close = "2026-06-01T00:00:00.000000Z";
    w.declare(CALC, &declaration(CONVERT, RESTRICTED, "2020-01-01T00:00:00.000000Z", Some(close)));
    w.publish_licensed(&src(&w, 0), "1", T0, RESTRICTED);
    w.publish_licensed(&src(&w, 1), "2", T0, "CC-BY-4.0");
    w.clock.set("2026-01-01T00:00:00.000000Z");
    let e = w.engine();
    let r = receipt_of(&get(&w, &e, CONVERT, 1).unwrap());
    assert_eq!(r.licence(), RESTRICTED);
    // The declaration has closed since.
    w.clock.set("2026-07-01T00:00:00.000000Z");
    // The memo, and a fresh engine validating the stored version: the same receipt.
    assert_eq!(receipt_of(&get(&w, &e, CONVERT, 1).unwrap()).hash(), r.hash());
    let e2 = w.engine();
    assert_eq!(receipt_of(&get(&w, &e2, CONVERT, 1).unwrap()).hash(), r.hash());
    assert!(entitlement(&w, &r).entitled, "judged at its own issued");
    // A cell not computed while the declaration was open: refused now.
    let (_, why) = unlicensed(get(&w, &e2, CONVERT, 2));
    assert_eq!(why, LicenceReason::DeclarationNotOpen);
}

#[test]
fn an_input_republished_with_the_same_value_under_another_licence() {
    let w = world();
    let e = w.engine();
    w.publish_licensed(&src(&w, 0), "1", T0, "CC-BY-4.0");
    let r1 = receipt_of(&get(&w, &e, CONVERT, 1).unwrap());
    assert_eq!(r1.licence(), "CC-BY-4.0");
    // The same value, now share-alike: early cutoff would keep r1; its licence is no longer
    // the rule's, so a new receipt is signed.
    w.publish_licensed(&src(&w, 0), "1", "2024-03-17T16:00:00.000000Z", "CC-BY-SA-4.0");
    let r2 = receipt_of(&get(&w, &e, CONVERT, 1).unwrap());
    assert_ne!(r2.hash(), r1.hash());
    assert_eq!(r2.licence(), "CC-BY-SA-4.0");
    assert!(entitlement(&w, &r2).entitled);
    // Then the same value under an undeclared restricted licence: refused.
    w.publish_licensed(&src(&w, 0), "1", "2024-03-18T16:00:00.000000Z", RESTRICTED);
    let (_, why) = unlicensed(get(&w, &e, CONVERT, 1));
    assert_eq!(why, LicenceReason::NotDeclared);
    let e2 = w.engine();
    let (_, why) = unlicensed(get(&w, &e2, CONVERT, 1));
    assert_eq!(why, LicenceReason::NotDeclared);
    // As of before the change, the share-alike receipt still answers.
    let r = receipt_of(&e2.get(&w.cellref(CONVERT, &[("n", num("1"))]), Some("2024-03-18T00:00:00.000000Z")).unwrap());
    assert_eq!(r.hash(), r2.hash());
    w.verify_all_issued(&e2, "licence_republished");
}

#[test]
fn a_constant_carries_the_mesh_licence() {
    let mut w = world();
    w.native_fn("calc.example.net/constant", &[], &[], |_, _, _| Ok(num("42")));
    let e = w.engine_with(|b| b.licence("https://example.com/licence"));
    let r = receipt_of(&e.get(&w.cellref("calc.example.net/constant", &[]), None).unwrap());
    assert_eq!(r.licence(), "https://example.com/licence");
    assert_eq!(entitlement(&w, &r).reason, LicenceReason::NoInputs);
}
