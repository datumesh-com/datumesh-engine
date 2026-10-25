//! [`NativeTest`] (feature `native-test`): a runtime of Rust closures keyed by function hash,
//! for tests and benchmarks. Its language is `native_test`.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use datumesh_core::{Cell, FunctionDescriptor, Scalar};

use crate::error::EvalError;
use crate::runtime::{Budget, Inputs, Runtime};

/// The language of [`NativeTest`]: descriptors with `code.lang` = `native_test`.
pub const LANG: &str = "native_test";

/// A function of the [`NativeTest`] runtime.
pub type NativeFn = dyn Fn(&Cell, &mut dyn Inputs, &mut Budget) -> Result<Scalar, EvalError> + Send + Sync;

/// A runtime whose functions are Rust closures, registered under the hash of the descriptor
/// they implement. For tests only: nothing ties a closure to the descriptor's body.
#[derive(Clone, Default)]
pub struct NativeTest {
    fns: HashMap<String, Arc<NativeFn>>,
    asof_dependent: HashSet<String>,
}

impl NativeTest {
    /// A runtime with no functions.
    pub fn new() -> NativeTest {
        NativeTest::default()
    }

    /// Registers `f` as the function whose descriptor hashes to `fn_hash`.
    pub fn register(
        &mut self,
        fn_hash: impl Into<String>,
        f: impl Fn(&Cell, &mut dyn Inputs, &mut Budget) -> Result<Scalar, EvalError> + Send + Sync + 'static,
    ) -> &mut Self {
        self.fns.insert(fn_hash.into(), Arc::new(f));
        self
    }

    /// Declares the function whose descriptor hashes to `fn_hash` asof-dependent
    /// ([`Runtime::asof_dependent`]). Since 0.3.0.
    pub fn set_asof_dependent(&mut self, fn_hash: impl Into<String>) -> &mut Self {
        self.asof_dependent.insert(fn_hash.into());
        self
    }
}

impl fmt::Debug for NativeTest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeTest").field("functions", &self.fns.len()).finish()
    }
}

impl Runtime for NativeTest {
    fn lang(&self) -> &str {
        LANG
    }

    fn asof_dependent(&self, descriptor: &FunctionDescriptor) -> bool {
        self.asof_dependent.contains(&descriptor.hash())
    }

    fn evaluate(
        &self,
        _descriptor: &FunctionDescriptor,
        cell: &Cell,
        inputs: &mut dyn Inputs,
        budget: &mut Budget,
    ) -> Result<Scalar, EvalError> {
        match self.fns.get(&cell.fn_hash) {
            Some(f) => {
                budget.charge(1)?;
                f(cell, inputs, budget)
            }
            None => Err(EvalError::Runtime(format!("no native function registered for {}", cell.fn_hash))),
        }
    }
}
