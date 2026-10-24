//! The memo: answers known to hold over an interval of asofs.

/// Memoised answers (filled in the memo phase).
pub(crate) struct Memo {
    capacity: usize,
}

impl Memo {
    pub(crate) fn new(capacity: usize) -> Memo {
        Memo { capacity }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }
}
