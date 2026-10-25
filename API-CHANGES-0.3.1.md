# API changes in datumesh-engine 0.3.1

Generated with `cargo public-api -ss --color=never diff 0.3.0` (cargo-public-api 0.52.0,
nightly-tools image) against the 0.3.0 release on crates.io; the full output is at the end.
**Additions only.** `public-api.txt` and `public-api-all-features.txt` are regenerated.

| Item | What |
|---|---|
| `Inputs::binding_peek(fn_name, at)` | Provided (default: `binding`). A binding looked up without being relied on. |
| `Inputs::asof_compare(t)` | Provided (default: through `asof`). `t` against the evaluation asof; at or before it, `t` becomes an asof candidate instead of the evaluation becoming asof-dependent. |
| `FnBinding::new` | A binding without a store, for a runtime's tests. |

The two `Inputs` methods have default bodies, so an `Inputs` written for 0.3.0 still compiles;
its defaults are the conservative readings (relying on the binding; asof-dependent). The
engine's own inputs override both.

Dependency: `datumesh-store` 0.3.2 (`slice_values`, `slice_changed_after`), an additive
release of the same sealed trait.

## Behaviour (not in the listing)

- A slice over a computed function evaluates its members in key order and stops at the first
  without a value.
- A charge exceeding both limits is `BudgetExceeded`.
- `E_FORMULA_FUTURE` stands over an input without a value when the engine saw a time after the
  asof or the runtime read the asof.
- Stored slices are read through `slice_values` and `slice_changed_after`: no receipt bytes,
  no second scan.
- Whether a function is asof-dependent is asked of its runtime once per function hash.

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
+impl datumesh_engine::FnBinding
+pub fn datumesh_engine::FnBinding::new(impl core::convert::Into<alloc::string::String>, impl core::convert::Into<alloc::string::String>, alloc::rcs::arc::Arc<datumesh_core::descriptor::FunctionDescriptor>) -> datumesh_engine::FnBinding
+pub fn datumesh_engine::Inputs::asof_compare(&mut self, &str) -> core::result::Result<core::cmp::Ordering, datumesh_engine::EvalError>
+pub fn datumesh_engine::Inputs::binding_peek(&mut self, &str, core::option::Option<&str>) -> core::result::Result<core::option::Option<datumesh_engine::FnBinding>, datumesh_engine::EvalError>

```
