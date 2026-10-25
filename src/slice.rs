//! Slices (SPEC 4.4, 15.13): slice objects from a function and a pattern, back from a
//! manifest's slice object, the cells of a computed function's extent, and the type order.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use datumesh_core::json::Value;
use datumesh_core::{Arg, Args, Cell, Error, ErrorCode, FunctionDescriptor, Scalar, Slice, Type};

use crate::runtime::{Pattern, SlicePattern};

/// The most cells a slice over a computed function may enumerate (SPEC 15.12, 15.13).
pub(crate) const MAX_CELLS: u128 = 1_048_576;

/// What a slice is over.
#[derive(Clone, Debug)]
pub(crate) enum Spec {
    /// The cells of a function matching a pattern.
    Cells { pattern: SlicePattern },
    /// The versions of one stored cell with `tx` in `[from, to]`.
    Versions { cell: Cell, from: String, to: String },
}

/// Two lexical values of `ty` in the type's order (SPEC 15.13): numbers numerically, dates
/// and instants chronologically (their canonical text order), strings by code point (UTF-8
/// byte order), `false` before `true`.
pub(crate) fn cmp_lexical(ty: Type, a: &str, b: &str) -> Ordering {
    match ty {
        Type::Number => cmp_numbers(a, b),
        Type::Boolean => (a == "true").cmp(&(b == "true")),
        _ => a.cmp(b),
    }
}

/// Two canonical decimal numbers (SPEC 3.1), numerically.
fn cmp_numbers(a: &str, b: &str) -> Ordering {
    let (an, a) = a.strip_prefix('-').map_or((false, a), |x| (true, x));
    let (bn, b) = b.strip_prefix('-').map_or((false, b), |x| (true, x));
    let magnitude = || {
        let (ai, af) = a.split_once('.').unwrap_or((a, ""));
        let (bi, bf) = b.split_once('.').unwrap_or((b, ""));
        ai.len().cmp(&bi.len()).then_with(|| ai.cmp(bi)).then_with(|| af.cmp(bf))
    };
    match (an, bn) {
        (false, false) => magnitude(),
        (true, true) => magnitude().reverse(),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
    }
}

fn invalid(code: ErrorCode, path: String, detail: impl Into<String>) -> Error {
    Error::new(code, path, detail)
}

/// Checks `pattern` against the function's signature (every declared argument, of its type;
/// ranges over `number`, `date`, `instant` or `string`; at least one open or ranged) and makes
/// the slice object of `fn_name` (`<ns>/<fn>`) over `fn_hash`.
pub(crate) fn cells_slice(
    fn_name: &str,
    fn_hash: &str,
    desc: &FunctionDescriptor,
    pattern: &SlicePattern,
) -> Result<Slice, Error> {
    let decl = &desc.signature.args;
    for k in pattern.keys() {
        if !decl.contains_key(k) {
            return Err(invalid(ErrorCode::ArgUnknown, format!("$.args.{k}"), "the function declares no such argument"));
        }
    }
    for k in decl.keys() {
        if !pattern.contains_key(k) {
            return Err(invalid(ErrorCode::ArgMissing, format!("$.args.{k}"), "a declared argument is not in the pattern"));
        }
    }
    let mut args = Vec::new();
    let mut open = false;
    for (k, p) in pattern {
        let ty = decl[k].ty;
        let path = format!("$.args.{k}");
        let v = match p {
            Pattern::Value(s) => {
                if s.ty() != ty {
                    return Err(invalid(ErrorCode::ArgType, path, "type differs from the declaration"));
                }
                s.to_value()
            }
            Pattern::Open => {
                open = true;
                Value::String("*".into())
            }
            Pattern::Range { lo, hi } => {
                open = true;
                if lo.ty() != ty || hi.ty() != ty {
                    return Err(invalid(ErrorCode::ArgType, path, "a range's ends are of the argument's type"));
                }
                if matches!(ty, Type::Boolean | Type::Blob) {
                    return Err(invalid(ErrorCode::CanonType, path, format!("a {} argument has no range", ty.as_str())));
                }
                Value::Object(vec![
                    ("hi".into(), Value::String(hi.lexical().into())),
                    ("lo".into(), Value::String(lo.lexical().into())),
                    ("type".into(), Value::String(ty.as_str().into())),
                ])
            }
        };
        args.push((k.clone(), v));
    }
    if !open {
        return Err(invalid(ErrorCode::Slice, "$.args".into(), "a slice has an open or ranged argument: read the cell"));
    }
    let (ns, f) = fn_name.rsplit_once('/').unwrap_or(("", fn_name));
    Slice::from_value(&Value::Object(vec![
        ("args".into(), Value::Object(args)),
        ("fn".into(), Value::String(f.into())),
        ("fn_hash".into(), Value::String(fn_hash.into())),
        ("ns".into(), Value::String(ns.into())),
    ]))
}

/// The version slice of `cell` over `[from, to]`.
pub(crate) fn versions_slice(cell: &Cell, from: &str, to: &str) -> Result<Slice, Error> {
    Slice::from_value(&Value::Object(vec![
        ("cell".into(), cell.to_value()),
        ("from".into(), Value::String(from.into())),
        ("to".into(), Value::String(to.into())),
    ]))
}

/// What a slice object (a manifest's) is over: back from its JSON form.
pub(crate) fn spec_of(slice: &Slice) -> Option<Spec> {
    let v = slice.to_value();
    let Value::Object(m) = &v else { return None };
    let get = |k: &str| m.iter().find(|(n, _)| n == k).map(|(_, x)| x);
    if slice.is_version_slice() {
        let cell = Cell::from_value(get("cell")?).ok()?;
        return Some(Spec::Versions {
            cell,
            from: get("from")?.as_str()?.to_string(),
            to: get("to")?.as_str()?.to_string(),
        });
    }
    let Value::Object(args) = get("args")? else { return None };
    let mut pattern = BTreeMap::new();
    for (k, a) in args {
        let p = match a {
            Value::String(s) if s == "*" => Pattern::Open,
            Value::Object(r) if r.len() == 3 && r.iter().any(|(n, _)| n == "lo") => {
                let field = |n: &str| r.iter().find(|(x, _)| x == n).and_then(|(_, v)| v.as_str());
                let ty = Type::from_name(field("type")?)?;
                Pattern::Range {
                    lo: Scalar::from_lexical(ty, field("lo")?).ok()?,
                    hi: Scalar::from_lexical(ty, field("hi")?).ok()?,
                }
            }
            other => Pattern::Value(Scalar::from_json(&other.to_jcs()).ok()?),
        };
        pattern.insert(k.clone(), p);
    }
    Some(Spec::Cells { pattern })
}

/// The cells of a computed function's slice (SPEC 15.13), in key order: for each open or
/// ranged argument (ascending name order), the values of its `extent` within the pattern;
/// without an extent, a range over a `date` (every day) or a `number` (every integer) is
/// enumerated, anything else is refused (`E_SLICE`: SPEC's define-time `E_FORMULA_UNBOUNDED`).
pub(crate) fn extent_cells(
    fn_name: &str,
    fn_hash: &str,
    desc: &FunctionDescriptor,
    pattern: &SlicePattern,
) -> Result<Vec<Cell>, Error> {
    let mut lists: Vec<(String, Vec<Scalar>)> = Vec::new();
    for (k, p) in pattern {
        let ty = desc.signature.args.get(k).map(|d| d.ty).unwrap_or(Type::String);
        let path = format!("$.args.{k}");
        let values = match p {
            Pattern::Value(v) => vec![v.clone()],
            Pattern::Open | Pattern::Range { .. } => {
                let within = |v: &str| match p {
                    Pattern::Range { lo, hi } => {
                        cmp_lexical(ty, lo.lexical(), v) != Ordering::Greater
                            && cmp_lexical(ty, v, hi.lexical()) != Ordering::Greater
                    }
                    _ => true,
                };
                match (desc.extents.get(k), p) {
                    (Some(datumesh_core::Extent::Values(vs)), _) => vs
                        .iter()
                        .filter(|v| within(v))
                        .map(|v| Scalar::from_lexical(ty, v))
                        .collect::<Result<Vec<_>, _>>()?,
                    (Some(datumesh_core::Extent::Range { lo, hi }), _) => {
                        let (lo, hi) = match p {
                            Pattern::Range { lo: plo, hi: phi } => (
                                max_lex(ty, lo, plo.lexical()).to_string(),
                                min_lex(ty, hi, phi.lexical()).to_string(),
                            ),
                            _ => (lo.clone(), hi.clone()),
                        };
                        enumerate(ty, &lo, &hi, &path)?
                    }
                    (None, Pattern::Range { lo, hi }) => enumerate(ty, lo.lexical(), hi.lexical(), &path)?,
                    (None, _) => {
                        return Err(invalid(
                            ErrorCode::Slice,
                            path,
                            "an open argument of a computed function without an extent (E_FORMULA_UNBOUNDED)",
                        ))
                    }
                    (Some(_), _) => {
                        return Err(invalid(ErrorCode::Slice, path, "an extent this engine does not know"));
                    }
                }
            }
        };
        lists.push((k.clone(), values));
    }
    let total = lists.iter().fold(1u128, |n, (_, v)| n.saturating_mul(v.len() as u128));
    if total > MAX_CELLS {
        return Err(invalid(ErrorCode::CanonLimit, "$.args".into(), "a slice of more than 1,048,576 cells"));
    }
    let mut out: Vec<Args> = vec![Args::new()];
    for (name, values) in &lists {
        let mut next = Vec::with_capacity(out.len() * values.len());
        for a in &out {
            for v in values {
                let mut b = a.clone();
                b.insert(name.clone(), Arg::Value(v.clone()));
                next.push(b);
            }
        }
        out = next;
    }
    let (ns, f) = fn_name.rsplit_once('/').unwrap_or(("", fn_name));
    Ok(out
        .into_iter()
        .map(|args| Cell { ns: ns.into(), fn_name: f.into(), fn_hash: fn_hash.into(), args })
        .collect())
}

fn max_lex<'a>(ty: Type, a: &'a str, b: &'a str) -> &'a str {
    if cmp_lexical(ty, a, b) == Ordering::Less {
        b
    } else {
        a
    }
}

fn min_lex<'a>(ty: Type, a: &'a str, b: &'a str) -> &'a str {
    if cmp_lexical(ty, a, b) == Ordering::Greater {
        b
    } else {
        a
    }
}

/// Every day (a `date`) or integer (a `number`) in `[lo, hi]`, ends rounded inward.
fn enumerate(ty: Type, lo: &str, hi: &str, path: &str) -> Result<Vec<Scalar>, Error> {
    let unbounded = || invalid(ErrorCode::Slice, path.to_string(), "a range that cannot be enumerated (E_FORMULA_UNBOUNDED)");
    let too_many = || invalid(ErrorCode::CanonLimit, path.to_string(), "a slice of more than 1,048,576 cells");
    let (a, b) = match ty {
        Type::Number => (ceil(lo).ok_or_else(unbounded)?, floor(hi).ok_or_else(unbounded)?),
        Type::Date => (day_number(lo).ok_or_else(unbounded)?, day_number(hi).ok_or_else(unbounded)?),
        _ => return Err(unbounded()),
    };
    if a > b {
        return Ok(Vec::new());
    }
    if (b - a) as u128 >= MAX_CELLS {
        return Err(too_many());
    }
    (a..=b)
        .map(|n| match ty {
            Type::Number => Scalar::from_lexical(Type::Number, &n.to_string()),
            _ => Scalar::from_lexical(Type::Date, &date_of(n)),
        })
        .collect()
}

/// The integer part of a canonical decimal, and whether it had a fraction.
fn split_int(s: &str) -> Option<(i128, bool)> {
    let (i, f) = s.split_once('.').unwrap_or((s, ""));
    if i.trim_start_matches('-').len() > 36 {
        return None;
    }
    Some((i.parse().ok()?, !f.is_empty()))
}

fn ceil(s: &str) -> Option<i128> {
    let (i, frac) = split_int(s)?;
    Some(if frac && !s.starts_with('-') { i + 1 } else { i })
}

fn floor(s: &str) -> Option<i128> {
    let (i, frac) = split_int(s)?;
    Some(if frac && s.starts_with('-') { i - 1 } else { i })
}

/// Days since 1970-01-01 of a `YYYY-MM-DD` date (Howard Hinnant's algorithm).
fn day_number(s: &str) -> Option<i128> {
    let y: i128 = s.get(0..4)?.parse().ok()?;
    let m: i128 = s.get(5..7)?.parse().ok()?;
    let d: i128 = s.get(8..10)?.parse().ok()?;
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn date_of(n: i128) -> String {
    let (y, m, d) = crate::clock::civil_from_days(n as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_and_days() {
        assert_eq!(cmp_numbers("9", "10"), Ordering::Less);
        assert_eq!(cmp_numbers("-1.5", "-1"), Ordering::Less);
        assert_eq!(cmp_numbers("0.45", "0.5"), Ordering::Less);
        assert_eq!((ceil("1.5"), floor("1.5"), ceil("-1.5"), floor("-1.5")), (Some(2), Some(1), Some(-1), Some(-2)));
        for s in ["1970-01-01", "2000-02-29", "2024-12-31", "0001-01-01", "9999-12-31"] {
            assert_eq!(date_of(day_number(s).unwrap()), s);
        }
        assert_eq!(day_number("2024-03-01").unwrap() - day_number("2024-02-28").unwrap(), 2);
    }
}
