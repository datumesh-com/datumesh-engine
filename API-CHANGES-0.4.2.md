# API changes in datumesh-engine 0.4.2

Generated with `cargo public-api -ss --color=never diff 0.4.1` (cargo-public-api 0.52.0,
nightly-tools image) against the 0.4.1 release on crates.io; the full output is at the end.
**Additions only.** `public-api.txt` and `public-api-all-features.txt` are regenerated.

| Item | What |
|---|---|
| `EngineError::NamespaceSuspended { host, suspended_at }` | New variant of a `#[non_exhaustive]` enum: a write under the mesh's suspended namespace (SPEC 9.9, store 0.4.1) is refused; nothing is signed or stored. |
| `Inputs::read_pinned(&mut self, fn_name, fn_hash, args)` | New **provided** method of the `Inputs` trait: a cell of `fn_name` at the function hash `fn_hash` (SPEC 16.3 `super`), default `EvalError::Runtime`. Implementors that do not override it are unaffected. |

Behaviour: faults are memoised (SPEC 15.12; no signature changes); suspended writes are
refused before any write; a pinned read's receipt is an input as any read's. Dependencies:
`datumesh-core` 0.3.8 (was 0.3.7), `datumesh-store` 0.4.1 (was 0.4.0). See the CHANGELOG.

## Full diff

```text
Removed items from the public API
=================================
(none)

Changed items in the public API
===============================
(none)

Added items to the public API
=============================
+pub datumesh_engine::EngineError::NamespaceSuspended
+pub datumesh_engine::EngineError::NamespaceSuspended::host: alloc::string::String
+pub datumesh_engine::EngineError::NamespaceSuspended::suspended_at: alloc::string::String
+pub fn datumesh_engine::Inputs::read_pinned(&mut self, &str, &str, &datumesh_core::model::Args) -> core::result::Result<datumesh_engine::Input, datumesh_engine::EvalError>


```
