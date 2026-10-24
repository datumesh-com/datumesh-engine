//! Counting the statements a `PgStore` runs, without a `Store` of our own (`Store` is sealed
//! since datumesh-store 0.3.0): a proxy between the store's connections and PostgreSQL that
//! reads the frontend protocol and counts each statement executed, by its SQL text.
//!
//! [`Wire::store`] gives a `PgStore` (through `pg_unstable::with_connector`) whose every
//! connection goes through the proxy. The extended protocol names the statement in `Parse`,
//! binds it to a portal in `Bind` and runs the portal in `Execute`, which is what is
//! counted; a simple `Query` counts its text. Bytes are forwarded unchanged.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use datumesh_store::pg_unstable::postgres::config::Host;
use datumesh_store::pg_unstable::postgres::{Config, NoTls};
use datumesh_store::{pg_unstable, PgStore, PoolOptions};

type Counts = Arc<Mutex<BTreeMap<String, usize>>>;

pub struct Wire {
    port: u16,
    counts: Counts,
    config: Config,
}

impl Wire {
    /// A proxy to the server of `config` (a TCP host), on a port of 127.0.0.1.
    pub fn start(config: &Config) -> Wire {
        let host = match config.get_hosts().first() {
            Some(Host::Tcp(h)) => h.clone(),
            other => panic!("the wire counter needs a TCP host, not {other:?}"),
        };
        let port = config.get_ports().first().copied().unwrap_or(5432);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let local = listener.local_addr().unwrap().port();
        let counts: Counts = Arc::default();
        let c = counts.clone();
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let Ok(server) = TcpStream::connect((host.as_str(), port)) else { continue };
                let (c2s_from, c2s_to) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                let counts = c.clone();
                std::thread::spawn(move || {
                    let _ = pump(c2s_from, &c2s_to, &counts);
                    let _ = c2s_to.shutdown(Shutdown::Write);
                });
                std::thread::spawn(move || {
                    let (mut from, mut to) = (server, client);
                    let _ = io::copy(&mut from, &mut to);
                    let _ = to.shutdown(Shutdown::Write);
                });
            }
        });
        Wire { port: local, counts, config: config.clone() }
    }

    /// A store on the same database whose connections go through the proxy.
    pub fn store(&self) -> PgStore {
        let port = self.port;
        let user = self.config.get_user().map(str::to_string);
        let password = self.config.get_password().map(<[u8]>::to_vec);
        let dbname = self.config.get_dbname().map(str::to_string);
        pg_unstable::with_connector(
            move || {
                let mut c = Config::new();
                c.host("127.0.0.1").port(port);
                if let Some(u) = &user {
                    c.user(u);
                }
                if let Some(p) = &password {
                    c.password(p);
                }
                if let Some(d) = &dbname {
                    c.dbname(d);
                }
                c.connect(NoTls)
            },
            PoolOptions::new(),
        )
        .unwrap()
    }

    /// The statements executed since the last `take`, by SQL text (whitespace collapsed).
    pub fn take(&self) -> BTreeMap<String, usize> {
        std::mem::take(&mut *self.counts.lock().unwrap())
    }
}

/// How many of `counts`' statements contain `needle`.
pub fn matching(counts: &BTreeMap<String, usize>, needle: &str) -> usize {
    counts.iter().filter(|(sql, _)| sql.contains(needle)).map(|(_, n)| n).sum()
}

/// The text of a NUL-terminated string at the start of `b`, and the rest.
fn cstr(b: &[u8]) -> (String, &[u8]) {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    (String::from_utf8_lossy(&b[..end]).into_owned(), b.get(end + 1..).unwrap_or(&[]))
}

fn normal(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Forwards the client's messages to the server, counting what is executed.
fn pump(from: TcpStream, to: &TcpStream, counts: &Counts) -> io::Result<()> {
    let mut to = to;
    let mut r = BufReader::new(from);
    // The startup message (no type byte); an SSL or GSS encryption request comes first.
    loop {
        let mut len = [0u8; 4];
        r.read_exact(&mut len)?;
        let mut body = vec![0u8; (u32::from_be_bytes(len) as usize).saturating_sub(4)];
        r.read_exact(&mut body)?;
        to.write_all(&len)?;
        to.write_all(&body)?;
        let code = body.get(..4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).unwrap_or(0);
        if code != 80_877_103 && code != 80_877_104 {
            break;
        }
    }
    let mut statements: HashMap<String, String> = HashMap::new();
    let mut portals: HashMap<String, String> = HashMap::new();
    loop {
        let mut head = [0u8; 5];
        if r.read_exact(&mut head).is_err() {
            return Ok(());
        }
        let mut body = vec![0u8; (u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize).saturating_sub(4)];
        r.read_exact(&mut body)?;
        match head[0] {
            b'P' => {
                let (name, rest) = cstr(&body);
                statements.insert(name, normal(&cstr(rest).0));
            }
            b'B' => {
                let (portal, rest) = cstr(&body);
                portals.insert(portal, cstr(rest).0);
            }
            b'E' => {
                let (portal, _) = cstr(&body);
                let sql = portals.get(&portal).and_then(|s| statements.get(s)).cloned().unwrap_or_default();
                *counts.lock().unwrap().entry(sql).or_default() += 1;
            }
            b'Q' => {
                *counts.lock().unwrap().entry(normal(&cstr(&body).0)).or_default() += 1;
            }
            _ => {}
        }
        to.write_all(&head)?;
        to.write_all(&body)?;
    }
}
