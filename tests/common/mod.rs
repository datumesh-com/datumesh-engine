//! Helpers shared by the integration tests (included with `mod common;`).
//!
//! Every test gets a PostgreSQL database of its own, created from `DATABASE_URL` (a role
//! allowed to create databases) and dropped afterwards. A [`World`] holds the store, the key
//! documents of two publishers and of the mesh, a clock the test moves, and the functions it
//! defines; it builds engines on them.
#![allow(dead_code)]

pub mod mem;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use datumesh_core::json::{self, Value};
use datumesh_core::{
    verify, Arg, Args, Cell, CellRef, Descriptor, Keys, Level, Receipt, ReceiptBuilder, Resolver, Scalar, SigningKey,
    Type,
};
use datumesh_engine::native_test::NativeTest;
use datumesh_engine::{Answer, Budget, Clock, Engine, EngineBuilder, EvalError, Inputs, Runtime};
use datumesh_store::pg_unstable::postgres::{Client, Config, NoTls};
use datumesh_store::{migrate, PgStore, Store};

pub fn admin_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL names a PostgreSQL 16 database (see README)")
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A database of its own, migrated, dropped when the value is dropped.
pub struct TestDb {
    pub name: String,
    pub config: Config,
    admin: String,
}

impl TestDb {
    pub fn migrated() -> TestDb {
        let admin = admin_url();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let name = format!("dm_engine_{}_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst), nanos);
        let admin_config: Config = admin.parse().unwrap();
        let mut c = admin_config.connect(NoTls).unwrap();
        c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
        let mut config = admin_config.clone();
        config.dbname(&name);
        let db = TestDb { name, config, admin };
        migrate::apply(&db.store()).unwrap();
        db
    }

    pub fn url(&self) -> String {
        let (path, query) = match self.admin.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (self.admin.as_str(), None),
        };
        let (base, _) = path.rsplit_once('/').unwrap();
        match query {
            Some(q) => format!("{base}/{}?{q}", self.name),
            None => format!("{base}/{}", self.name),
        }
    }

    pub fn store(&self) -> PgStore {
        PgStore::connect(&self.url()).unwrap()
    }

    pub fn client(&self) -> Client {
        self.config.connect(NoTls).unwrap()
    }

    /// The number of rows of a table.
    pub fn count(&self, table: &str) -> i64 {
        self.client().query_one(&format!("SELECT count(*) FROM {table}"), &[]).unwrap().get(0)
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let admin: Config = self.admin.parse().unwrap();
        if let Ok(mut c) = admin.connect(NoTls) {
            let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name));
        }
    }
}

/// A clock the test sets.
#[derive(Clone)]
pub struct TestClock(Arc<Mutex<String>>);

impl TestClock {
    pub fn new(now: &str) -> TestClock {
        TestClock(Arc::new(Mutex::new(now.to_string())))
    }
    pub fn set(&self, now: &str) {
        *self.0.lock().unwrap() = now.to_string();
    }
}

impl Clock for TestClock {
    fn now(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

pub const MESH: &str = "mesh.example.com";
pub const RATES: &str = "rates.example.org";
pub const PRICES: &str = "prices.example.net";
pub const CALC: &str = "calc.example.net";
/// When the tests' functions are bound to their names.
pub const BIND_TX: &str = "2020-01-01T00:00:00.000000Z";
/// The engine's clock unless a test moves it.
pub const NOW: &str = "2026-01-01T00:00:00.000000Z";
pub const MIN_INSTANT: &str = "0001-01-01T00:00:00.000000Z";

pub fn key_for(host: &str) -> SigningKey {
    let seed: u8 = match host {
        RATES => 1,
        PRICES => 2,
        CALC => 3,
        _ => 9,
    };
    SigningKey::from_seed(&[seed; 32])
}

/// A key document publishing `key` for `host` from 2000 on (canonical JSON).
pub fn key_doc(host: &str, key: &SigningKey) -> Vec<u8> {
    let hex: String = key.public_key().iter().map(|b| format!("{b:02x}")).collect();
    format!(
        r#"{{"host":"{host}","keys":[{{"alg":"ed25519","id":"{}","public":"{hex}","valid_from":"2000-01-01T00:00:00.000000Z"}}],"proto":"datumesh/1"}}"#,
        key.key_id()
    )
    .into_bytes()
}

pub fn all_keys() -> Keys {
    let mut keys = Keys::new();
    for host in [MESH, RATES, PRICES, CALC] {
        keys.insert_response(host, 200, Some(&key_doc(host, &key_for(host))));
    }
    keys
}

/// The canonical JSON text of a whole receipt (payload plus `sig`).
pub fn receipt_json(r: &Receipt) -> Vec<u8> {
    let mut members = vec![("sig".to_string(), Value::String(r.sig().to_string()))];
    members.extend(json::parse(r.payload()).unwrap().as_object().unwrap().iter().cloned());
    Value::Object(members).to_jcs()
}

pub fn num(s: &str) -> Scalar {
    Scalar::from_lexical(Type::Number, s).unwrap()
}

pub fn text(s: &str) -> Scalar {
    Scalar::from_lexical(Type::String, s).unwrap()
}

pub fn args(pairs: &[(&str, Scalar)]) -> Args {
    pairs.iter().map(|(k, v)| (k.to_string(), Arg::Value(v.clone()))).collect()
}

/// `<ns>/<name>` split.
pub fn split(fn_name: &str) -> (&str, &str) {
    fn_name.rsplit_once('/').unwrap()
}

/// A number argument or return declaration (unit "1").
pub fn number_arg() -> &'static str {
    r#"{"type":"number","unit":"1","valid":false}"#
}

/// A descriptor: `args` maps argument names to declarations (JSON text), returns a number.
pub fn descriptor(lang: &str, body: &str, inputs: &[&str], arg_decls: &[(&str, &str)]) -> Vec<u8> {
    let mut inputs: Vec<&str> = inputs.to_vec();
    inputs.sort();
    inputs.dedup();
    let args: Vec<String> = {
        let mut a: Vec<(&str, &str)> = arg_decls.to_vec();
        a.sort();
        a.iter().map(|(k, d)| format!(r#""{k}":{d}"#)).collect()
    };
    let inputs: Vec<String> = inputs.iter().map(|i| format!(r#""{i}""#)).collect();
    let text = format!(
        r#"{{"code":{{"body":{},"lang":"{lang}"}},"inputs":[{}],"signature":{{"args":{{{}}},"returns":{{"type":"number","unit":"1"}}}}}}"#,
        Value::String(body.to_string()).to_jcs().iter().map(|&b| b as char).collect::<String>(),
        inputs.join(","),
        args.join(",")
    );
    let d = Descriptor::from_json(text.as_bytes()).unwrap();
    d.canonical()
}

pub type NativeClosure =
    Arc<dyn Fn(&Cell, &mut dyn Inputs, &mut Budget) -> Result<Scalar, EvalError> + Send + Sync + 'static>;

/// The store, keys, clock and functions of a test; builds engines on them.
pub struct World {
    pub db: Option<TestDb>,
    pub store: Arc<dyn Store + Send + Sync>,
    pub pg: Option<Arc<PgStore>>,
    pub clock: TestClock,
    /// The native functions, shared with every engine of this world (functions defined
    /// after an engine was built are seen by it).
    pub native: Arc<RwLock<NativeTest>>,
    pub engines: Mutex<Vec<Weak<Engine>>>,
    /// Calls of each native function, by function name.
    pub calls: Arc<Mutex<BTreeMap<String, Arc<AtomicUsize>>>>,
}

impl World {
    /// A world on a PostgreSQL database of its own.
    pub fn new() -> World {
        let db = TestDb::migrated();
        let pg = Arc::new(db.store());
        let store: Arc<dyn Store + Send + Sync> = pg.clone();
        World::on(store, Some(db), Some(pg))
    }

    /// A world on an in-memory store (tests/common/mem.rs).
    pub fn in_memory() -> World {
        // A Version to clone (only datumesh-store can make one): from a PostgreSQL store.
        let proto = {
            let pg = World::new();
            let h = pg.stored_fn("rates.example.org/proto", &[]);
            let cell = Cell { ns: RATES.into(), fn_name: "proto".into(), fn_hash: h, args: Args::new() };
            pg.publish(&cell, "1", &t(1, 0));
            (pg.store.latest(&cell.id()).unwrap().unwrap(), pg.store.namespace(MESH).unwrap().unwrap())
        };
        World::on(Arc::new(mem::MemStore::new(proto.0, proto.1)), None, None)
    }

    fn on(store: Arc<dyn Store + Send + Sync>, db: Option<TestDb>, pg: Option<Arc<PgStore>>) -> World {
        for host in [MESH, RATES, PRICES, CALC] {
            store.put_namespace(host, &key_doc(host, &key_for(host)), "2020-01-01T00:00:00.000000Z").unwrap();
        }
        World {
            db,
            store,
            pg,
            clock: TestClock::new(NOW),
            native: Arc::new(RwLock::new(NativeTest::new())),
            engines: Mutex::new(Vec::new()),
            calls: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn db(&self) -> &TestDb {
        self.db.as_ref().expect("a PostgreSQL world")
    }

    pub fn builder(&self) -> EngineBuilder {
        Engine::builder(self.store.clone(), MESH)
            .signing_key(key_for(MESH))
            .clock(Arc::new(self.clock.clone()))
            .runtime(Arc::new(SharedNative(self.native.clone())))
    }

    /// An engine with the default configuration, notified of every version `publish` adds.
    pub fn engine(&self) -> Arc<Engine> {
        self.engine_with(|b| b)
    }

    pub fn engine_with(&self, f: impl FnOnce(EngineBuilder) -> EngineBuilder) -> Arc<Engine> {
        let e = Arc::new(f(self.builder()).build().unwrap());
        self.engines.lock().unwrap().push(Arc::downgrade(&e));
        e
    }

    /// A stored function `<fn_name>` with the given argument declarations, bound at BIND_TX.
    pub fn stored_fn(&self, fn_name: &str, arg_decls: &[(&str, &str)]) -> String {
        let d = descriptor("stored", "", &[], arg_decls);
        let h = self.store.put_descriptor(&d).unwrap();
        self.store.bind_name(fn_name, &h, BIND_TX).unwrap();
        h
    }

    /// A native function `<fn_name>` reading `inputs`, bound at BIND_TX; its calls are
    /// counted under its name.
    pub fn native_fn(
        &mut self,
        fn_name: &str,
        inputs: &[&str],
        arg_decls: &[(&str, &str)],
        f: impl Fn(&Cell, &mut dyn Inputs, &mut Budget) -> Result<Scalar, EvalError> + Send + Sync + 'static,
    ) -> String {
        self.native_fn_body(fn_name, fn_name, inputs, arg_decls, f)
    }

    pub fn native_fn_body(
        &mut self,
        fn_name: &str,
        body: &str,
        inputs: &[&str],
        arg_decls: &[(&str, &str)],
        f: impl Fn(&Cell, &mut dyn Inputs, &mut Budget) -> Result<Scalar, EvalError> + Send + Sync + 'static,
    ) -> String {
        let d = descriptor("native_test", body, inputs, arg_decls);
        let h = self.store.put_descriptor(&d).unwrap();
        self.store.bind_name(fn_name, &h, BIND_TX).unwrap();
        let counter = self.calls.lock().unwrap().entry(fn_name.to_string()).or_default().clone();
        self.native.write().unwrap().register(h.clone(), move |c, i, b| {
            counter.fetch_add(1, Ordering::SeqCst);
            f(c, i, b)
        });
        h
    }

    /// The calls of native function `fn_name` so far.
    pub fn calls(&self, fn_name: &str) -> usize {
        self.calls.lock().unwrap().get(fn_name).map_or(0, |c| c.load(Ordering::SeqCst))
    }

    pub fn reset_calls(&self) {
        for c in self.calls.lock().unwrap().values() {
            c.store(0, Ordering::SeqCst);
        }
    }

    /// The cell of `fn_name` (resolved now) with `args`.
    pub fn cell(&self, fn_name: &str, a: &[(&str, Scalar)]) -> Cell {
        let (ns, f) = split(fn_name);
        let h = self.store.resolve_name(fn_name, "9999-12-31T23:59:59.999999Z").unwrap().unwrap();
        Cell { ns: ns.into(), fn_name: f.into(), fn_hash: h, args: args(a) }
    }

    pub fn cellref(&self, fn_name: &str, a: &[(&str, Scalar)]) -> CellRef {
        let (ns, f) = split(fn_name);
        CellRef { ns: ns.into(), fn_name: f.into(), args: args(a), asof: None }
    }

    /// A receipt of a stored cell signed by its publisher (the host of its namespace).
    pub fn sign_stored(&self, cell: &Cell, value: Scalar, tx: &str, asof: &str) -> Vec<u8> {
        let host = cell.ns.split('/').next().unwrap().to_string();
        let r = ReceiptBuilder::new()
            .cell(cell.clone())
            .value(value)
            .valid(Args::new())
            .asof(asof)
            .tx(tx)
            .issued(asof)
            .publisher(cell.ns.clone())
            .signer_host(host.clone())
            .licence("CC-BY-4.0")
            .summary(format!("A value of {}.", cell.function_name()))
            .sign(&key_for(&host))
            .unwrap();
        receipt_json(&r)
    }

    /// Publishes a stored version: signs, stores the receipt and the version, and tells every
    /// engine of this world. Returns the receipt hash.
    pub fn publish(&self, cell: &Cell, value: &str, tx: &str) -> String {
        self.publish_late(cell, value, tx, tx)
    }

    /// The same with `asof` later than `tx` (a late-signed receipt).
    pub fn publish_late(&self, cell: &Cell, value: &str, tx: &str, asof: &str) -> String {
        let bytes = self.sign_stored(cell, num(value), tx, asof);
        let h = self.store.put_receipt(&bytes).unwrap();
        self.store.put_version(&cell.id(), tx, &h).unwrap();
        self.notify(&cell.id());
        h
    }

    pub fn notify(&self, cell_id: &str) {
        for e in self.engines.lock().unwrap().iter().filter_map(Weak::upgrade) {
            e.stored_version_added(cell_id);
        }
    }

    pub fn stored_bytes(&self, hash: &str) -> Vec<u8> {
        self.store.receipt(hash).unwrap().into_owned()
    }

    /// Every receipt the mesh signed, in the store (PostgreSQL worlds).
    pub fn issued_receipts(&self) -> Vec<Vec<u8>> {
        let rows = self.db().client().query("SELECT bytes FROM receipts ORDER BY hash", &[]).unwrap();
        rows.iter()
            .map(|r| r.get::<_, Vec<u8>>(0))
            .filter(|b| Receipt::from_json(b).map(|r| r.signer_host() == MESH).unwrap_or(false))
            .collect()
    }

    /// Verifies every receipt the mesh signed at level recomputed, through `engine` as the
    /// resolver; with `DATUMESH_ENGINE_DUMP` set, also writes them, with the key documents, as
    /// JSON lines for the Python and JavaScript verifiers (tools/crosscheck.sh). Returns how
    /// many there were.
    pub fn verify_all_issued(&self, engine: &Engine, test: &str) -> usize {
        let receipts = self.issued_receipts();
        let keys = all_keys();
        for bytes in &receipts {
            let out = verify(bytes, &keys, Some(engine as &dyn Resolver), None);
            assert!(out.ok, "{test}: {out:?}\n{}", String::from_utf8_lossy(bytes));
            assert_eq!(out.level, Some(Level::Recomputed), "{test}: {out:?}\n{}", String::from_utf8_lossy(bytes));
        }
        dump(test, &receipts);
        receipts.len()
    }
}

/// The world's native functions as a runtime.
pub struct SharedNative(pub Arc<RwLock<NativeTest>>);

impl Runtime for SharedNative {
    fn lang(&self) -> &str {
        datumesh_engine::native_test::LANG
    }
    fn evaluate(&self, d: &Descriptor, c: &Cell, i: &mut dyn Inputs, b: &mut Budget) -> Result<Scalar, EvalError> {
        let rt = self.0.read().unwrap().clone();
        rt.evaluate(d, c, i, b)
    }
}

/// Writes receipts to `$DATUMESH_ENGINE_DUMP/<test>.jsonl`, each line
/// `{"hash": receipt hash, "keys": {host: key document text}, "receipt": receipt text}`.
pub fn dump(test: &str, receipts: &[Vec<u8>]) {
    let Ok(dir) = std::env::var("DATUMESH_ENGINE_DUMP") else { return };
    if dir.is_empty() {
        return;
    }
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = String::new();
    for bytes in receipts {
        let keys: Vec<(String, Value)> = [MESH, RATES, PRICES, CALC]
            .iter()
            .map(|h| {
                (h.to_string(), Value::String(String::from_utf8(key_doc(h, &key_for(h))).unwrap()))
            })
            .collect();
        let line = Value::Object(vec![
            ("hash".into(), Value::String(Receipt::from_json(bytes).unwrap().hash().to_string())),
            ("keys".into(), Value::Object(keys)),
            ("receipt".into(), Value::String(String::from_utf8(bytes.clone()).unwrap())),
        ]);
        out.push_str(&String::from_utf8(line.to_jcs()).unwrap());
        out.push('\n');
    }
    std::fs::write(format!("{dir}/{test}.jsonl"), out).unwrap();
}

/// The receipt of an answer (panics on NotCovered).
pub fn receipt_of(a: &Answer) -> Receipt {
    match a {
        Answer::Receipt(b) => Receipt::from_json(b).unwrap(),
        Answer::NotCovered => panic!("not covered"),
    }
}

pub fn bytes_of(a: &Answer) -> Vec<u8> {
    match a {
        Answer::Receipt(b) => b.clone(),
        Answer::NotCovered => panic!("not covered"),
    }
}

/// Runs `f` on a thread with the default 2 MiB stack and fails after `secs` seconds.
pub fn on_thread<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let _ = tx.send(f());
        })
        .unwrap();
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(v) => {
            h.join().unwrap();
            v
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("timed out after {secs} s"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match h.join() {
            Err(p) => std::panic::resume_unwind(p),
            Ok(()) => panic!("the thread ended without a result"),
        },
    }
}

/// An exact decimal sum of integers (no floats).
pub fn add(a: &Scalar, b: &Scalar) -> Scalar {
    let x: i128 = a.lexical().parse().unwrap();
    let y: i128 = b.lexical().parse().unwrap();
    num(&(x + y).to_string())
}

pub fn int(s: &Scalar) -> i128 {
    s.lexical().parse().unwrap()
}

/// Instant helpers: day `d` of 2024 at hour `h`.
pub fn t(day: u32, hour: u32) -> String {
    format!("2024-03-{day:02}T{hour:02}:00:00.000000Z")
}
