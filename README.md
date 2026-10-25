# datumesh-engine

The [Datumesh](https://datumesh.com) engine: the value of a cell as of a time. `Engine::get` answers with a receipt: for a stored cell, the publisher's receipt from the store, unchanged; for a computed cell, one the engine signs with the mesh's key after evaluating the cell's function through a `Runtime`, from the receipts of the cells it reads. Answers are memoised, computed versions are kept in the store and validated before reuse, and evaluation runs on an explicit stack, so a function may read itself (SecDB-style recursion) to any depth the limit allows. It implements [spec-0.4.0](https://github.com/datumesh-com/datumesh-spec) (slice-as-lineage, transaction-time reads, evaluation faults, normative fuel) on [`datumesh-store`](https://crates.io/crates/datumesh-store) 0.3.2 and [`datumesh-core`](https://crates.io/crates/datumesh-core) 0.3.4. Function languages plug in through `Runtime`; HTTP is not part of it.

## Example

```toml
[dependencies]
datumesh-engine = "0.3.1"
datumesh-store = "0.3.2"
datumesh-core = { version = "0.3.4", features = ["sign"] }
```

A publisher's two stored functions (net price and tax per item), a computed function adding them in a toy language `sum`, its runtime, and the engine answering the computed cell; the receipt it signs then verifies at level `recomputed` with the engine as the resolver:

```rust
use std::sync::Arc;

use datumesh_core::json::Value;
use datumesh_core::{verify, Arg, Args, Cell, CellRef, FunctionDescriptor, Keys, Level, ReceiptBuilder, Resolver, Scalar, SigningKey, Type};
use datumesh_engine::{Answer, Budget, Engine, EvalError, Inputs, Runtime};
use datumesh_store::{migrate, PgStore, Store};

/// The language `sum`: the sum of the function's inputs, each read with the cell's arguments.
struct Sum;

impl Runtime for Sum {
    fn lang(&self) -> &str {
        "sum"
    }
    fn evaluate(&self, d: &FunctionDescriptor, cell: &Cell, inputs: &mut dyn Inputs, budget: &mut Budget) -> Result<Scalar, EvalError> {
        let mut total: i128 = 0;
        for name in &d.inputs {
            budget.charge(1)?;
            let value = inputs.read(name, &cell.args)?; // propagate every read error
            total += value.lexical().parse::<i128>().map_err(|e| EvalError::Runtime(e.to_string()))?;
        }
        Scalar::from_lexical(Type::Number, &total.to_string()).map_err(|e| EvalError::Runtime(e.to_string()))
    }
}

/// A key document publishing `key` for `host`, as https://<host>/.well-known/datumesh.json serves it.
fn key_doc(host: &str, key: &SigningKey) -> String {
    let public: String = key.public_key().iter().map(|b| format!("{b:02x}")).collect();
    format!(r#"{{"host":"{host}","keys":[{{"alg":"ed25519","id":"{}","public":"{public}","valid_from":"2026-01-01T00:00:00.000000Z"}}],"proto":"datumesh/1"}}"#, key.key_id())
}

fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let store = Arc::new(PgStore::connect(&url).unwrap());
    migrate::apply(&*store).unwrap();

    // The store verifies every receipt against its signer host's key document.
    let publisher = SigningKey::from_seed(&[1; 32]);
    let mesh = SigningKey::from_seed(&[2; 32]);
    let mut keys = Keys::new();
    for (host, key) in [("prices.example.org", &publisher), ("mesh.example.com", &mesh)] {
        store.put_namespace(host, key_doc(host, key).as_bytes(), "2026-01-01T00:00:00.000000Z").unwrap();
        keys.insert_response(host, 200, Some(key_doc(host, key).as_bytes()));
    }

    // Functions: two stored, one computed; names bound once (re-running finds them bound).
    let signature = r#"{"args":{"item":{"type":"string","valid":false}},"returns":{"type":"number","unit":"EUR"}}"#;
    let functions = [
        ("prices.example.org/net", r#"{"body":"","lang":"stored"}"#, "[]"),
        ("prices.example.org/tax", r#"{"body":"","lang":"stored"}"#, "[]"),
        ("shop.example.com/total", r#"{"body":"","lang":"sum"}"#, r#"["prices.example.org/net","prices.example.org/tax"]"#),
    ];
    for (name, code, inputs) in functions {
        let d = format!(r#"{{"code":{code},"inputs":{inputs},"signature":{signature}}}"#);
        let fn_hash = store.put_descriptor(d.as_bytes()).unwrap();
        if store.resolve_name(name, "2026-01-01T00:00:00.000000Z").unwrap().is_none() {
            store.bind_name(name, &fn_hash, "2026-01-01T00:00:00.000000Z").unwrap();
        }
    }

    // The publisher's versions: net 100 recorded on 1 March, tax 20 on 2 March.
    let item: Args = [("item".to_string(), Arg::Value(Scalar::from_lexical(Type::String, "bolt").unwrap()))].into();
    for (f, value, tx) in [("net", "100", "2026-03-01T09:00:00.000000Z"), ("tax", "20", "2026-03-02T09:00:00.000000Z")] {
        let fn_hash = store.resolve_name(&format!("prices.example.org/{f}"), tx).unwrap().unwrap();
        let cell = Cell { ns: "prices.example.org".into(), fn_name: f.into(), fn_hash, args: item.clone() };
        let receipt = ReceiptBuilder::new()
            .cell(cell.clone())
            .value(Scalar::from_lexical(Type::Number, value).unwrap())
            .valid(Args::new())
            .asof(tx)
            .tx(tx)
            .issued(tx)
            .publisher("prices.example.org")
            .signer_host("prices.example.org")
            .licence("CC-BY-4.0")
            .summary(format!("The {f} price of a bolt."))
            .sign(&publisher)
            .unwrap();
        let mut members = vec![("sig".to_string(), Value::String(receipt.sig().to_string()))];
        members.extend(datumesh_core::json::parse(receipt.payload()).unwrap().as_object().unwrap().iter().cloned());
        let hash = store.put_receipt(&Value::Object(members).to_jcs()).unwrap();
        if store.latest(&cell.id()).unwrap().is_none() {
            store.put_version(&cell.id(), tx, &hash).unwrap();
        }
    }

    // The engine: the store, the mesh's host and key, the runtimes.
    let shared: Arc<dyn Store + Send + Sync> = store.clone();
    let engine = Engine::builder(shared, "mesh.example.com").signing_key(mesh).runtime(Arc::new(Sum)).build().unwrap();
    let total = CellRef { ns: "shop.example.com".into(), fn_name: "total".into(), args: item, asof: None };
    let Answer::Receipt(bytes) = engine.get(&total, Some("2026-03-05T00:00:00.000000Z")).unwrap() else { panic!("not covered") };

    let result = verify(&bytes, &keys, Some(&engine as &dyn Resolver), None);
    assert_eq!(result.level, Some(Level::Recomputed), "{:?}", result.error());
    let receipt = datumesh_core::Receipt::from_json(&bytes).unwrap();
    println!("total = {} as of {}, tx {}", receipt.value().lexical(), receipt.asof(), receipt.tx());
    println!("verified {} receipt {} by {}", result.level.unwrap().as_str(), receipt.hash(), receipt.computed_by());
}
```

It prints `total = 120 as of 2026-03-02T09:00:00.000000Z, tx 2026-03-02T09:00:00.000000Z`, then the receipt's hash and `mesh.example.com#<key id>`. Run again, it validates the stored version instead of computing a new one, and prints the same.

## What it does

- **Answering as of A.** A receipt answers as of A only when its `tx` and its `asof` are both at or before A. Stored cell: `stored_asof(cell, A)`, the publisher's bytes unchanged. Computed cell: the computed version with the greatest `asof <= A`, validated as of A; when none validates, the function is evaluated and a receipt signed (or the version kept, when the value is the same). No asof: the clock reading at the start of the request (also the `issued` of what it signs). Names resolve as of A, the function's and its inputs'. `get_cell` takes a cell with its function hash pinned.
- **Computed receipts.** `computed_by` = `<mesh host>#<key id>`; `inputs` = the receipts read; `tx` = the latest input `tx`; `asof` = the latest of its input asofs and the start of every name binding it relied on (the names read, transitively; from `resolve_binding`), so that it holds, and recomputes, as of its own `asof` (an input kept across a change of its own inputs counts from where its value is known again). One receipt per version, keyed on (cell, `tx`, `asof`), stored with `put_computed`, never in `versions`; historical requests add older versions (backfill). `licence` and `summary` come from the engine's configuration.
- **Receipt identity.** Two valid receipts for the same value may exist with different lineage (an input that changes and changes back: early cutoff can accept either version). After a backdated correction (an input superseded at a `tx` earlier than the receipt's `tx`), the old receipt stays signature-valid and answers the asofs it was issued for, but no longer recomputes: it is historical, not wrong. The crate documentation has the details.
- **Memo and early cutoff.** A version is valid as of A when each input gives the same receipt or the same value. The memo keeps answers with the interval they hold for; `stored_version_added` (call it when a stored version is added) drops the answers that rest on the cell, and the unchanged ones are rechecked from memory. The memo is an optimisation: losing it (a restart) costs work, not correctness; early-cutoff records need not be persisted.
- **Slices (spec-0.4.0).** `Inputs::read_slice` reads a slice as of A in key order: a stored function's matching cells (`Store::slice_members`), or a computed function's declared `extent`, each member a cell the engine evaluates and memoises (a member on the stack is a cycle; a member without a value makes the read `E_FORMULA_ABSENT`). One `inputs` entry `slice:<slice id>:<members hash>` per slice; its manifest is stored and served by the engine as a resolver. Validation compares members hashes, then the members' values; `stored_version_added` drops the answers whose slice pattern the new version falls into, and nothing else (a correction to one day of a running total drops that day and the later ones). `read_versions` (a version slice) and `read_asof` (a cell as of a transaction time `T`, `E_FORMULA_FUTURE` after A) complete the reads; a computed receipt's `asof` counts every member, binding start, `T` and `to`.
- **Faults.** A runtime's `EvalError::Fault` (`E_FORMULA_DIV_ZERO`, ...) gives no receipt: `Answer::NotCovered { fault: Some(code) }`, for the envelope's `fault`. Nothing is stored for the faulted cell; a reader gets the same fault and may catch it.
- **Asof-dependent functions** (`Runtime::asof_dependent`, or reading `Inputs::asof`): the receipt's `asof` is the evaluation asof, one receipt per requested asof, never reused for another asof by the memo or the store.
- **Recursion.** An explicit stack; a read of an input not computed yet suspends the function (`EvalError::Suspended`), which is run again once the input is answered. `Cycle`, a depth limit (default 10,000; a 100,000-deep chain runs on a 2 MiB thread under a raised one).
- **Budget.** Fuel is normative (SPEC 15.12): each attempt of an evaluation has its own meter, and a cell's fuel is that of its completed attempt, whatever the restarts; the request's fuel (default 10,000,000) bounds the sum, checked at every charge against a lower bound. A work limit (default 16 times the fuel) bounds everything else, discarded attempts included. `BudgetExceeded` and `WorkExceeded` store nothing.
- **Single-flight.** Evaluations run one at a time per engine: concurrent requests for one uncomputed cell evaluate it once. Memoised and stored answers do not wait.
- **Resolver.** `Engine` is a time-aware `datumesh_core::Resolver` with the same answering rule; computed cells are evaluated, never read back from their own receipts, and `evaluation` gives the lineage (so a verifier reports `E_INPUT_SUPERSEDED` for a receipt whose input was superseded); `manifest` serves stored manifests. The core's test kit passes every check under its default answering rule `TxAndAsof`, with the slice checks (`Kit::slices(true)`, `tests/conformance.rs`).

The crate documentation describes the `Runtime` and `Inputs` contract for function languages and the `get` contract for an HTTP layer (answers, errors, envelope outcomes). The API is synchronous; `Engine` is `Send + Sync`. Dependencies: `datumesh-core` (feature `sign`), `datumesh-store`, and `tracing` (failures that `Resolver::evaluate` turns into `None`, and computed-version conflicts, at target `datumesh_engine`). No `unsafe`, no panics on any input, no global state, no network, no file but the configured key file.

Feature `native-test`: `native_test::NativeTest`, a runtime of Rust closures keyed by function hash, for tests and the benchmark.

## Development

With a PostgreSQL 16 server whose role may create databases in `DATABASE_URL`, `cargo test --all-features` runs everything (each test in a database of its own, all on `PgStore`; the 100,000-deep chain of `tests/recursion.rs` takes about 8 minutes of it). `tools/cargo.sh <toolchain> <args>` runs cargo in a `rust:<toolchain>` container next to a `datumesh-pg` container; `tools/package_test.sh` is the packaging test; `tools/public_api.sh` regenerates `public-api.txt` and `public-api-all-features.txt` (`cargo public-api`); `tools/crosscheck.sh` verifies every receipt the test suite signs with the Python and JavaScript verifiers (`datumesh` 0.4.0, `@datumesh/verify` 0.4.0) at level signed, and checks every manifest against its slice entry.

## Benchmark

`examples/bench.rs` (`cargo run --release --features native-test --example bench`), against PostgreSQL 16 with default settings on the development host (6 vCPU, 15 GB RAM, `synchronous_commit` on), one call at a time; the full run is in `bench/results/2026-10-25-0.3.0.md` (0.2.0's and 0.1.0's beside it; 0.3.1's, back to back with 0.2.0 and with the early-cutoff case repeated, in `bench/results/2026-10-26-0.3.1.md`):

| Operation | p50 | p99 |
|---|---:|---:|
| warm `get` stored, by reference (resolving the name: 1 query) | 197 µs | 401 µs |
| warm `get_cell` stored (pinned: no query) | 5.3 µs | 15 µs |
| warm `get` computed, by reference (2 name queries) | 375 µs | 777 µs |
| warm `get_cell` computed (1 name query) | 217 µs | 416 µs |
| cold `get` computed, first ever (sign, `put_receipt`, `put_computed`) | 6.6 ms | 9.2 ms |
| `get` computed, empty memo, version in the store (validated) | 1.6 ms | 3.0 ms |
| cold `get` of a diamond (4 computed cells) | 28 ms | 39 ms |
| cold `get` of a 1,000-deep chain | 5.6 s | 5.8 s |
| signing one receipt | 290 µs | 547 µs |
| storing one receipt (`put_receipt` verifies it) | 2.6 ms | 4.0 ms |

A 10,000-cell graph (100 layers of 100, each cell reading two below): built cold in 58 s; one correction of a stored input under it, until the next `get` of the top is correct: 29 s when the change propagates, about 200 ms when early cutoff stops it at the first layer (0.3.1; 0.3.0 was 62% slower than 0.2.0 there, 0.3.1 is about 11% slower: see the 0.3.1 results).

Slice reads (one request each; the sum over a stored slice):

| Members | cold (first ever) | warm (memo) | fresh engine, validated | after one member's correction | memo per reader |
|---:|---:|---:|---:|---:|---:|
| 1,000 | 339 ms | 0.43 ms | 123 ms | 191 ms | 0.47 MB |
| 100,000 | 13.0 s | 0.58 ms | 7.9 s | 16.3 s | 46.7 MB |

A slice over a computed function of 1,000 members (each evaluated, signed and stored): 5.4 s cold, 0.54 ms warm, 453 ms validated by a fresh engine. (0.3.1 on store 0.3.2.) A stored slice read is one `slice_values` query (values, hashes and cells without receipt bytes) and, when the memo is on, one `slice_changed_after` query; 0.3.0 read the members' receipt bytes twice.

Licensed under Apache-2.0.
