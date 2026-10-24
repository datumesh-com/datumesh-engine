//! The memo: answers known to hold over an interval of asofs, dropped when a stored version
//! they depend on is added (the dirty set).
//!
//! An answer for cell C holds for every asof in `[lo, hi)` given the store as it was read
//! and its function names resolving as recorded (checked on every use, see
//! [`Ans::names`]). Adding a stored version can change that, so
//! [`Memo::invalidate`] drops the answers of the cell and of every cell that read it,
//! transitively, through the memo's own index of who read what (the cell-level dependents
//! of the answers it holds: a dirty mark can only matter to an answer it holds).
//!
//! An evaluation that was running when an invalidation happened may have read the old
//! version: every evaluation registers the revision it started at ([`Memo::begin`]), every
//! invalidation bumps the revision and marks the cells it reached dirty at it, and
//! [`Memo::end`] refuses an answer whose cell or whose inputs were marked dirty after the
//! evaluation began (and marks it dirty in turn, for the answers that read it). Answers are
//! inserted inputs first, so the refusal propagates. Marks are kept only while an evaluation
//! that began before them is running.
//!
//! Correctness does not depend on the memo: a miss validates the store's computed version
//! (or computes one). The memo only saves that work, so losing it (a restart) is harmless.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::engine::MemoStats;
use crate::eval::Ans;

pub(crate) struct Memo {
    capacity: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    rev: u64,
    entries: HashMap<String, Vec<Arc<Ans>>>,
    count: usize,
    /// Answers dropped by an invalidation, kept as candidates to recheck (from memory) before
    /// the store's version is looked at.
    stale: HashMap<String, Vec<Arc<Ans>>>,
    stale_count: usize,
    /// Input cell id to the cells whose answers read it.
    readers: HashMap<String, HashSet<String>>,
    edges: usize,
    /// Cell id to the revision at which it was last marked dirty.
    dirty: HashMap<String, u64>,
    /// Revisions at which running evaluations began, with how many began at each.
    active: BTreeMap<u64, usize>,
}

impl Memo {
    pub(crate) fn new(capacity: usize) -> Memo {
        Memo { capacity, state: Mutex::new(State::default()) }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.capacity > 0
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// An answer for `cell_id` holding at `asof` (its names still to be checked).
    pub(crate) fn get(&self, cell_id: &str, asof: &str) -> Option<Arc<Ans>> {
        if !self.enabled() {
            return None;
        }
        let s = self.lock();
        s.entries
            .get(cell_id)?
            .iter()
            .find(|a| a.lo.as_str() <= asof && a.hi.as_deref().is_none_or(|h| asof < h))
            .cloned()
    }

    /// A dropped answer for `cell_id` that held at `asof` before it was dropped.
    pub(crate) fn stale(&self, cell_id: &str, asof: &str) -> Option<Arc<Ans>> {
        if !self.enabled() {
            return None;
        }
        let s = self.lock();
        s.stale
            .get(cell_id)?
            .iter()
            .find(|a| a.lo.as_str() <= asof && a.hi.as_deref().is_none_or(|h| asof < h))
            .cloned()
    }

    /// Registers an evaluation; returns the revision it began at.
    pub(crate) fn begin(&self) -> u64 {
        let mut s = self.lock();
        let rev = s.rev;
        *s.active.entry(rev).or_default() += 1;
        rev
    }

    /// Ends an evaluation begun at `start`, keeping `answers` (inputs before their readers)
    /// unless something they depend on was marked dirty since.
    pub(crate) fn end(&self, start: u64, answers: &[Arc<Ans>]) {
        let mut s = self.lock();
        if self.enabled() {
            for a in answers {
                let mark = std::iter::once(&a.id).chain(a.deps()).filter_map(|c| s.dirty.get(c)).max().copied();
                if let Some(m) = mark.filter(|m| *m > start) {
                    let d = s.dirty.entry(a.id.clone()).or_default();
                    *d = (*d).max(m);
                    continue;
                }
                s.insert(a.clone(), self.capacity);
            }
        }
        s.finish(start);
    }

    /// Ends an evaluation begun at `start` that keeps nothing.
    pub(crate) fn abort(&self, start: u64) {
        self.lock().finish(start);
    }

    /// A stored version of `cell_id` was added: drops its answers and those of every cell
    /// that read it, transitively.
    pub(crate) fn invalidate(&self, cell_id: &str) {
        let mut s = self.lock();
        s.rev += 1;
        let rev = s.rev;
        let track = !s.active.is_empty();
        let mut queue = vec![cell_id.to_string()];
        let mut seen = HashSet::new();
        while let Some(c) = queue.pop() {
            if !seen.insert(c.clone()) {
                continue;
            }
            if track {
                s.dirty.insert(c.clone(), rev);
            }
            if let Some(list) = s.entries.remove(&c) {
                s.count -= list.len();
                s.stale_count += list.len();
                s.stale.entry(c.clone()).or_default().extend(list);
            }
            if let Some(readers) = s.readers.remove(&c) {
                s.edges -= readers.len();
                queue.extend(readers);
            }
        }
    }

    /// Drops every answer.
    pub(crate) fn clear(&self) {
        let mut s = self.lock();
        s.rev += 1;
        s.entries.clear();
        s.readers.clear();
        s.stale.clear();
        s.count = 0;
        s.stale_count = 0;
        s.edges = 0;
    }

    pub(crate) fn stats(&self) -> MemoStats {
        let s = self.lock();
        let mut bytes = 0usize;
        for (id, list) in &s.entries {
            bytes += id.len() + 48;
            for a in list {
                bytes += std::mem::size_of::<Ans>()
                    + a.bytes.len()
                    + a.id.len()
                    + a.hash.len()
                    + a.tx.len()
                    + a.asof.len()
                    + a.lo.len()
                    + a.hi.as_ref().map_or(0, String::len)
                    + a.inputs
                        .iter()
                        .map(|u| u.id.len() + u.hash.len() + std::mem::size_of_val(u) + 160)
                        .sum::<usize>()
                    + a.names.iter().map(|(k, v)| k.len() + v.len() + 64).sum::<usize>()
                        / Arc::strong_count(&a.names).max(1);
            }
        }
        for (k, set) in &s.readers {
            bytes += k.len() + 48 + set.iter().map(|c| c.len() + 32).sum::<usize>();
        }
        bytes += s.dirty.keys().map(|k| k.len() + 40).sum::<usize>();
        bytes += s.stale.values().flatten().map(|a| a.bytes.len() + 400).sum::<usize>();
        MemoStats {
            stale: s.stale_count,
            entries: s.count,
            cells: s.entries.len(),
            dependent_edges: s.edges,
            dirty: s.dirty.len(),
            approx_bytes: bytes,
        }
    }
}

impl State {
    fn insert(&mut self, a: Arc<Ans>, capacity: usize) {
        if let Some(list) = self.entries.get(&a.id) {
            if list.iter().any(|x| x.hash == a.hash && x.lo == a.lo && x.hi == a.hi) {
                return;
            }
        }
        if let Some(old) = self.stale.remove(&a.id) {
            self.stale_count -= old.len();
        }
        if self.count + self.stale_count >= capacity {
            self.entries.clear();
            self.readers.clear();
            self.stale.clear();
            self.count = 0;
            self.stale_count = 0;
            self.edges = 0;
        }
        for d in a.deps() {
            if self.readers.entry(d.clone()).or_default().insert(a.id.clone()) {
                self.edges += 1;
            }
        }
        self.entries.entry(a.id.clone()).or_default().push(a);
        self.count += 1;
    }

    fn finish(&mut self, start: u64) {
        if let Some(n) = self.active.get_mut(&start) {
            *n -= 1;
            if *n == 0 {
                self.active.remove(&start);
            }
        }
        // Marks matter only to evaluations that began before them.
        match self.active.keys().next().copied() {
            None => self.dirty.clear(),
            Some(oldest) => self.dirty.retain(|_, m| *m > oldest),
        }
    }
}
