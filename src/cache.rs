//! Bounded caches of immutable, content-addressed things: parsed descriptors and receipts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use datumesh_core::Receipt;

/// A map from a hash to a value that never changes for that hash; when it holds `capacity`
/// entries it starts afresh.
pub(crate) struct Cache<V> {
    map: Mutex<HashMap<String, V>>,
    capacity: usize,
}

impl<V: Clone> Cache<V> {
    pub(crate) fn new(capacity: usize) -> Cache<V> {
        Cache { map: Mutex::new(HashMap::new()), capacity }
    }

    pub(crate) fn get(&self, key: &str) -> Option<V> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).get(key).cloned()
    }

    pub(crate) fn insert(&self, key: String, value: V) {
        let mut m = self.map.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() >= self.capacity {
            m.clear();
        }
        m.insert(key, value);
    }
}

/// A receipt as stored: its canonical bytes, parsed.
pub(crate) struct Parsed {
    pub(crate) bytes: Arc<Vec<u8>>,
    pub(crate) receipt: Receipt,
}
