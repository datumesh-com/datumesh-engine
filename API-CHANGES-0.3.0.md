# API changes in datumesh-engine 0.3.0

Generated with `cargo public-api -ss --color=never diff 0.2.0` (cargo-public-api 0.52.0,
nightly-tools image) against the 0.2.0 release on crates.io; the full output is at the end.
`public-api.txt` and `public-api-all-features.txt` are regenerated (`native-test` adds only
`NativeTest::set_asof_dependent` and its `Runtime::asof_dependent`).

## Breaking

| Item | Change | Why |
|---|---|---|
| `Runtime::evaluate(&self, &FunctionDescriptor, ..)` | Was `&Descriptor`. | spec-0.4.0 descriptors carry `signature.order` and argument `extent`, which core's `Descriptor` refuses; formula runtimes need both. |
| `Inputs` | New required methods `read_cell`, `read_asof`, `read_slice`, `read_versions`, `binding`, `asof`. | The reads of spec-0.4.0 (15.12, 15.13). Only the engine implements `Inputs`; a runtime's test double must add them. |
| `Answer` | `#[non_exhaustive]`; `NotCovered` is the struct variant `NotCovered { fault: Option<Fault> }` (the listing shows the variant's new field only). `Answer::NOT_COVERED` is the one without a fault. | A faulted evaluation is `not_covered` with the code (SPEC 5.5, 15.12); further answers can come without a break. |
| `datumesh-store` 0.3.1, `datumesh-core` 0.3.4 | Minimums. | Slices, manifests, `FunctionDescriptor`, `Fault`, `Evaluation`. |

## Added

| Item | What |
|---|---|
| `Input`, `Member`, `Pattern`, `SlicePattern`, `FnBinding` | What the new reads take and give (`#[non_exhaustive]`; `Input::new`, `Member::new` for a runtime's tests). |
| `Runtime::asof_dependent` (provided: `false`) | An asof-dependent function: one receipt per requested asof, never reused for another. |
| `EvalError::Fault(Fault)` | An evaluation fault. |
| `EngineError::WorkExceeded { limit }`, `EngineBuilder::work_limit`, `DEFAULT_WORK_FACTOR` | The bound on total work (attempts discarded by restarts, validation reads) beside the normative fuel. |
| `Budget::used` | The fuel of the running attempt; at its end, the evaluation's normative fuel. |
| `Engine::manifest`, `Engine::evaluation` (`Resolver`) | Manifests from the store; the evaluation with its lineage (`E_INPUT_SUPERSEDED`). |
| `MemoStats::slice_patterns` | Slices over stored functions held by memoised answers. |
| `NativeTest::set_asof_dependent` (feature `native-test`) | Declares a test function asof-dependent. |

## Behaviour (not in the listing)

- A computed receipt's `asof` also counts slice members, `T` of `read_asof` and `to` of
  `read_versions`; asof-dependent receipts have the evaluation asof.
- Fuel: one meter per attempt; a cell's fuel is its completed attempt's; validation reads are
  work, not fuel. A request that fit its fuel in 0.2.0 fits it in 0.3.0.
- `stored_version_added` also drops answers whose slice pattern the new version falls into.
- Recomputing a receipt whose input was superseded by a backdated correction reports
  `E_INPUT_SUPERSEDED` (level signed) through `Engine` as the resolver, where 0.2.0 gave
  `E_VALUE_MISMATCH`.

## Full diff

```text
Removed items from the public API
=================================
(none)

Changed items in the public API
===============================
-pub enum datumesh_engine::Answer
+#[non_exhaustive] pub enum datumesh_engine::Answer
-pub fn datumesh_engine::Runtime::evaluate(&self, &datumesh_core::descriptor::Descriptor, &datumesh_core::model::Cell, &mut dyn datumesh_engine::Inputs, &mut datumesh_engine::Budget) -> core::result::Result<datumesh_core::model::Scalar, datumesh_engine::EvalError>
+pub fn datumesh_engine::Runtime::evaluate(&self, &datumesh_core::descriptor::FunctionDescriptor, &datumesh_core::model::Cell, &mut dyn datumesh_engine::Inputs, &mut datumesh_engine::Budget) -> core::result::Result<datumesh_core::model::Scalar, datumesh_engine::EvalError>

Added items to the public API
=============================
+pub datumesh_engine::Answer::NotCovered::fault: core::option::Option<datumesh_core::envelope::Fault>
+impl datumesh_engine::Answer
+pub const datumesh_engine::Answer::NOT_COVERED: datumesh_engine::Answer
+pub datumesh_engine::EngineError::WorkExceeded
+pub datumesh_engine::EngineError::WorkExceeded::limit: u64
+pub datumesh_engine::EvalError::Fault(datumesh_core::envelope::Fault)
+#[non_exhaustive] pub enum datumesh_engine::Pattern
+pub datumesh_engine::Pattern::Open
+pub datumesh_engine::Pattern::Range
+pub datumesh_engine::Pattern::Range::hi: datumesh_core::model::Scalar
+pub datumesh_engine::Pattern::Range::lo: datumesh_core::model::Scalar
+pub datumesh_engine::Pattern::Value(datumesh_core::model::Scalar)
+impl core::clone::Clone for datumesh_engine::Pattern
+pub fn datumesh_engine::Pattern::clone(&self) -> Self
+impl core::cmp::Eq for datumesh_engine::Pattern
+impl core::cmp::PartialEq for datumesh_engine::Pattern
+pub fn datumesh_engine::Pattern::eq(&self, &Self) -> bool
+impl core::fmt::Debug for datumesh_engine::Pattern
+pub fn datumesh_engine::Pattern::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+impl core::marker::StructuralPartialEq for datumesh_engine::Pattern
+pub fn datumesh_engine::Budget::used(&self) -> u64
+pub fn datumesh_engine::Engine::evaluation(&self, &datumesh_core::model::Cell, &str) -> core::option::Option<datumesh_core::verify::Evaluation>
+pub fn datumesh_engine::Engine::manifest(&self, &str) -> core::option::Option<alloc::borrow::Cow<'_, [u8]>>
+pub fn datumesh_engine::EngineBuilder::work_limit(self, u64) -> Self
+#[non_exhaustive] pub struct datumesh_engine::FnBinding
+pub datumesh_engine::FnBinding::descriptor: alloc::rcs::arc::Arc<datumesh_core::descriptor::FunctionDescriptor>
+pub datumesh_engine::FnBinding::fn_hash: alloc::string::String
+pub datumesh_engine::FnBinding::start: alloc::string::String
+impl core::clone::Clone for datumesh_engine::FnBinding
+pub fn datumesh_engine::FnBinding::clone(&self) -> Self
+impl core::fmt::Debug for datumesh_engine::FnBinding
+pub fn datumesh_engine::FnBinding::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+#[non_exhaustive] pub struct datumesh_engine::Input
+pub datumesh_engine::Input::asof: alloc::string::String
+pub datumesh_engine::Input::receipt_hash: alloc::string::String
+pub datumesh_engine::Input::tx: alloc::string::String
+pub datumesh_engine::Input::value: datumesh_core::model::Scalar
+impl datumesh_engine::Input
+pub fn datumesh_engine::Input::new(datumesh_core::model::Scalar, impl core::convert::Into<alloc::string::String>, impl core::convert::Into<alloc::string::String>, impl core::convert::Into<alloc::string::String>) -> datumesh_engine::Input
+impl core::clone::Clone for datumesh_engine::Input
+pub fn datumesh_engine::Input::clone(&self) -> Self
+impl core::cmp::Eq for datumesh_engine::Input
+impl core::cmp::PartialEq for datumesh_engine::Input
+pub fn datumesh_engine::Input::eq(&self, &Self) -> bool
+impl core::fmt::Debug for datumesh_engine::Input
+pub fn datumesh_engine::Input::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+impl core::marker::StructuralPartialEq for datumesh_engine::Input
+#[non_exhaustive] pub struct datumesh_engine::Member
+pub datumesh_engine::Member::args: datumesh_core::model::Args
+pub datumesh_engine::Member::input: datumesh_engine::Input
+impl datumesh_engine::Member
+pub fn datumesh_engine::Member::new(datumesh_core::model::Args, datumesh_engine::Input) -> datumesh_engine::Member
+impl core::clone::Clone for datumesh_engine::Member
+pub fn datumesh_engine::Member::clone(&self) -> Self
+impl core::cmp::Eq for datumesh_engine::Member
+impl core::cmp::PartialEq for datumesh_engine::Member
+pub fn datumesh_engine::Member::eq(&self, &Self) -> bool
+impl core::fmt::Debug for datumesh_engine::Member
+pub fn datumesh_engine::Member::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+impl core::marker::StructuralPartialEq for datumesh_engine::Member
+pub datumesh_engine::MemoStats::slice_patterns: usize
+pub const datumesh_engine::DEFAULT_WORK_FACTOR: u64
+pub fn datumesh_engine::Inputs::asof(&mut self) -> alloc::string::String
+pub fn datumesh_engine::Inputs::binding(&mut self, &str, core::option::Option<&str>) -> core::result::Result<core::option::Option<datumesh_engine::FnBinding>, datumesh_engine::EvalError>
+pub fn datumesh_engine::Inputs::read_asof(&mut self, &str, &datumesh_core::model::Args, &str) -> core::result::Result<datumesh_engine::Input, datumesh_engine::EvalError>
+pub fn datumesh_engine::Inputs::read_cell(&mut self, &str, &datumesh_core::model::Args) -> core::result::Result<datumesh_engine::Input, datumesh_engine::EvalError>
+pub fn datumesh_engine::Inputs::read_slice(&mut self, &str, &datumesh_engine::SlicePattern) -> core::result::Result<alloc::vec::Vec<datumesh_engine::Member>, datumesh_engine::EvalError>
+pub fn datumesh_engine::Inputs::read_versions(&mut self, &str, &datumesh_core::model::Args, &str, &str) -> core::result::Result<alloc::vec::Vec<datumesh_engine::Input>, datumesh_engine::EvalError>
+pub fn datumesh_engine::Runtime::asof_dependent(&self, &datumesh_core::descriptor::FunctionDescriptor) -> bool
+pub type datumesh_engine::SlicePattern = alloc::collections::btree::map::BTreeMap<alloc::string::String, datumesh_engine::Pattern>

```
