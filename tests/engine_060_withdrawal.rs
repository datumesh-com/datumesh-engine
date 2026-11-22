//! Engine 0.6.0, R1 (CONTRACT-round-0.7; SPEC 3.6, 15.11-15.13 of spec-0.7.0): a stored cell
//! whose latest version as of the asof is a withdrawal (the null version) is read as no value.
//!
//! The core cannot sign withdrawals, so the tests use the four signed spec vectors
//! `receipt/withdrawn-{primary,relayed,non-ascii,max-length}` (vendored in
//! `tests/vectors/receipt/`, from the spec repository's spec-0.7.0 working tree at 3080969,
//! with `receipt/stored-number`, whose descriptor, key document and keypair they share). All
//! four withdraw the same cell: `rates.example.org/fx_rate(base: EUR, date: 2024-03-15, quote:
//! USD)`, tx 2024-03-15T16:00, asof 2024-03-16T09:00 (late-signed: until its asof the version
//! before it answers). Valued versions of that cell and of its siblings (quote GBP, JPY) are
//! signed here with the vector's keypair.
//!
//! API ASSUMED (the smallest addition; everything else is the 0.5.0 API):
//!
//! ```text
//! // datumesh_engine::Answer (already #[non_exhaustive]) gains a variant:
//! Answer::Withdrawn { receipt: Vec<u8>, reason: String }
//!     // `get` / `get_cell` of a stored cell whose latest version as of the asof is a
//!     // withdrawal: no value, "not covered" (the HTTP layer answers 404 not_covered), with
//!     // the withdrawal receipt's bytes (the store's canonical bytes, `Withdrawal::canonical`;
//!     // what `Resolver::receipt(hash)` returns) and its `withdrawn` text as `reason`.
//!     // `Answer::NOT_COVERED` and `NotCovered { fault }` are unchanged: a cell with no
//!     // receipt, and a computed cell with a withdrawn input, are still those.
//! ```
//!
//! Engine behaviour assumed on top of the store 0.6.0 (`Version.withdrawn`, `stored_asof`
//! returns the withdrawal as the answering version, `slice_values` leaves withdrawn cells out):
//!
//! - a formula's single-cell read of a withdrawn cell is `EvalError::InputNotCovered`
//!   (E_FORMULA_NOT_COVERED, as for a cell with no receipt): the reader's answer is
//!   `Answer::NOT_COVERED` (not `Withdrawn`: the withdrawal is not the reader's own), however
//!   the runtime reacts to the error; nothing is stored for it;
//! - `read_slice` leaves the withdrawn cell out of the members and of the manifest (lineage);
//! - so a runtime's `at(f(*), k)` (the member for key k, else `E_FORMULA_ABSENT`) of it is the
//!   fault Absent, which `otherwise` does catch;
//! - a later valued version ends the withdrawal; a withdrawal stored after a memoised answer
//!   (and announced by `stored_version_added`) supersedes it for asofs from its own.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use datumesh_core::json::{self, Value};
use datumesh_core::{Cell, ErrorCode, Fault, Manifest, Receipt, ReceiptBuilder, Scalar, SigningKey, Withdrawal};
use datumesh_engine::{Answer, EvalError};
use datumesh_store::Resolver;

const FXR: &str = "rates.example.org/fx_rate";
const ONE: &str = "calc.example.net/usd_now";
const FALLBACK: &str = "calc.example.net/usd_or_zero";
const TOTAL: &str = "calc.example.net/total";
const COUNT: &str = "calc.example.net/count";
const AT_USD: &str = "calc.example.net/at_usd";
const AT_USD_OR_0: &str = "calc.example.net/at_usd_or_zero";

const IDS: [&str; 4] =
    ["withdrawn-primary", "withdrawn-relayed", "withdrawn-non-ascii", "withdrawn-max-length"];

const T0: &str = "2024-03-10T12:00:00.000000Z";
const TW: &str = "2024-03-15T16:00:00.000000Z"; // the withdrawals' tx
const MIDDLE: &str = "2024-03-15T20:00:00.000000Z"; // after the withdrawal's tx, before its asof
const AW: &str = "2024-03-16T09:00:00.000000Z"; // the withdrawals' asof
const AFTER: &str = "2024-03-18T00:00:00.000000Z";
const T2: &str = "2024-03-20T08:30:00.000000Z"; // a valued version ends the withdrawal
const LATER: &str = "2024-03-25T00:00:00.000000Z";

fn vector(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/vectors/receipt/{name}.json"));
    json::parse(&std::fs::read(path).unwrap()).unwrap()
}

fn input_of(v: &Value) -> Vec<u8> {
    v.get("input").and_then(Value::as_str).unwrap().as_bytes().to_vec()
}

struct X {
    w: World,
    base: Receipt,
    key: SigningKey,
    wbytes: Vec<u8>,
    wd: Withdrawal,
    /// The receipt hashes of the valued versions at T0: USD, GBP, JPY.
    usd0: String,
    gbp0: String,
    jpy0: String,
}

impl X {
    fn cell(&self, quote: &str) -> Cell {
        let mut c = self.base.cell().clone();
        c.args = args(&[("base", text("EUR")), ("date", date("2024-03-15")), ("quote", text(quote))]);
        c
    }

    fn cellref(&self, quote: &str) -> datumesh_core::CellRef {
        let c = self.cell(quote);
        datumesh_core::CellRef { ns: c.ns.clone(), fn_name: c.fn_name.clone(), args: c.args.clone(), asof: None }
    }

    /// A valued version of the `quote` cell, signed with the vector's key, recorded at `tx`.
    fn publish(&self, quote: &str, value: &str, tx: &str) -> String {
        let cell = self.cell(quote);
        let r = ReceiptBuilder::new()
            .cell(cell.clone())
            .value(num(value))
            .valid(self.base.valid().clone())
            .asof(tx)
            .tx(tx)
            .issued(tx)
            .publisher(self.base.publisher())
            .signer_host(self.base.signer_host())
            .licence(self.base.licence())
            .summary(format!("1 EUR was worth {value} {quote}."))
            .sign(&self.key)
            .unwrap();
        let h = self.w.store.put_receipt(&receipt_json(&r)).unwrap();
        self.w.store.put_version(&cell.id(), tx, &h).unwrap();
        self.w.notify(&cell.id());
        h
    }

    /// Stores the withdrawal of the USD cell (tx TW, asof AW); returns its hash.
    fn withdraw(&self) -> String {
        let h = self.w.store.put_receipt(&self.wbytes).unwrap();
        assert_eq!(h, self.wd.hash());
        self.w.store.put_version(&self.cell("USD").id(), TW, &h).unwrap();
        self.w.notify(&self.cell("USD").id());
        h
    }
}

fn slice_pattern() -> datumesh_engine::SlicePattern {
    pattern(&[("base", bound(text("EUR"))), ("date", bound(date("2024-03-15"))), ("quote", open())])
}

fn usd_args() -> datumesh_core::Args {
    args(&[("base", text("EUR")), ("date", date("2024-03-15")), ("quote", text("USD"))])
}

/// fx_rate (stored, the vector's descriptor, key document and keypair) with USD, GBP and JPY
/// valued at T0; the readers: usd_now (one cell), usd_or_zero (one cell, catching
/// InputNotCovered), total and count (the slice over quote), at_usd (the member for USD, else
/// Absent: `at(fx_rate(EUR, d, *), "USD")`) and at_usd_or_zero (the same under `otherwise`).
fn world(vector_id: &str) -> X {
    let mut w = World::new();
    let stored = vector("stored-number");
    let base = Receipt::from_json(&input_of(&stored)).unwrap();
    let seed: [u8; 32] = {
        let s = stored.get("keypair").and_then(|k| k.get("seed")).and_then(Value::as_str).unwrap();
        (0..32).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect::<Vec<_>>().try_into().unwrap()
    };
    let key = SigningKey::from_seed(&seed);
    // The vector's key document is rates.example.org's (the withdrawal is signed with it).
    let body = stored.get("keys").and_then(|k| k.get(RATES)).and_then(|k| k.get("https")).unwrap();
    w.store.put_namespace(RATES, body.get("body").and_then(Value::as_str).unwrap().as_bytes(), "2024-01-01T00:00:00.000000Z").unwrap();
    let descriptor = stored.get("recompute").and_then(|r| r.get("descriptor")).unwrap().to_jcs();
    let h = w.store.put_descriptor(&descriptor).unwrap();
    assert_eq!(h, base.cell().fn_hash, "the vectors' function");
    w.store.bind_name(FXR, &h, BIND_TX).unwrap();

    w.native_fn(ONE, &[FXR], &[], |_, i, _| i.read(FXR, &usd_args()));
    w.native_fn(FALLBACK, &[FXR], &[], |_, i, _| match i.read(FXR, &usd_args()) {
        Ok(v) => Ok(v),
        Err(EvalError::InputNotCovered { .. }) => {
            SAW_NOT_COVERED.store(true, Ordering::SeqCst);
            Ok(num("0"))
        }
        Err(e) => Err(e),
    });
    w.native_fn(TOTAL, &[FXR], &[], |_, i, _| {
        let members = i.read_slice(FXR, &slice_pattern())?;
        Ok(sum(members.iter().map(|m| &m.input.value)))
    });
    w.native_fn(COUNT, &[FXR], &[], |_, i, _| {
        let members = i.read_slice(FXR, &slice_pattern())?;
        Ok(num(&members.len().to_string()))
    });
    fn at_usd(i: &mut dyn datumesh_engine::Inputs) -> Result<Scalar, EvalError> {
        let members = i.read_slice(FXR, &slice_pattern())?;
        match members.iter().find(|m| m.args["quote"].scalar() == Some(&text("USD"))) {
            Some(m) => Ok(m.input.value.clone()),
            None => Err(EvalError::Fault(Fault::Absent)),
        }
    }
    w.native_fn(AT_USD, &[FXR], &[], |_, i, _| at_usd(i));
    w.native_fn(AT_USD_OR_0, &[FXR], &[], |_, i, _| match at_usd(i) {
        Err(EvalError::Fault(Fault::Absent)) => Ok(num("0")),
        other => other,
    });

    let wbytes = input_of(&vector(vector_id));
    let wd = Withdrawal::from_json(&wbytes).unwrap();
    assert_eq!(wd.cell().id(), {
        let mut c = base.cell().clone();
        c.args = usd_args();
        c.id()
    });
    let mut x = X { w, base, key, wbytes, wd, usd0: String::new(), gbp0: String::new(), jpy0: String::new() };
    x.usd0 = x.publish("USD", "100", T0);
    x.gbp0 = x.publish("GBP", "85", T0);
    x.jpy0 = x.publish("JPY", "16000", T0);
    x
}

static SAW_NOT_COVERED: AtomicBool = AtomicBool::new(false);

/// The withdrawal of an answer (panics on any other answer).
fn withdrawal_of(a: &Answer) -> (Vec<u8>, String) {
    match a {
        Answer::Withdrawn { receipt, reason } => (receipt.clone(), reason.clone()),
        other => panic!("expected a withdrawal, got {other:?}"),
    }
}

#[test]
fn a_stored_cell_whose_latest_version_is_a_withdrawal_answers_not_covered_with_the_withdrawal() {
    for id in IDS {
        let x = world(id);
        let e = x.w.engine();
        let usd = x.cell("USD");
        let r = x.cellref("USD");
        // Before the withdrawal's asof the valued version answers: at the first version, and
        // between the withdrawal's tx and its asof (it is late-signed).
        for asof in [T0, TW, MIDDLE] {
            assert_eq!(
                bytes_of(&e.get_cell(&usd, Some(asof)).unwrap()),
                x.w.stored_bytes(&x.usd0),
                "{id} as of {asof}"
            );
        }
        x.withdraw();
        for asof in [T0, MIDDLE] {
            assert_eq!(bytes_of(&e.get_cell(&usd, Some(asof)).unwrap()), x.w.stored_bytes(&x.usd0), "{id} {asof}");
        }
        // From the asof on: no value; the withdrawal's own bytes and its reason.
        for asof in [AW, AFTER, LATER] {
            for a in [e.get_cell(&usd, Some(asof)).unwrap(), e.get(&r, Some(asof)).unwrap()] {
                assert_ne!(a, Answer::NOT_COVERED, "{id}: the withdrawal is available to the caller");
                let (bytes, reason) = withdrawal_of(&a);
                assert_eq!(bytes, x.wd.canonical(), "{id} as of {asof}");
                assert_eq!(reason, x.wd.withdrawn(), "{id}");
                // The bytes are a withdrawal on the wire, not a valued receipt.
                assert!(Receipt::from_json(&bytes).is_err());
                assert_eq!(Withdrawal::from_json(&bytes).unwrap().hash(), x.wd.hash());
            }
        }
        // A sibling is untouched; nothing was signed or stored by the engine for the reads.
        assert_eq!(bytes_of(&e.get_cell(&x.cell("GBP"), Some(LATER)).unwrap()), x.w.stored_bytes(&x.gbp0));
        assert_eq!(x.w.db().count("computed_versions"), 0, "{id}");
    }
}

#[test]
fn a_withdrawal_stored_after_a_memoised_answer_supersedes_it() {
    let x = world(IDS[0]);
    let e = x.w.engine();
    let usd = x.cell("USD");
    // Memoised as of a time after the withdrawal's asof, before the withdrawal exists.
    assert_eq!(bytes_of(&e.get_cell(&usd, Some(LATER)).unwrap()), x.w.stored_bytes(&x.usd0));
    assert_eq!(bytes_of(&e.get_cell(&usd, Some(LATER)).unwrap()), x.w.stored_bytes(&x.usd0));
    x.withdraw();
    assert_eq!(withdrawal_of(&e.get_cell(&usd, Some(LATER)).unwrap()).1, x.wd.withdrawn());
    // The old asofs keep their answers.
    assert_eq!(bytes_of(&e.get_cell(&usd, Some(MIDDLE)).unwrap()), x.w.stored_bytes(&x.usd0));
}

#[test]
fn a_later_valued_version_ends_the_withdrawal() {
    let x = world(IDS[0]);
    let e = x.w.engine();
    let usd = x.cell("USD");
    x.withdraw();
    assert_eq!(withdrawal_of(&e.get_cell(&usd, Some("2024-03-20T08:29:59.999999Z")).unwrap()).0, x.wd.canonical());
    let h2 = x.publish("USD", "105", T2);
    assert_eq!(withdrawal_of(&e.get_cell(&usd, Some("2024-03-20T08:29:59.999999Z")).unwrap()).0, x.wd.canonical());
    assert_eq!(bytes_of(&e.get_cell(&usd, Some(T2)).unwrap()), x.w.stored_bytes(&h2));
    assert_eq!(bytes_of(&e.get_cell(&usd, Some(LATER)).unwrap()), x.w.stored_bytes(&h2));
    // And the reader has its value back, from the new version.
    let one = x.w.cellref(ONE, &[]);
    assert_eq!(e.get(&one, Some(AFTER)).unwrap(), Answer::NOT_COVERED);
    let r = receipt_of(&e.get(&one, Some(LATER)).unwrap());
    assert_eq!(r.value(), &num("105"));
    assert_eq!(r.inputs(), [h2.as_str()]);
}

#[test]
fn a_formula_reading_the_withdrawn_cell_alone_is_e_formula_not_covered() {
    let x = world(IDS[0]);
    let e = x.w.engine();
    let one = x.w.cellref(ONE, &[]);
    // Memoised with a value first.
    let before = receipt_of(&e.get(&one, Some(LATER)).unwrap());
    assert_eq!(before.value(), &num("100"));
    assert_eq!(before.inputs(), [x.usd0.as_str()]);
    x.withdraw();
    // Between the withdrawal's tx and its asof: still the value, the same receipt.
    assert_eq!(receipt_of(&e.get(&one, Some(MIDDLE)).unwrap()).value(), &num("100"));
    // From its asof on: no value, plain not_covered (not a fault, not the withdrawal: it is
    // the input's, not the reader's), and nothing stored for it.
    let rows = x.w.db().count("computed_versions");
    for asof in [AW, AFTER, LATER] {
        assert_eq!(e.get(&one, Some(asof)).unwrap(), Answer::NOT_COVERED, "as of {asof}");
    }
    assert_eq!(x.w.db().count("computed_versions"), rows);
}

#[test]
fn otherwise_does_not_catch_a_single_cell_read_with_no_value() {
    let x = world(IDS[0]);
    let e = x.w.engine();
    let fallback = x.w.cellref(FALLBACK, &[]);
    assert_eq!(receipt_of(&e.get(&fallback, Some(LATER)).unwrap()).value(), &num("100"));
    x.withdraw();
    // The runtime sees InputNotCovered and returns 0; the engine answers not covered anyway
    // (a receipt cannot list an absence), as for a cell with no receipt.
    assert_eq!(e.get(&fallback, Some(LATER)).unwrap(), Answer::NOT_COVERED);
    assert!(SAW_NOT_COVERED.load(Ordering::SeqCst), "the read's error is InputNotCovered, not a fault");
}

#[test]
fn a_slice_leaves_the_withdrawn_cell_out_count_sum_and_lineage() {
    let x = world(IDS[0]);
    let e = x.w.engine();
    let (total, count) = (x.w.cellref(TOTAL, &[]), x.w.cellref(COUNT, &[]));
    // Three members, memoised as of a time after the withdrawal's asof.
    let before = receipt_of(&e.get(&total, Some(LATER)).unwrap());
    assert_eq!(before.value(), &num("16185"));
    assert_eq!(receipt_of(&e.get(&count, Some(LATER)).unwrap()).value(), &num("3"));
    let withdrawal = x.withdraw();
    // Before the withdrawal's asof: three, from the memo of the old receipt.
    assert_eq!(receipt_of(&e.get(&total, Some(MIDDLE)).unwrap()).value(), &num("16185"));
    // From it on: two members, GBP and JPY.
    let after = receipt_of(&e.get(&total, Some(LATER)).unwrap());
    assert_eq!(after.value(), &num("16085"));
    assert_eq!(receipt_of(&e.get(&count, Some(LATER)).unwrap()).value(), &num("2"));
    assert_eq!(receipt_of(&e.get(&count, Some(AW)).unwrap()).value(), &num("2"));
    // Lineage: one slice entry; its manifest lists the two valued members, neither the
    // withdrawn cell's old version nor the withdrawal.
    assert_eq!(after.inputs().len(), 1);
    let m = Manifest::from_json(&e.manifest(&after.inputs()[0]).expect("the engine serves the manifest")).unwrap();
    let mut want = vec![x.gbp0.clone(), x.jpy0.clone()];
    want.sort();
    assert_eq!(m.members(), want.as_slice());
    assert!(!m.members().contains(&withdrawal) && !m.members().contains(&x.usd0));
    // The old receipt's manifest still has all three.
    let old = Manifest::from_json(&e.manifest(&before.inputs()[0]).unwrap()).unwrap();
    assert_eq!(old.members().len(), 3);
    // A later valued version puts the cell back among the members.
    let h2 = x.publish("USD", "105", T2);
    let back = receipt_of(&e.get(&total, Some(LATER)).unwrap());
    assert_eq!(back.value(), &num("16190"));
    assert_eq!(receipt_of(&e.get(&count, Some(LATER)).unwrap()).value(), &num("3"));
    let m = Manifest::from_json(&e.manifest(&back.inputs()[0]).unwrap()).unwrap();
    assert!(m.members().contains(&h2));
    // Between the withdrawal and the new version it is still out.
    assert_eq!(receipt_of(&e.get(&count, Some(AFTER)).unwrap()).value(), &num("2"));
}

#[test]
fn at_of_a_withdrawn_member_is_e_formula_absent_and_otherwise_catches_it() {
    let x = world(IDS[0]);
    let e = x.w.engine();
    let (at, caught) = (x.w.cellref(AT_USD, &[]), x.w.cellref(AT_USD_OR_0, &[]));
    assert_eq!(receipt_of(&e.get(&at, Some(LATER)).unwrap()).value(), &num("100"));
    x.withdraw();
    assert_eq!(receipt_of(&e.get(&at, Some(MIDDLE)).unwrap()).value(), &num("100"));
    assert_eq!(e.get(&at, Some(LATER)).unwrap(), faulted(Fault::Absent));
    // Unlike a single-cell read, the absence is provable by the slice lineage: caught, with a
    // value whose one input is the slice (the members: GBP and JPY).
    let r = receipt_of(&e.get(&caught, Some(LATER)).unwrap());
    assert_eq!(r.value(), &num("0"));
    assert_eq!(r.inputs().len(), 1);
    let m = Manifest::from_json(&e.manifest(&r.inputs()[0]).unwrap()).unwrap();
    assert_eq!(m.members().len(), 2);
    // The bad-request codes are unaffected: a malformed asof is still invalid.
    match x.w.engine().get_cell(&x.cell("USD"), Some("2024-03-15")) {
        Err(datumesh_engine::EngineError::Invalid(err)) => assert_eq!(err.code(), ErrorCode::CanonInstant),
        other => panic!("{other:?}"),
    }
}
