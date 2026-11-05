# API changes in datumesh-engine 0.4.1

Generated with `cargo public-api -ss --color=never diff 0.4.0` (cargo-public-api 0.52.0,
nightly-tools image) against the 0.4.0 release on crates.io; the full output is at the end.
**Additions only.** `public-api.txt` and `public-api-all-features.txt` are regenerated.

| Item | What |
|---|---|
| `EngineError::Unlicensed { cell_id, reason }` | New variant of a `#[non_exhaustive]` enum: licence inheritance (SPEC 8.5, spec-0.4.3) refuses the cell, or a cell it reads; `reason` is core's `LicenceReason`. The HTTP layer answers `unlicensed` (403). |

Behaviour: a computed receipt's `licence` is the result of SPEC 8.5 over its input receipts'
licences (the configured licence only for a receipt with no input); a computed version is
reused only while its licence is allowed (see the CHANGELOG). Dependency: `datumesh-core` 0.3.7
(was 0.3.5).

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
+pub datumesh_engine::EngineError::Unlicensed
+pub datumesh_engine::EngineError::Unlicensed::cell_id: alloc::string::String
+pub datumesh_engine::EngineError::Unlicensed::reason: datumesh_core::licence::LicenceReason

```
