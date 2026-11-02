# API changes in datumesh-engine 0.3.2

Generated with `cargo public-api -ss --color=never diff 0.3.1` (cargo-public-api 0.52.0,
nightly-tools image) against the 0.3.1 release on crates.io; the full output is at the end.
**Additions only.** `public-api.txt` and `public-api-all-features.txt` are regenerated.

| Item | What |
|---|---|
| `Inputs::read_blob(sha256, length)` | Provided (default: `EvalError::BlobUnavailable`, an `Inputs` with no content to offer). The engine's reads the content from the store and checks its length and SHA-256 (spec-0.4.2 `text_of`). |
| `EvalError::BlobUnavailable { sha256, reason }`, `EngineError::BlobUnavailable { sha256, reason }` | New variants of `#[non_exhaustive]` enums: content unavailable or not the blob's, the evaluator's failure (`E_EVAL_UNAVAILABLE`). |
| `Engine::resolve(cell_ref, asof) -> Option<Cell>` | The cell `get` would answer, or `None` for an unbound name (`E_NAME_UNBOUND`). |

Behaviour: none changed. `get` still answers `NotCovered` for an unbound name;
`Resolver::evaluation` evaluates through the engine, which reads blob content from the store.
Dependency: `datumesh-core` 0.3.5 (was 0.3.4).

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
+pub datumesh_engine::EngineError::BlobUnavailable
+pub datumesh_engine::EngineError::BlobUnavailable::reason: alloc::string::String
+pub datumesh_engine::EngineError::BlobUnavailable::sha256: alloc::string::String
+pub datumesh_engine::EvalError::BlobUnavailable
+pub datumesh_engine::EvalError::BlobUnavailable::reason: alloc::string::String
+pub datumesh_engine::EvalError::BlobUnavailable::sha256: alloc::string::String
+pub fn datumesh_engine::Engine::resolve(&self, &datumesh_core::model::CellRef, core::option::Option<&str>) -> core::result::Result<core::option::Option<datumesh_core::model::Cell>, datumesh_engine::EngineError>
+pub fn datumesh_engine::Inputs::read_blob(&mut self, &str, u64) -> core::result::Result<alloc::vec::Vec<u8>, datumesh_engine::EvalError>

```
