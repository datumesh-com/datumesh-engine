//! Checks of requests: slices, and arguments against a function's signature (as SPEC 10.1
//! step 7.2 checks a receipt's cell, so that every receipt the engine signs recomputes).

use datumesh_core::{Arg, Args, Cell, Descriptor, Error, ErrorCode, Scalar};

use crate::error::EngineError;

/// `E_SLICE` for an argument that is the slice marker: a request is for one cell.
pub(crate) fn check_no_slice(args: &Args) -> Result<(), EngineError> {
    for (k, a) in args {
        if *a == Arg::Slice {
            return Err(EngineError::Invalid(Error::new(
                ErrorCode::Slice,
                format!("$.args.{k}"),
                "a cell is asked for, not a slice",
            )));
        }
    }
    Ok(())
}

/// The cell's arguments are exactly the declared ones, each of the declared type: first an
/// undeclared argument (`E_ARG_UNKNOWN`), then an unbound one (`E_ARG_MISSING`), then a type
/// (`E_ARG_TYPE`), each in JCS order of names.
pub(crate) fn check_args(cell: &Cell, d: &Descriptor) -> Result<(), Error> {
    let decl = &d.signature.args;
    for k in cell.args.keys() {
        if !decl.contains_key(k) {
            return Err(Error::new(
                ErrorCode::ArgUnknown,
                format!("$.args.{k}"),
                "the function declares no such argument",
            ));
        }
    }
    for k in decl.keys() {
        if !cell.args.contains_key(k) {
            return Err(Error::new(ErrorCode::ArgMissing, format!("$.args.{k}"), "a declared argument is not bound"));
        }
    }
    for (k, a) in &cell.args {
        if a.scalar().map(Scalar::ty) != decl.get(k).map(|x| x.ty) {
            return Err(Error::new(ErrorCode::ArgType, format!("$.args.{k}"), "type differs from the declaration"));
        }
    }
    Ok(())
}

/// The valid-time arguments of a cell (SPEC 8.1 `valid`).
pub(crate) fn valid_args(cell: &Cell, d: &Descriptor) -> Args {
    cell.args
        .iter()
        .filter(|(k, _)| d.signature.args.get(*k).is_some_and(|x| x.valid))
        .map(|(k, a)| (k.clone(), a.clone()))
        .collect()
}
