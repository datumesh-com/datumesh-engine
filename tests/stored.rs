//! Stored cells: the publisher's bytes, unchanged; NotCovered before the first version; a
//! correction answers later asofs and the old receipt earlier ones; a late-signed receipt
//! does not answer between its tx and its asof. And the builder's checks.

mod common;

use std::sync::Arc;

use common::*;
use datumesh_core::{Arg, ErrorCode, SigningKey};
use datumesh_engine::{Answer, ConfigError, Engine, EngineError};

const FX: &str = "rates.example.org/fx";

fn fx_world() -> World {
    let w = World::new();
    w.stored_fn(FX, &[("quote", r#"{"type":"string","valid":false}"#)]);
    w
}

#[test]
fn returns_the_publishers_bytes_unchanged() {
    let w = fx_world();
    let e = w.engine();
    let cell = w.cell(FX, &[("quote", text("USD"))]);
    let given = w.sign_stored(&cell, num("1.0892"), &t(15, 16), &t(15, 16));
    let h = w.store.put_receipt(&given).unwrap();
    w.store.put_version(&cell.id(), &t(15, 16), &h).unwrap();
    w.notify(&cell.id());
    let a = e.get(&w.cellref(FX, &[("quote", text("USD"))]), Some(&t(16, 0))).unwrap();
    assert_eq!(a, Answer::Receipt(given.clone()));
    assert_eq!(bytes_of(&a), w.stored_bytes(&h));
    // By cell (fn_hash pinned) too, and the same again (memoised).
    assert_eq!(e.get_cell(&cell, Some(&t(16, 0))).unwrap(), Answer::Receipt(given.clone()));
    assert_eq!(e.get_cell(&cell, Some(&t(16, 0))).unwrap(), Answer::Receipt(given));
    // Nothing was signed or stored by the engine.
    assert_eq!(w.db().count("receipts"), 1);
    assert_eq!(w.db().count("computed_versions"), 0);
}

#[test]
fn not_covered_before_the_first_version_and_for_unknown_functions() {
    let w = fx_world();
    let e = w.engine();
    let cell = w.cell(FX, &[("quote", text("USD"))]);
    let r = w.cellref(FX, &[("quote", text("USD"))]);
    assert_eq!(e.get(&r, Some(&t(16, 0))).unwrap(), Answer::NotCovered);
    w.publish(&cell, "1.0892", &t(15, 16));
    assert_eq!(e.get(&r, Some(&t(15, 15))).unwrap(), Answer::NotCovered);
    assert!(matches!(e.get(&r, Some(&t(15, 16))).unwrap(), Answer::Receipt(_)));
    // An unknown function, and a known one asked as of before its name was bound.
    let unknown = w.cellref("rates.example.org/nope", &[]);
    assert_eq!(e.get(&unknown, Some(&t(16, 0))).unwrap(), Answer::NotCovered);
    assert_eq!(e.get(&r, Some("2019-12-31T23:59:59.999999Z")).unwrap(), Answer::NotCovered);
    let mut pinned = cell.clone();
    pinned.fn_hash = "0".repeat(64);
    assert_eq!(e.get_cell(&pinned, Some(&t(16, 0))).unwrap(), Answer::NotCovered);
}

#[test]
fn a_correction_answers_later_asofs_and_the_old_receipt_earlier_ones() {
    let w = fx_world();
    let e = w.engine();
    let cell = w.cell(FX, &[("quote", text("USD"))]);
    let r = w.cellref(FX, &[("quote", text("USD"))]);
    let h1 = w.publish(&cell, "1.0892", &t(15, 16));
    // Memoise the first answer, as of a time after the correction's tx.
    assert_eq!(bytes_of(&e.get(&r, Some(&t(20, 0))).unwrap()), w.stored_bytes(&h1));
    let h2 = w.publish(&cell, "1.09", &t(17, 9));
    for (asof, want) in [(t(15, 16), &h1), (t(17, 8), &h1), (t(17, 9), &h2), (t(20, 0), &h2)] {
        assert_eq!(bytes_of(&e.get(&r, Some(&asof)).unwrap()), w.stored_bytes(want), "as of {asof}");
    }
}

#[test]
fn a_late_signed_receipt_does_not_answer_between_its_tx_and_its_asof() {
    let w = fx_world();
    let e = w.engine();
    let cell = w.cell(FX, &[("quote", text("USD"))]);
    let r = w.cellref(FX, &[("quote", text("USD"))]);
    // The first version itself late-signed: nothing answers before its asof.
    let h1 = w.publish_late(&cell, "1.0892", &t(15, 16), &t(16, 9));
    assert_eq!(e.get(&r, Some(&t(15, 16))).unwrap(), Answer::NotCovered);
    assert_eq!(e.get(&r, Some(&t(16, 8))).unwrap(), Answer::NotCovered);
    assert_eq!(bytes_of(&e.get(&r, Some(&t(16, 9))).unwrap()), w.stored_bytes(&h1));
    // A late-signed correction: the first version answers until its asof.
    let h2 = w.publish_late(&cell, "1.09", &t(18, 10), &t(20, 0));
    for (asof, want) in [(t(18, 10), &h1), (t(19, 23), &h1), (t(20, 0), &h2), (t(25, 0), &h2)] {
        assert_eq!(bytes_of(&e.get(&r, Some(&asof)).unwrap()), w.stored_bytes(want), "as of {asof}");
    }
}

#[test]
fn no_asof_means_the_clock_reading_and_a_cellref_asof_is_used() {
    let w = fx_world();
    let e = w.engine();
    let cell = w.cell(FX, &[("quote", text("USD"))]);
    let mut r = w.cellref(FX, &[("quote", text("USD"))]);
    let h1 = w.publish(&cell, "1.0892", &t(15, 16));
    w.clock.set(&t(15, 15));
    assert_eq!(e.get(&r, None).unwrap(), Answer::NotCovered);
    w.clock.set(&t(15, 16));
    assert_eq!(bytes_of(&e.get(&r, None).unwrap()), w.stored_bytes(&h1));
    r.asof = Some(t(15, 15));
    assert_eq!(e.get(&r, None).unwrap(), Answer::NotCovered);
    // The asof argument wins over the reference's.
    assert_eq!(bytes_of(&e.get(&r, Some(&t(15, 16))).unwrap()), w.stored_bytes(&h1));
}

#[test]
fn malformed_requests_are_invalid() {
    let w = fx_world();
    let e = w.engine();
    let r = w.cellref(FX, &[("quote", text("USD"))]);
    let code = |res: Result<Answer, EngineError>| match res {
        Err(EngineError::Invalid(e)) => e.code(),
        other => panic!("{other:?}"),
    };
    assert_eq!(code(e.get(&r, Some("2024-03-15"))), ErrorCode::CanonInstant);
    let mut slice = r.clone();
    slice.args.insert("quote".into(), Arg::Slice);
    assert_eq!(code(e.get(&slice, Some(&t(16, 0)))), ErrorCode::Slice);
    let unknown = w.cellref(FX, &[("quote", text("USD")), ("other", text("x"))]);
    assert_eq!(code(e.get(&unknown, Some(&t(16, 0)))), ErrorCode::ArgUnknown);
    let missing = w.cellref(FX, &[]);
    assert_eq!(code(e.get(&missing, Some(&t(16, 0)))), ErrorCode::ArgMissing);
    let typed = w.cellref(FX, &[("quote", num("1"))]);
    assert_eq!(code(e.get(&typed, Some(&t(16, 0)))), ErrorCode::ArgType);
}

#[test]
fn the_builder_checks_its_configuration() {
    let w = fx_world();
    let base = || Engine::builder(w.store.clone(), MESH);
    assert!(matches!(base().build(), Err(ConfigError::NoSigningKey)));
    assert!(matches!(
        Engine::builder(w.store.clone(), "Mesh.Example").signing_key(key_for(MESH)).build(),
        Err(ConfigError::Host(_))
    ));
    // A key the mesh's document does not publish; a host with no document.
    let other = SigningKey::from_seed(&[42; 32]);
    assert!(matches!(base().signing_key(other).build(), Err(ConfigError::KeyNotPublished { .. })));
    assert!(matches!(
        Engine::builder(w.store.clone(), "unknown.example.com").signing_key(key_for(MESH)).build(),
        Err(ConfigError::KeyNotPublished { .. })
    ));
    assert!(matches!(base().signing_key(key_for(MESH)).licence("not a licence").build(), Err(ConfigError::Licence(_))));
    // Runtimes: none for `stored`, one per language.
    let rt: Arc<dyn datumesh_engine::Runtime> = Arc::new(Named("stored"));
    assert!(matches!(base().signing_key(key_for(MESH)).runtime(rt).build(), Err(ConfigError::Runtime(_))));
    let a: Arc<dyn datumesh_engine::Runtime> = Arc::new(Named("expr"));
    let b: Arc<dyn datumesh_engine::Runtime> = Arc::new(Named("expr"));
    assert!(matches!(base().signing_key(key_for(MESH)).runtime(a).runtime(b).build(), Err(ConfigError::Runtime(_))));
    assert!(base().signing_key(key_for(MESH)).build().is_ok());
}

#[test]
fn the_signing_key_comes_from_a_seed_file() {
    let w = fx_world();
    let dir = std::env::temp_dir().join(format!("dm_engine_key_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let hex: String = [9u8; 32].iter().map(|b| format!("{b:02x}")).collect();
    let files = [("hex", format!("{hex}\n").into_bytes()), ("raw", vec![9u8; 32]), ("bad", b"nope".to_vec())];
    for (name, content) in &files {
        std::fs::write(dir.join(name), content).unwrap();
    }
    let build = |name: &str| Engine::builder(w.store.clone(), MESH).signing_key_file(dir.join(name)).build();
    assert!(build("hex").is_ok());
    assert!(build("raw").is_ok());
    assert!(matches!(build("bad"), Err(ConfigError::KeyFile(_))));
    assert!(matches!(build("missing"), Err(ConfigError::KeyFile(_))));
    for (name, _) in &files {
        std::fs::remove_file(dir.join(name)).unwrap();
    }
    std::fs::remove_dir(&dir).unwrap();
}

struct Named(&'static str);

impl datumesh_engine::Runtime for Named {
    fn lang(&self) -> &str {
        self.0
    }
    fn evaluate(
        &self,
        _: &datumesh_core::Descriptor,
        _: &datumesh_core::Cell,
        _: &mut dyn datumesh_engine::Inputs,
        _: &mut datumesh_engine::Budget,
    ) -> Result<datumesh_core::Scalar, datumesh_engine::EvalError> {
        Err(datumesh_engine::EvalError::Runtime("unused".into()))
    }
}
