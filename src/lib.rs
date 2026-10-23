//! The [Datumesh](https://datumesh.com) engine: the value of a cell as of a time.
//!
//! [`Engine::get`] loads the stored receipt of a stored cell, or evaluates a computed cell's
//! function through a [`Runtime`], memoises answers, and signs receipts for computed cells
//! with the mesh's key.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod clock;
mod engine;
mod error;
#[cfg(feature = "native-test")]
pub mod native_test;
mod runtime;

pub use clock::{Clock, SystemClock};
pub use engine::{
    Answer, Engine, EngineBuilder, MemoStats, SummaryFn, DEFAULT_DEPTH_LIMIT, DEFAULT_FUEL, DEFAULT_MEMO_CAPACITY,
};
pub use error::{ConfigError, EngineError, EvalError, StoreError};
pub use runtime::{Budget, Inputs, Runtime};
