//! Engine 0.6.0, SPEC 8.2 (spec-cr 4196c5a, coordinator decision): a withdrawal that left a
//! member out of a slice read counts, for time, like an input of the reading cell: the
//! computed receipt's `tx` includes the withdrawal's `tx`, and the withdrawal's `asof` is
//! among the reading cell's asof candidates. Without it a read of the reading cell as of
//! between the latest member's asof and the withdrawal's returns the memoised post-withdrawal
//! value, and a verifier re-evaluating at the receipt's asof sees more members than the
//! receipt does (E_INPUT_SUPERSEDED).
//!
//! The spec vectors `formula-eval/withdrawn-slice-asof-candidate` and
//! `withdrawn-outside-range-not-candidate` (spec-cr 6d0a5c5) are formula-language worlds with
//! receipts signed for them; the engine's tests run native functions, so these tests build
//! the same two situations over the withdrawal vectors of tests/engine_060_withdrawal.rs
//! (the cell fx_rate(EUR, 2024-03-15, USD) withdrawn at tx 2024-03-15T16:00, asof
//! 2024-03-16T09:00; siblings GBP and JPY valued at 2024-03-10T12:00): `total` is the sum of
//! the slice over `quote` for a date; for 2024-03-15 the withdrawal left a member out, for
//! 2024-03-16 (the "outside the range" case) it did not touch the slice.
//!
//! No new API: the same `Engine::get` / `Answer::Receipt`; the receipt's times change.

mod common;

use common::*;
use datumesh_core::json::{self, Value};
use datumesh_core::{verify, Args, Cell, Level, Manifest, Receipt, ReceiptBuilder, Resolver, SigningKey, Withdrawal};

const FXR: &str = "rates.example.org/fx_rate";
const TOTAL: &str = "calc.example.net/total";
const D15: &str = "2024-03-15";
const D16: &str = "2024-03-16";
const T0: &str = "2024-03-10T12:00:00.000000Z";
const TW: &str = "2024-03-15T16:00:00.000000Z"; // the withdrawal's tx
const BETWEEN: &str = "2024-03-15T20:00:00.000000Z"; // after the withdrawal's tx, before its asof
const AW: &str = "2024-03-16T09:00:00.000000Z"; // the withdrawal's asof
const LATER: &str = "2024-03-25T00:00:00.000000Z";

fn vector(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/vectors/receipt/{name}.json"));
    json::parse(&std::fs::read(path).unwrap()).unwrap()
}

struct X {
    w: World,
    base: Receipt,
    key: SigningKey,
    key_doc: Vec<u8>,
    wbytes: Vec<u8>,
    wd: Withdrawal,
}

impl X {
    fn cell(&self, date_: &str, quote: &str) -> Cell {
        let mut c = self.base.cell().clone();
        c.args = args(&[("base", text("EUR")), ("date", date(date_)), ("quote", text(quote))]);
        c
    }

    fn publish(&self, date_: &str, quote: &str, value: &str, tx: &str) -> String {
        let cell = self.cell(date_, quote);
        let valid: Args = args(&[("date", date(date_))]);
        let r = ReceiptBuilder::new()
            .cell(cell.clone())
            .value(num(value))
            .valid(valid)
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

    fn withdraw(&self) -> String {
        let h = self.w.store.put_receipt(&self.wbytes).unwrap();
        let id = self.cell(D15, "USD").id();
        assert_eq!(id, self.wd.cell().id());
        self.w.store.put_version(&id, TW, &h).unwrap();
        self.w.notify(&id);
        h
    }

    fn total(&self, date_: &str) -> datumesh_core::CellRef {
        self.w.cellref(TOTAL, &[("day", date(date_))])
    }

    /// The keys: the mesh's and the vector's for rates.example.org.
    fn keys(&self) -> datumesh_core::Keys {
        let mut k = all_keys();
        k.insert_response(RATES, 200, Some(&self.key_doc));
        k
    }
}

/// fx_rate (the vectors' descriptor and keys); USD, GBP, JPY of 15 March and GBP, JPY of
/// 16 March valued at T0; total(day) = the sum over quote of fx_rate(EUR, day, *).
fn world() -> X {
    let mut w = World::new();
    let stored = vector("stored-number");
    let base = Receipt::from_json(stored.get("input").and_then(Value::as_str).unwrap().as_bytes()).unwrap();
    let s = stored.get("keypair").and_then(|k| k.get("seed")).and_then(Value::as_str).unwrap();
    let seed: [u8; 32] =
        (0..32).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect::<Vec<_>>().try_into().unwrap();
    let key = SigningKey::from_seed(&seed);
    let key_doc = stored
        .get("keys")
        .and_then(|k| k.get(RATES))
        .and_then(|k| k.get("https"))
        .and_then(|h| h.get("body"))
        .and_then(Value::as_str)
        .unwrap()
        .as_bytes()
        .to_vec();
    w.store.put_namespace(RATES, &key_doc, "2024-01-01T00:00:00.000000Z").unwrap();
    let h =
        w.store.put_descriptor(&stored.get("recompute").and_then(|r| r.get("descriptor")).unwrap().to_jcs()).unwrap();
    assert_eq!(h, base.cell().fn_hash);
    w.store.bind_name(FXR, &h, BIND_TX).unwrap();
    w.native_fn(TOTAL, &[FXR], &[("day", r#"{"type":"date","valid":false}"#)], |c, i, _| {
        let day = c.args["day"].scalar().unwrap().clone();
        let ms =
            i.read_slice(FXR, &pattern(&[("base", bound(text("EUR"))), ("date", bound(day)), ("quote", open())]))?;
        Ok(sum(ms.iter().map(|m| &m.input.value)))
    });
    let wbytes = vector("withdrawn-primary").get("input").and_then(Value::as_str).unwrap().as_bytes().to_vec();
    let wd = Withdrawal::from_json(&wbytes).unwrap();
    let x = X { w, base, key, key_doc, wbytes, wd };
    for (d, q, v) in
        [(D15, "USD", "100"), (D15, "GBP", "85"), (D15, "JPY", "16000"), (D16, "GBP", "86"), (D16, "JPY", "16100")]
    {
        x.publish(d, q, v, T0);
    }
    x
}

#[test]
fn the_receipt_counts_the_withdrawal_in_tx_and_asof() {
    let x = world();
    let e = x.w.engine();
    let wh = x.withdraw();
    let r = receipt_of(&e.get(&x.total(D15), Some(LATER)).unwrap());
    assert_eq!(r.value(), &num("16085"), "GBP and JPY: the withdrawn USD is out");
    // The members' latest tx and asof are T0; the withdrawal's are TW and AW, later: they count.
    assert_eq!(r.tx(), TW, "tx includes the withdrawal's tx");
    assert_eq!(r.asof(), AW, "the withdrawal's asof is a candidate: the latest of the candidates");
    // The lineage is still one slice entry, whose members are the two valued receipts.
    assert_eq!(r.inputs().len(), 1);
    let m = Manifest::from_json(&e.manifest(&r.inputs()[0]).unwrap()).unwrap();
    assert_eq!(m.members().len(), 2);
    assert!(!m.members().contains(&wh));
}

#[test]
fn a_read_between_the_last_member_and_the_withdrawal_recomputes_the_earlier_value() {
    let x = world();
    let e = x.w.engine();
    x.withdraw();
    // Memoised first as of a time after the withdrawal: value 16085, asof AW.
    assert_eq!(receipt_of(&e.get(&x.total(D15), Some(LATER)).unwrap()).value(), &num("16085"));
    // As of the withdrawal's tx..asof gap, and before its tx but after the members: the
    // withdrawal does not answer yet, USD is a member: 16185, not the memoised 16085.
    for asof in ["2024-03-12T00:00:00.000000Z", TW, BETWEEN, "2024-03-16T08:59:59.999999Z"] {
        let r = receipt_of(&e.get(&x.total(D15), Some(asof)).unwrap());
        assert_eq!(r.value(), &num("16185"), "as of {asof}");
        assert_eq!(r.asof(), T0, "as of {asof}: no withdrawal read, the members' latest");
    }
    // From the withdrawal's asof on: 16085 again, from the one receipt.
    assert_eq!(receipt_of(&e.get(&x.total(D15), Some(AW)).unwrap()).value(), &num("16085"));
    assert_eq!(receipt_of(&e.get(&x.total(D15), Some(AW)).unwrap()).asof(), AW);
}

#[test]
fn a_withdrawal_outside_the_slices_range_is_no_candidate() {
    let x = world();
    let e = x.w.engine();
    x.withdraw();
    // The slice of 16 March: the withdrawal of 15 March left nothing out of it.
    let r = receipt_of(&e.get(&x.total(D16), Some(LATER)).unwrap());
    assert_eq!(r.value(), &num("16186"));
    assert_eq!((r.tx(), r.asof()), (T0, T0), "the members' latest tx and asof");
    // And as of the gap it is the same single receipt (memo hit or recomputed alike).
    let again = receipt_of(&e.get(&x.total(D16), Some(BETWEEN)).unwrap());
    assert_eq!(again.value(), &num("16186"));
    assert_eq!(again.hash(), r.hash());
}

#[test]
fn the_receipt_verifies_at_recomputed_through_the_engine() {
    let x = world();
    let e = x.w.engine();
    x.withdraw();
    let keys = x.keys();
    for day in [D15, D16] {
        let bytes = bytes_of(&e.get(&x.total(day), Some(LATER)).unwrap());
        let out = verify(&bytes, &keys, Some(&*e as &dyn Resolver), None);
        assert!(out.ok, "{day}: {out:?}");
        assert_eq!(out.level, Some(Level::Recomputed), "{day}: {out:?}");
    }
}
