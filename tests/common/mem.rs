//! An in-memory `Store`, for the tests whose size makes PostgreSQL too slow (the 100,000-deep
//! chain). It follows `PgStore`'s documented behaviour for what the engine uses: receipts
//! verified against stored key documents and kept canonical, append-only stored versions,
//! insert-only computed versions keyed on (cell, tx, asof), answering as of A with both
//! times, names in tx order. Not a conformance target: the PostgreSQL tests are.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

use datumesh_core::json;
use datumesh_core::{keys_from_response, verify, Cell, Descriptor, Keys, Receipt, Resolver, Scalar};
use datumesh_store::{Error, Namespace, Result, Store, Version};

#[derive(Default)]
struct Inner {
    descriptors: BTreeMap<String, Vec<u8>>,
    receipts: BTreeMap<String, (Vec<u8>, Receipt)>,
    versions: BTreeMap<String, Vec<Version>>,
    computed: BTreeMap<String, Vec<Version>>,
    edges: BTreeMap<String, BTreeSet<String>>,
    namespaces: BTreeMap<String, Namespace>,
    names: BTreeMap<String, Vec<(String, String)>>,
    blobs: BTreeMap<String, Vec<u8>>,
}

pub struct MemStore {
    inner: RwLock<Inner>,
    /// `Version` is `#[non_exhaustive]`: only datumesh-store makes one, so this store clones
    /// one a `PgStore` returned and sets its fields.
    proto: Version,
    proto_ns: Namespace,
}

impl MemStore {
    pub fn new(proto: Version, proto_ns: Namespace) -> MemStore {
        MemStore { inner: RwLock::new(Inner::default()), proto, proto_ns }
    }

    fn version(&self, cell_id: &str, tx: &str, receipt_hash: &str, asof: &str) -> Version {
        let mut v = self.proto.clone();
        v.cell_id = cell_id.into();
        v.tx = tx.into();
        v.receipt_hash = receipt_hash.into();
        v.asof = asof.into();
        v
    }
}

fn not_found(what: &'static str, key: &str) -> Error {
    Error::NotFound { what, key: key.to_string() }
}

impl Resolver for MemStore {
    fn descriptor(&self, fn_hash: &str) -> Option<Cow<'_, [u8]>> {
        self.inner.read().unwrap().descriptors.get(fn_hash).map(|b| Cow::Owned(b.clone()))
    }
    fn receipt(&self, hash: &str) -> Option<Cow<'_, [u8]>> {
        self.inner.read().unwrap().receipts.get(hash).map(|(b, _)| Cow::Owned(b.clone()))
    }
    fn evaluate(&self, cell: &Cell, asof: &str) -> Option<Scalar> {
        let v = self.version_asof(&cell.id(), asof).ok()??;
        self.inner.read().unwrap().receipts.get(&v.receipt_hash).map(|(_, r)| r.value().clone())
    }
}

impl Store for MemStore {
    fn put_descriptor(&self, bytes: &[u8]) -> Result<String> {
        let d = Descriptor::from_json(bytes)?;
        let h = d.hash();
        self.inner.write().unwrap().descriptors.insert(h.clone(), d.canonical());
        Ok(h)
    }

    fn put_receipt(&self, bytes: &[u8]) -> Result<String> {
        let r = Receipt::from_json(bytes)?;
        let canonical = json::parse(bytes)?.to_jcs();
        let mut g = self.inner.write().unwrap();
        if let Some((stored, _)) = g.receipts.get(r.hash()) {
            if *stored == canonical {
                return Ok(r.hash().to_string());
            }
        }
        let mut keys = Keys::new();
        if let Some(ns) = g.namespaces.get(r.signer_host()) {
            keys.insert_response(&ns.host, 200, Some(&ns.keydoc));
        }
        let out = verify(&canonical, &keys, None, None);
        if !out.ok {
            return Err(Error::Invalid(out.error().unwrap()));
        }
        if g.receipts.contains_key(r.hash()) {
            return Err(Error::Conflict { hash: r.hash().to_string() });
        }
        for i in r.inputs() {
            g.edges.entry(i.clone()).or_default().insert(r.hash().to_string());
        }
        let h = r.hash().to_string();
        g.receipts.insert(h.clone(), (canonical, r));
        Ok(h)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let h = datumesh_core::sha256_hex(bytes);
        self.inner.write().unwrap().blobs.insert(h.clone(), bytes.to_vec());
        Ok(h)
    }

    fn blob(&self, sha256: &str) -> Result<Option<Cow<'_, [u8]>>> {
        Ok(self.inner.read().unwrap().blobs.get(sha256).map(|b| Cow::Owned(b.clone())))
    }

    fn put_version(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()> {
        let mut g = self.inner.write().unwrap();
        let asof = check(&g, cell_id, tx, receipt_hash)?;
        let list = g.versions.entry(cell_id.to_string()).or_default();
        if let Some(last) = list.last() {
            if last.tx.as_str() >= tx {
                return Err(Error::NotLater {
                    what: "version",
                    key: cell_id.into(),
                    given: tx.into(),
                    latest: last.tx.clone(),
                });
            }
        }
        list.push(self.version(cell_id, tx, receipt_hash, &asof));
        Ok(())
    }

    fn version_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        let g = self.inner.read().unwrap();
        Ok(g.versions.get(cell_id).and_then(|l| l.iter().rev().find(|v| v.tx.as_str() <= asof).cloned()))
    }

    fn versions(&self, cell_id: &str, from: &str, to: &str) -> Result<Vec<Version>> {
        let g = self.inner.read().unwrap();
        Ok(g.versions
            .get(cell_id)
            .map(|l| l.iter().filter(|v| v.tx.as_str() >= from && v.tx.as_str() <= to).cloned().collect())
            .unwrap_or_default())
    }

    fn latest(&self, cell_id: &str) -> Result<Option<Version>> {
        Ok(self.inner.read().unwrap().versions.get(cell_id).and_then(|l| l.last().cloned()))
    }

    fn dependents(&self, receipt_hash: &str) -> Result<Vec<String>> {
        let g = self.inner.read().unwrap();
        Ok(g.edges.get(receipt_hash).map(|s| s.iter().cloned().collect()).unwrap_or_default())
    }

    fn inputs(&self, receipt_hash: &str) -> Result<Vec<String>> {
        let g = self.inner.read().unwrap();
        Ok(g.receipts.get(receipt_hash).map(|(_, r)| r.inputs().to_vec()).unwrap_or_default())
    }

    fn put_namespace(&self, host: &str, keydoc_bytes: &[u8], validated_at: &str) -> Result<()> {
        let doc = keys_from_response(host, 200, Some(keydoc_bytes))?;
        let _ = doc;
        let keydoc = json::parse(keydoc_bytes)?.to_jcs();
        let mut ns = self.proto_ns.clone();
        ns.host = host.into();
        ns.keydoc = keydoc;
        ns.validated_at = validated_at.into();
        ns.suspended_at = None;
        self.inner.write().unwrap().namespaces.insert(host.to_string(), ns);
        Ok(())
    }

    fn namespace(&self, host: &str) -> Result<Option<Namespace>> {
        Ok(self.inner.read().unwrap().namespaces.get(host).cloned())
    }

    fn bind_name(&self, fn_name: &str, fn_hash: &str, tx: &str) -> Result<()> {
        let mut g = self.inner.write().unwrap();
        if !g.descriptors.contains_key(fn_hash) {
            return Err(not_found("descriptor", fn_hash));
        }
        let list = g.names.entry(fn_name.to_string()).or_default();
        if let Some((last, _)) = list.last() {
            if last.as_str() >= tx {
                return Err(Error::NotLater { what: "name", key: fn_name.into(), given: tx.into(), latest: last.clone() });
            }
        }
        list.push((tx.to_string(), fn_hash.to_string()));
        Ok(())
    }

    fn resolve_name(&self, fn_name: &str, asof: &str) -> Result<Option<String>> {
        let g = self.inner.read().unwrap();
        Ok(g.names.get(fn_name).and_then(|l| l.iter().rev().find(|(tx, _)| tx.as_str() <= asof).map(|(_, h)| h.clone())))
    }

    fn put_computed(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()> {
        let mut g = self.inner.write().unwrap();
        let asof = check(&g, cell_id, tx, receipt_hash)?;
        let list = g.computed.entry(cell_id.to_string()).or_default();
        if let Some(v) = list.iter().find(|v| v.tx == tx && v.asof == asof) {
            if v.receipt_hash == receipt_hash {
                return Ok(());
            }
            return Err(Error::Conflict { hash: receipt_hash.into() });
        }
        list.push(self.version(cell_id, tx, receipt_hash, &asof));
        Ok(())
    }

    fn computed_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        let g = self.inner.read().unwrap();
        Ok(g.computed.get(cell_id).and_then(|l| {
            l.iter().filter(|v| v.asof.as_str() <= asof).max_by(|a, b| (&a.asof, &a.tx).cmp(&(&b.asof, &b.tx))).cloned()
        }))
    }

    fn stored_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        let g = self.inner.read().unwrap();
        Ok(g.versions
            .get(cell_id)
            .and_then(|l| l.iter().rev().find(|v| v.tx.as_str() <= asof && v.asof.as_str() <= asof).cloned()))
    }

    fn cell_dependents(&self, cell_id: &str) -> Result<Vec<String>> {
        let g = self.inner.read().unwrap();
        let mut out = BTreeSet::new();
        for (h, (_, r)) in &g.receipts {
            if r.cell().id() == cell_id {
                for d in g.edges.get(h).into_iter().flatten() {
                    if let Some((_, dr)) = g.receipts.get(d) {
                        out.insert(dr.cell().id());
                    }
                }
            }
        }
        Ok(out.into_iter().collect())
    }
}

/// The receipt is stored, of the cell, at the tx; returns its asof.
fn check(g: &Inner, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<String> {
    let Some((_, r)) = g.receipts.get(receipt_hash) else { return Err(not_found("receipt", receipt_hash)) };
    if r.cell().id() != cell_id {
        return Err(Error::Mismatch { what: "cell_id", expected: cell_id.into(), found: r.cell().id() });
    }
    if r.tx() != tx {
        return Err(Error::Mismatch { what: "tx", expected: tx.into(), found: r.tx().into() });
    }
    Ok(r.asof().to_string())
}
