//! Benchmark of the engine against PostgreSQL (datumesh-pg): numbers only, no thresholds.
//!
//!     DATABASE_URL=postgres://… cargo run --release --features native-test --example bench
//!
//! It makes a database of its own on that server (`dm_engine_bench_<pid>`, dropped at the
//! end), and measures, through `Engine` on a `PgStore`, one call at a time:
//!
//! * warm `get` (memoised), stored and computed, by reference (the name resolves per
//!   request: one query) and by pinned cell;
//! * cold `get` (empty memo, nothing computed yet) of a diamond (4 computed cells over one
//!   stored cell) and of a 1,000-deep chain;
//! * one input correction under a 10,000-cell dependent graph (100 layers of 100 cells,
//!   each reading two of the layer below; layer 0 reads 100 stored cells; the top reads layer
//!   99): the time from the new version to the next correct `get` of the top, when the
//!   correction changes values all the way up, and when early cutoff stops it at layer 0;
//! * signing one receipt (`ReceiptBuilder::sign` and its JSON), and storing one
//!   (`put_receipt`, which verifies it, plus `put_computed`);
//! * the memo and dirty set's sizes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use datumesh_core::{Arg, Args, Cell, CellRef, Descriptor, ReceiptBuilder, Scalar, SigningKey, Type};
use datumesh_engine::native_test::NativeTest;
use datumesh_engine::{Answer, Budget, Clock, Engine, EvalError, Inputs};
use datumesh_store::pg_unstable::postgres::{Config, NoTls};
use datumesh_store::{migrate, PgStore, Store};

const MESH: &str = "mesh.example.com";
const PUB: &str = "bench.example.org";
const S: &str = "bench.example.org/s";
const N: &str = r#"{"type":"number","unit":"1","valid":false}"#;
const BIND: &str = "2020-01-01T00:00:00.000000Z";

struct Clockwork(std::sync::Mutex<String>);

impl Clock for Clockwork {
    fn now(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

fn num(s: impl ToString) -> Scalar {
    Scalar::from_lexical(Type::Number, &s.to_string()).unwrap()
}

fn int(s: &Scalar) -> i128 {
    s.lexical().parse().unwrap()
}

fn args(pairs: &[(&str, Scalar)]) -> Args {
    pairs.iter().map(|(k, v)| (k.to_string(), Arg::Value(v.clone()))).collect()
}

fn key_doc(host: &str, key: &SigningKey) -> Vec<u8> {
    let hex: String = key.public_key().iter().map(|b| format!("{b:02x}")).collect();
    format!(
        r#"{{"host":"{host}","keys":[{{"alg":"ed25519","id":"{}","public":"{hex}","valid_from":"2000-01-01T00:00:00.000000Z"}}],"proto":"datumesh/1"}}"#,
        key.key_id()
    )
    .into_bytes()
}

fn descriptor(lang: &str, body: &str, inputs: &[&str], arg_decls: &[(&str, &str)]) -> Vec<u8> {
    let mut inputs: Vec<&str> = inputs.to_vec();
    inputs.sort();
    inputs.dedup();
    let a: Vec<String> = arg_decls.iter().map(|(k, d)| format!(r#""{k}":{d}"#)).collect();
    let i: Vec<String> = inputs.iter().map(|x| format!(r#""{x}""#)).collect();
    let text = format!(
        r#"{{"code":{{"body":"{body}","lang":"{lang}"}},"inputs":[{}],"signature":{{"args":{{{}}},"returns":{{"type":"number","unit":"1"}}}}}}"#,
        i.join(","),
        a.join(",")
    );
    Descriptor::from_json(text.as_bytes()).unwrap().canonical()
}

fn receipt_json(r: &datumesh_core::Receipt) -> Vec<u8> {
    use datumesh_core::json::{self, Value};
    let mut m = vec![("sig".to_string(), Value::String(r.sig().to_string()))];
    m.extend(json::parse(r.payload()).unwrap().as_object().unwrap().iter().cloned());
    Value::Object(m).to_jcs()
}

struct Bench {
    store: Arc<PgStore>,
    publisher: SigningKey,
    native: NativeTest,
    clock: Arc<Clockwork>,
    s_hash: String,
}

impl Bench {
    fn engine(&self) -> Engine {
        let store: Arc<dyn Store + Send + Sync> = self.store.clone();
        Engine::builder(store, MESH)
            .signing_key(SigningKey::from_seed(&[9; 32]))
            .runtime(Arc::new(self.native.clone()))
            .clock(self.clock.clone())
            .depth_limit(100_000)
            .build()
            .unwrap()
    }

    fn function(
        &mut self,
        name: &str,
        inputs: &[&str],
        arg_decls: &[(&str, &str)],
        f: impl Fn(&Cell, &mut dyn Inputs, &mut Budget) -> Result<Scalar, EvalError> + Send + Sync + 'static,
    ) {
        let h = self.store.put_descriptor(&descriptor("native_test", name, inputs, arg_decls)).unwrap();
        self.store.bind_name(name, &h, BIND).unwrap();
        self.native.register(h, f);
    }

    fn s_cell(&self, k: i64) -> Cell {
        Cell { ns: PUB.into(), fn_name: "s".into(), fn_hash: self.s_hash.clone(), args: args(&[("k", num(k))]) }
    }

    fn publish(&self, engine: Option<&Engine>, k: i64, value: i128, tx: &str) {
        let cell = self.s_cell(k);
        let r = ReceiptBuilder::new()
            .cell(cell.clone())
            .value(num(value))
            .valid(Args::new())
            .asof(tx)
            .tx(tx)
            .issued(tx)
            .publisher(PUB)
            .signer_host(PUB)
            .licence("CC0-1.0")
            .summary("A benchmark value.")
            .sign(&self.publisher)
            .unwrap();
        let h = self.store.put_receipt(&receipt_json(&r)).unwrap();
        self.store.put_version(&cell.id(), tx, &h).unwrap();
        if let Some(e) = engine {
            e.stored_version_added(&cell.id());
        }
    }
}

fn reference(name: &str, a: &[(&str, Scalar)]) -> CellRef {
    let (ns, f) = name.rsplit_once('/').unwrap();
    CellRef { ns: ns.into(), fn_name: f.into(), args: args(a), asof: None }
}

fn pinned(store: &PgStore, name: &str, a: &[(&str, Scalar)]) -> Cell {
    let (ns, f) = name.rsplit_once('/').unwrap();
    let h = store.resolve_name(name, "9999-12-31T23:59:59.999999Z").unwrap().unwrap();
    Cell { ns: ns.into(), fn_name: f.into(), fn_hash: h, args: args(a) }
}

fn value(a: &Answer) -> Scalar {
    match a {
        Answer::Receipt(b) => datumesh_core::Receipt::from_json(b).unwrap().value().clone(),
        _ => panic!("not covered"),
    }
}

struct Stats(Vec<Duration>);

impl Stats {
    fn of(mut v: Vec<Duration>) -> Stats {
        v.sort();
        Stats(v)
    }
    fn p(&self, q: f64) -> Duration {
        self.0[((self.0.len() as f64 - 1.0) * q).round() as usize]
    }
    fn mean(&self) -> Duration {
        self.0.iter().sum::<Duration>() / self.0.len() as u32
    }
    fn row(&self, what: &str) {
        println!("| {what} | {} | {} | {} | {} |", self.0.len(), fmt(self.p(0.5)), fmt(self.p(0.99)), fmt(self.mean()));
    }
}

fn fmt(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us >= 1e6 {
        format!("{:.2} s", us / 1e6)
    } else if us >= 1e3 {
        format!("{:.2} ms", us / 1e3)
    } else {
        format!("{us:.1} µs")
    }
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let v = f();
    (v, t.elapsed())
}

fn main() {
    let admin = std::env::var("DATABASE_URL").expect("DATABASE_URL names a PostgreSQL 16 server");
    let name = format!("dm_engine_bench_{}", std::process::id());
    let admin_config: Config = admin.parse().unwrap();
    admin_config.connect(NoTls).unwrap().batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
    let url = match admin.rsplit_once('/') {
        Some((base, _)) => format!("{base}/{name}"),
        None => panic!("DATABASE_URL has no database"),
    };
    let result = std::panic::catch_unwind(|| run(&url));
    let _ = admin_config
        .connect(NoTls)
        .map(|mut c| c.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")));
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

fn run(url: &str) {
    let store = Arc::new(PgStore::connect(url).unwrap());
    migrate::apply(&store).unwrap();
    let publisher = SigningKey::from_seed(&[1; 32]);
    let mesh = SigningKey::from_seed(&[9; 32]);
    store.put_namespace(PUB, &key_doc(PUB, &publisher), "2020-01-01T00:00:00.000000Z").unwrap();
    store.put_namespace(MESH, &key_doc(MESH, &mesh), "2020-01-01T00:00:00.000000Z").unwrap();
    let s_hash = store.put_descriptor(&descriptor("stored", "", &[], &[("k", N)])).unwrap();
    store.bind_name(S, &s_hash, BIND).unwrap();
    let clock = Arc::new(Clockwork(std::sync::Mutex::new("2026-01-01T00:00:00.000000Z".into())));
    let mut b = Bench { store: store.clone(), publisher, native: NativeTest::new(), clock: clock.clone(), s_hash };

    // f(k) = s(k) + 1.
    b.function("bench.example.org/f", &[S], &[("k", N)], |c, i, _| {
        Ok(num(int(&i.read(S, &args(&[("k", c.args["k"].scalar().unwrap().clone())]))?) + 1))
    });
    // A diamond over s(k): top = left + right, left = shared + 1, right = shared + 2, shared = s(k) * 2.
    b.function("bench.example.org/shared", &[S], &[("k", N)], |c, i, _| {
        Ok(num(2 * int(&i.read(S, &args(&[("k", c.args["k"].scalar().unwrap().clone())]))?)))
    });
    for (name, plus) in [("bench.example.org/left", 1), ("bench.example.org/right", 2)] {
        b.function(name, &["bench.example.org/shared"], &[("k", N)], move |c, i, _| {
            Ok(num(int(&i.read("bench.example.org/shared", &args(&[("k", c.args["k"].scalar().unwrap().clone())]))?)
                + plus))
        });
    }
    b.function(
        "bench.example.org/top",
        &["bench.example.org/left", "bench.example.org/right"],
        &[("k", N)],
        |c, i, _| {
            let k = c.args["k"].scalar().unwrap().clone();
            Ok(num(int(&i.read("bench.example.org/left", &args(&[("k", k.clone())]))?)
                + int(&i.read("bench.example.org/right", &args(&[("k", k)]))?)))
        },
    );
    // chain(n, k) = chain(n - 1, k) + 1, chain(0, k) = s(k).
    b.function("bench.example.org/chain", &["bench.example.org/chain", S], &[("k", N), ("n", N)], |c, i, _| {
        let k = c.args["k"].scalar().unwrap().clone();
        let n = int(c.args["n"].scalar().unwrap());
        if n == 0 {
            return i.read(S, &args(&[("k", k)]));
        }
        Ok(num(int(&i.read("bench.example.org/chain", &args(&[("k", k), ("n", num(n - 1))]))?) + 1))
    });
    // The graph: g(l, j) for 100 layers of 100: g(0, j) = s(10000 + j) mod 2 (so a change of
    // parity changes it, a change by 2 does not); g(l, j) = (g(l-1, j) + g(l-1, j+1 mod 100))
    // mod 1000003; top() = the sum of layer 99.
    b.function("bench.example.org/g", &["bench.example.org/g", S], &[("j", N), ("l", N)], |c, i, _| {
        let l = int(c.args["l"].scalar().unwrap());
        let j = int(c.args["j"].scalar().unwrap());
        if l == 0 {
            return Ok(num(int(&i.read(S, &args(&[("k", num(10_000 + j))]))?).rem_euclid(2)));
        }
        let a = int(&i.read("bench.example.org/g", &args(&[("j", num(j)), ("l", num(l - 1))]))?);
        let b2 = int(&i.read("bench.example.org/g", &args(&[("j", num((j + 1) % 100)), ("l", num(l - 1))]))?);
        Ok(num((a + b2) % 1_000_003))
    });
    b.function("bench.example.org/graph_top", &["bench.example.org/g"], &[], |_, i, _| {
        let mut sum = 0;
        for j in 0..100 {
            sum += int(&i.read("bench.example.org/g", &args(&[("j", num(j)), ("l", num(99))]))?);
        }
        Ok(num(sum))
    });

    // Stored data: s(0..1000) for the warm and diamond runs, s(10000..10100) for the graph.
    let (_, load) = time(|| {
        for k in 0..1000 {
            b.publish(None, k, k as i128, "2024-01-01T00:00:00.000000Z");
        }
        for j in 0..100 {
            b.publish(None, 10_000 + j, j as i128, "2024-01-01T00:00:00.000000Z");
        }
    });
    println!("Setup: 1,100 stored cells (one version each) in {}.\n", fmt(load));
    println!("| Operation | samples | p50 | p99 | mean |");
    println!("|---|---:|---:|---:|---:|");

    let e = b.engine();
    let refs_s: Vec<CellRef> = (0..1000).map(|k| reference(S, &[("k", num(k))])).collect();
    let cells_s: Vec<Cell> = (0..1000).map(|k| b.s_cell(k)).collect();
    let refs_f: Vec<CellRef> = (0..1000).map(|k| reference("bench.example.org/f", &[("k", num(k))])).collect();
    let cells_f: Vec<Cell> = (0..1000).map(|k| pinned(&store, "bench.example.org/f", &[("k", num(k))])).collect();
    // Cold, to fill the memo (and the computed versions of f).
    let cold_stored = Stats::of(refs_s.iter().map(|r| time(|| e.get(r, None).unwrap()).1).collect());
    let cold_f = Stats::of(refs_f.iter().map(|r| time(|| e.get(r, None).unwrap()).1).collect());
    let mut rng = 0x2545_f491_4f6c_dd1du64;
    let mut pick = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng % 1000) as usize
    };
    let samples = 10_000;
    let warm = |f: &mut dyn FnMut() -> Duration| Stats::of((0..samples).map(|_| f()).collect());
    warm(&mut || time(|| e.get(&refs_s[pick()], None).unwrap()).1)
        .row("warm `get` stored, by reference (name resolved: 1 query)");
    warm(&mut || time(|| e.get_cell(&cells_s[pick()], None).unwrap()).1)
        .row("warm `get_cell` stored (pinned: no query)");
    warm(&mut || time(|| e.get(&refs_f[pick()], None).unwrap()).1)
        .row("warm `get` computed f = s + 1, by reference (2 queries: f's name, s's name)");
    warm(&mut || time(|| e.get_cell(&cells_f[pick()], None).unwrap()).1)
        .row("warm `get_cell` computed f (1 query: s's name)");
    cold_stored.row("cold `get` stored (empty memo: resolve_name, stored_asof_with_receipt, versions)");
    cold_f.row("cold `get` computed f, first ever (computed_asof, s, sign, put_receipt, put_computed)");
    // A fresh engine on the same store: f's versions are validated, not recomputed.
    let fresh = b.engine();
    Stats::of(refs_f.iter().map(|r| time(|| fresh.get(r, None).unwrap()).1).collect())
        .row("`get` computed f, empty memo, version in the store (validated)");

    // Cold diamonds: 100 distinct k, nothing computed before.
    Stats::of(
        (0..100)
            .map(|k| time(|| e.get(&reference("bench.example.org/top", &[("k", num(500 + k))]), None).unwrap()).1)
            .collect(),
    )
    .row("cold `get` of a diamond (4 computed cells over 1 stored)");
    // Cold 1,000-deep chains: 5 distinct k.
    let chains: Vec<Duration> = (0..5)
        .map(|k| {
            let r = reference("bench.example.org/chain", &[("k", num(k)), ("n", num(999))]);
            let (a, d) = time(|| e.get(&r, None).unwrap());
            assert_eq!(int(&value(&a)), k as i128 + 999);
            d
        })
        .collect();
    Stats::of(chains).row("cold `get` of a 1,000-deep chain (1,000 computed cells)");

    // Signing and storing one receipt.
    let mut signing = Vec::new();
    let mut storing = Vec::new();
    let cell = pinned(&store, "bench.example.org/f", &[("k", num(424242))]);
    for i in 0..1000 {
        let issued = format!("2026-01-01T00:00:{:02}.{:06}Z", i / 1000, i % 1000);
        let (bytes, d) = time(|| {
            let r = ReceiptBuilder::new()
                .cell(cell.clone())
                .value(num(i))
                .valid(Args::new())
                .asof("2024-01-01T00:00:00.000000Z")
                .tx("2024-01-01T00:00:00.000000Z")
                .issued(issued.clone())
                .publisher(PUB)
                .signer_host(MESH)
                .licence("CC-BY-4.0")
                .summary("A benchmark value.")
                .sign(&mesh)
                .unwrap();
            (receipt_json(&r), r.hash().to_string())
        });
        signing.push(d);
        storing.push(time(|| store.put_receipt(&bytes.0).unwrap()).1);
    }
    Stats::of(signing).row("sign one receipt (ReceiptBuilder::sign + JSON)");
    Stats::of(storing).row("store one receipt (put_receipt, verified by the store)");

    // The 10,000-cell graph.
    let ge = b.engine();
    let top = reference("bench.example.org/graph_top", &[]);
    clock.0.lock().unwrap().replace_range(.., "2026-01-02T00:00:00.000000Z");
    let (a0, build) = time(|| ge.get(&top, None).unwrap());
    let before = ge.memo_stats();
    println!("\nGraph: 10,001 computed cells (100 layers x 100, each reading 2 below; layer 0 reads 100 stored cells; the top reads layer 99). Cold build: {}.", fmt(build));
    println!(
        "Memo after the build (and the runs above, same engine type, separate instance): {} answers for {} cells, {} dependents edges, dirty set {}, dropped answers {}, about {:.1} MB.",
        before.entries,
        before.cells,
        before.dependent_edges,
        before.dirty,
        before.stale,
        before.approx_bytes as f64 / 1e6
    );
    let mut rows = Vec::new();
    for (case, delta, tx) in [
        ("changes every layer above it (s(10000) parity flips)", 1, "2024-06-01T00:00:00.000000Z"),
        ("stopped at layer 0 by early cutoff (s(10000) + 2: same parity)", 2, "2024-07-01T00:00:00.000000Z"),
    ] {
        let old = int(&value(&ge.get(&top, None).unwrap()));
        let current = int(&value(&ge.get(&reference(S, &[("k", num(10_000))]), None).unwrap()));
        clock.0.lock().unwrap().replace_range(.., &format!("2026-{}-01T00:00:00.000000Z", &tx[5..7]));
        let t0 = Instant::now();
        b.publish(Some(&ge), 10_000, current + delta, tx);
        let notified = t0.elapsed();
        let a = ge.get(&top, None).unwrap();
        let total = t0.elapsed();
        let stats = ge.memo_stats();
        rows.push(format!(
            "| {case} | {} | {} | {} | {} | {} -> {} |",
            fmt(notified),
            fmt(total),
            stats.entries,
            stats.stale,
            old,
            int(&value(&a))
        ));
    }
    println!("\n| Correction of s(10000) under the graph | publish + notify | until the next `get` of the top is correct | memo answers after | dropped answers left | top value |");
    println!("|---|---:|---:|---:|---:|---:|");
    for r in rows {
        println!("{r}");
    }
    let _ = a0;
    println!(
        "\nThe dirty set holds marks only while an evaluation that began before them runs; at rest it is {}.",
        ge.memo_stats().dirty
    );
    println!(
        "Rows: receipts {}, computed_versions {}, versions {}.",
        store_count(url, "receipts"),
        store_count(url, "computed_versions"),
        store_count(url, "versions")
    );
}

fn store_count(url: &str, table: &str) -> i64 {
    let config: Config = url.parse().unwrap();
    config.connect(NoTls).unwrap().query_one(&format!("SELECT count(*) FROM {table}"), &[]).unwrap().get(0)
}
