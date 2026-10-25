//! The early-cutoff case of the benchmark, alone and repeated, for profiling and for running
//! two engine versions back to back (it compiles against 0.2.0 too):
//!
//!     DATABASE_URL=postgres://… cargo run --release --features native-test --example cutoff [rounds]
//!
//! A 10,000-cell graph (100 layers of 100, each cell reading two below; layer 0 reads 100
//! stored cells; the top reads layer 99), built cold. Then `rounds` (default 30) corrections of
//! s(10000) by +2, which leave g(0, 0)'s value (a parity) unchanged: each drops about 5,000
//! answers, which the next `get` of the top rechecks from memory. Prints each round's time
//! from the new version to the correct answer, and p50 / mean.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use datumesh_core::{Arg, Args, Cell, CellRef, Descriptor, ReceiptBuilder, Scalar, SigningKey, Type};
use datumesh_engine::native_test::NativeTest;
use datumesh_engine::{Answer, Clock, Engine};
use datumesh_store::pg_unstable::postgres::{Config, NoTls};
use datumesh_store::{migrate, PgStore, Store};

const MESH: &str = "mesh.example.com";
const PUB: &str = "bench.example.org";
const S: &str = "bench.example.org/s";
const G: &str = "bench.example.org/g";
const TOP: &str = "bench.example.org/graph_top";
const N: &str = r#"{"type":"number","unit":"1","valid":false}"#;
const BIND: &str = "2020-01-01T00:00:00.000000Z";

struct Clockwork(Mutex<String>);

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

fn value(a: &Answer) -> i128 {
    match a {
        Answer::Receipt(b) => int(datumesh_core::Receipt::from_json(b).unwrap().value()),
        _ => panic!("not covered"),
    }
}

fn main() {
    let rounds: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let admin = std::env::var("DATABASE_URL").expect("DATABASE_URL names a PostgreSQL 16 server");
    let name = format!("dm_engine_cutoff_{}", std::process::id());
    let admin_config: Config = admin.parse().unwrap();
    admin_config.connect(NoTls).unwrap().batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
    let url = format!("{}/{name}", admin.rsplit_once('/').unwrap().0);
    let result = std::panic::catch_unwind(|| run(&url, rounds));
    let _ = admin_config
        .connect(NoTls)
        .map(|mut c| c.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")));
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

fn run(url: &str, rounds: usize) {
    let store = Arc::new(PgStore::connect(url).unwrap());
    migrate::apply(&*store).unwrap();
    let publisher = SigningKey::from_seed(&[1; 32]);
    let mesh = SigningKey::from_seed(&[9; 32]);
    store.put_namespace(PUB, &key_doc(PUB, &publisher), BIND).unwrap();
    store.put_namespace(MESH, &key_doc(MESH, &mesh), BIND).unwrap();
    let s_hash = store.put_descriptor(&descriptor("stored", "", &[], &[("k", N)])).unwrap();
    store.bind_name(S, &s_hash, BIND).unwrap();
    let mut native = NativeTest::new();
    let g_hash = store.put_descriptor(&descriptor("native_test", G, &[G, S], &[("j", N), ("l", N)])).unwrap();
    store.bind_name(G, &g_hash, BIND).unwrap();
    native.register(g_hash, |c, i, _| {
        let l = int(c.args["l"].scalar().unwrap());
        let j = int(c.args["j"].scalar().unwrap());
        if l == 0 {
            return Ok(num(int(&i.read(S, &args(&[("k", num(10_000 + j))]))?).rem_euclid(2)));
        }
        let a = int(&i.read(G, &args(&[("j", num(j)), ("l", num(l - 1))]))?);
        let b = int(&i.read(G, &args(&[("j", num((j + 1) % 100)), ("l", num(l - 1))]))?);
        Ok(num((a + b) % 1_000_003))
    });
    let top_hash = store.put_descriptor(&descriptor("native_test", TOP, &[G], &[])).unwrap();
    store.bind_name(TOP, &top_hash, BIND).unwrap();
    native.register(top_hash, |_, i, _| {
        let mut sum = 0;
        for j in 0..100 {
            sum += int(&i.read(G, &args(&[("j", num(j)), ("l", num(99))]))?);
        }
        Ok(num(sum))
    });
    let publish = |e: Option<&Engine>, j: i64, v: i128, tx: &str| {
        let cell = Cell {
            ns: PUB.into(),
            fn_name: "s".into(),
            fn_hash: s_hash.clone(),
            args: args(&[("k", num(10_000 + j))]),
        };
        let r = ReceiptBuilder::new()
            .cell(cell.clone())
            .value(num(v))
            .valid(Args::new())
            .asof(tx)
            .tx(tx)
            .issued(tx)
            .publisher(PUB)
            .signer_host(PUB)
            .licence("CC0-1.0")
            .summary("A benchmark value.")
            .sign(&publisher)
            .unwrap();
        let h = store.put_receipt(&receipt_json(&r)).unwrap();
        store.put_version(&cell.id(), tx, &h).unwrap();
        if let Some(e) = e {
            e.stored_version_added(&cell.id());
        }
    };
    for j in 0..100 {
        publish(None, j, j as i128, "2024-01-01T00:00:00.000000Z");
    }
    let clock = Arc::new(Clockwork(Mutex::new("2026-01-01T00:00:00.000000Z".into())));
    let shared: Arc<dyn Store + Send + Sync> = store.clone();
    let e = Engine::builder(shared, MESH)
        .signing_key(mesh)
        .runtime(Arc::new(native))
        .clock(clock.clone())
        .depth_limit(100_000)
        .build()
        .unwrap();
    let top = CellRef { ns: PUB.into(), fn_name: "graph_top".into(), args: Args::new(), asof: None };
    let t0 = Instant::now();
    let want = value(&e.get(&top, None).unwrap());
    println!("engine {}: graph built cold in {:.2} s", env!("CARGO_PKG_VERSION"), t0.elapsed().as_secs_f64());
    let mut times: Vec<Duration> = Vec::new();
    let mut current = 0i128;
    for k in 0..rounds {
        current += 2;
        let tx = format!("2024-02-{:02}T{:02}:00:00.000000Z", 1 + k / 24, k % 24);
        *clock.0.lock().unwrap() = format!("2026-02-{:02}T{:02}:00:00.000000Z", 1 + k / 24, k % 24);
        let t = Instant::now();
        publish(Some(&e), 0, current, &tx);
        assert_eq!(value(&e.get(&top, None).unwrap()), want);
        times.push(t.elapsed());
    }
    let ms: Vec<f64> = times.iter().map(|d| d.as_secs_f64() * 1e3).collect();
    let mut sorted = ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("rounds: {}", ms.iter().map(|m| format!("{m:.1}")).collect::<Vec<_>>().join(" "));
    println!(
        "engine {}: correction stopped at layer 0, until the top is correct: p50 {:.1} ms, mean {:.1} ms, min {:.1} ms ({} rounds)",
        env!("CARGO_PKG_VERSION"),
        sorted[sorted.len() / 2],
        ms.iter().sum::<f64>() / ms.len() as f64,
        sorted[0],
        rounds
    );
}
