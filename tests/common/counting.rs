//! A `Store` that forwards to another and counts the calls of each method.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use datumesh_core::{Cell, Resolver, Scalar};
use datumesh_store::{Namespace, Result, Store, Version};

pub struct Counting {
    pub inner: Arc<dyn Store + Send + Sync>,
    pub calls: Mutex<BTreeMap<&'static str, usize>>,
}

impl Counting {
    pub fn new(inner: Arc<dyn Store + Send + Sync>) -> Counting {
        Counting { inner, calls: Mutex::new(BTreeMap::new()) }
    }
    fn count(&self, m: &'static str) {
        *self.calls.lock().unwrap().entry(m).or_default() += 1;
    }
    pub fn take(&self) -> BTreeMap<&'static str, usize> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl Resolver for Counting {
    fn descriptor(&self, h: &str) -> Option<Cow<'_, [u8]>> {
        self.count("descriptor");
        self.inner.descriptor(h)
    }
    fn receipt(&self, h: &str) -> Option<Cow<'_, [u8]>> {
        self.count("receipt");
        self.inner.receipt(h)
    }
    fn evaluate(&self, c: &Cell, a: &str) -> Option<Scalar> {
        self.count("evaluate");
        self.inner.evaluate(c, a)
    }
}

macro_rules! fwd {
    ($name:ident ( $($a:ident : $t:ty),* ) -> $r:ty) => {
        fn $name(&self, $($a: $t),*) -> $r {
            self.count(stringify!($name));
            self.inner.$name($($a),*)
        }
    };
}

impl Store for Counting {
    fwd!(put_descriptor(b: &[u8]) -> Result<String>);
    fwd!(put_receipt(b: &[u8]) -> Result<String>);
    fwd!(put_blob(b: &[u8]) -> Result<String>);
    fwd!(blob(h: &str) -> Result<Option<Cow<'_, [u8]>>>);
    fwd!(put_version(c: &str, t: &str, h: &str) -> Result<()>);
    fwd!(version_asof(c: &str, a: &str) -> Result<Option<Version>>);
    fwd!(versions(c: &str, f: &str, t: &str) -> Result<Vec<Version>>);
    fwd!(latest(c: &str) -> Result<Option<Version>>);
    fwd!(dependents(h: &str) -> Result<Vec<String>>);
    fwd!(inputs(h: &str) -> Result<Vec<String>>);
    fwd!(put_namespace(h: &str, k: &[u8], v: &str) -> Result<()>);
    fwd!(namespace(h: &str) -> Result<Option<Namespace>>);
    fwd!(bind_name(n: &str, h: &str, t: &str) -> Result<()>);
    fwd!(resolve_name(n: &str, a: &str) -> Result<Option<String>>);
    fwd!(put_computed(c: &str, t: &str, h: &str) -> Result<()>);
    fwd!(computed_asof(c: &str, a: &str) -> Result<Option<Version>>);
    fwd!(stored_asof(c: &str, a: &str) -> Result<Option<Version>>);
    fwd!(cell_dependents(c: &str) -> Result<Vec<String>>);
    fwd!(computed_asof_with_receipt(c: &str, a: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>>);
    fwd!(stored_asof_with_receipt(c: &str, a: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>>);
}
