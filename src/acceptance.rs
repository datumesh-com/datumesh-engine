//! The local acceptance policy of licence inheritance (engine 0.6.1, E1; DECISIONS-round-protocol
//! D4, D4.1): on an unrooted host, a restricted input licence that the operator has accepted may
//! be computed over, where SPEC 8.5 would refuse it.

use std::collections::BTreeSet;

use datumesh_core::{lexical, LicenceReason};

/// The licence of a local result over two or more distinct accepted restricted licences (E1,
/// D4.1). Such a result can never be published or exported (the CLI refuses it).
pub const LOCAL_DERIVATION_LICENCE: &str = "LicenseRef-datumesh-local-derivation";

/// The open identifiers of SPEC 8.5's open tiers (`spec-0.6.2`), as core lists them. Core keeps
/// them private, so this copy must agree with core: the unit tests check that core gives each of
/// them a licence when it is the only input.
const OPEN_IDENTIFIERS: &[&str] =
    &["CC0-1.0", "PDDL-1.0", "MIT", "Apache-2.0", "CC-BY-4.0", "OGL-UK-3.0", "CC-BY-SA-4.0", "ODbL-1.0"];

/// Whether `licence` is open in SPEC 8.5: one of the open identifiers, or a `public:<https URL>`
/// licence (the public information tier). Every other licence, an https URL included, is
/// restricted.
pub(crate) fn is_open(licence: &str) -> bool {
    OPEN_IDENTIFIERS.contains(&licence)
        || (licence.starts_with("public:") && lexical::check_licence(licence, "$").is_ok())
}

/// The outcome of the acceptance rule for one request: the licence the result carries, or the
/// reason it is refused, as core gives it. Engine-internal: core's `Inheritance` is
/// non-exhaustive and cannot be built here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rule {
    /// The result's licence; `None` when the mesh may not compute the cell.
    pub(crate) licence: Option<String>,
    /// Why: core's reason. For a result given by acceptance, the reason of the refusal it
    /// overrides (`not_declared`, `declaration_not_open` or `restricted_conflict`).
    pub(crate) reason: LicenceReason,
}

/// What the acceptance rule decides for a refusal of core (E1).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Computed under this licence.
    Give(String),
    /// Restricted licences not accepted: the sorted list of the missing ones.
    Missing(Vec<String>),
}

/// Whether core's refusal `reason` is one the acceptance rule may override: a restricted input
/// not declared (`not_declared`), declared but not open (`declaration_not_open`), or two
/// restricted licences in conflict (`restricted_conflict`). A share-alike conflict is never
/// overridden.
pub(crate) fn overridable(reason: LicenceReason) -> bool {
    matches!(reason, LicenceReason::NotDeclared | LicenceReason::DeclarationNotOpen | LicenceReason::RestrictedConflict)
}

/// The acceptance rule (E1) over the input licences `licences`, for a refusal `reason` of core
/// that [`overridable`] allows. Every restricted input licence must be in `accepted`: then one
/// distinct restricted licence is the result's licence, and two or more give
/// [`LOCAL_DERIVATION_LICENCE`]. Otherwise the missing licences are named.
pub(crate) fn decide(licences: &[String], accepted: &BTreeSet<String>) -> Verdict {
    let restricted: BTreeSet<&str> = licences.iter().map(String::as_str).filter(|l| !is_open(l)).collect();
    let missing: Vec<String> = restricted.iter().filter(|l| !accepted.contains(**l)).map(|l| l.to_string()).collect();
    if !missing.is_empty() {
        return Verdict::Missing(missing);
    }
    match restricted.len() {
        1 => Verdict::Give(restricted.iter().next().map_or_else(String::new, |l| l.to_string())),
        _ => Verdict::Give(LOCAL_DERIVATION_LICENCE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datumesh_core::inherit_licence;

    const FN: &str = "rates.example.org/src";
    const ISSUED: &str = "2024-03-15T16:00:00.000000Z";

    #[test]
    fn each_open_identifier_is_open_to_core_alone() {
        // Core gives an open licence alone (no declarations needed) with the reason `open`.
        for id in OPEN_IDENTIFIERS {
            assert!(is_open(id), "{id}");
            let r = inherit_licence([*id], None, FN, ISSUED, "local.invalid");
            assert_eq!(r.licence.as_deref(), Some(*id), "{id}");
            assert_eq!(r.reason, LicenceReason::Open, "{id}");
        }
    }

    #[test]
    fn a_public_information_licence_is_open_to_core_alone() {
        let id = "public:https://www.gov.example/licence";
        assert!(is_open(id));
        let r = inherit_licence([id], None, FN, ISSUED, "local.invalid");
        assert_eq!(r.licence.as_deref(), Some(id));
    }

    #[test]
    fn core_refuses_a_restricted_licence_alone_and_so_do_we_class_it_restricted() {
        for id in ["https://rates.example.org/licences/restricted", "cc0-1.0", "BSD-3-Clause"] {
            assert!(!is_open(id), "{id}");
            let r = inherit_licence([id], None, FN, ISSUED, "local.invalid");
            assert_eq!(r.reason, LicenceReason::NotDeclared, "{id}");
        }
    }

    #[test]
    fn decide_gives_the_one_licence_or_the_local_derivation() {
        let acc: BTreeSet<String> =
            ["https://a.example/r", "https://b.example/r"].iter().map(|s| s.to_string()).collect();
        let one = vec!["CC-BY-4.0".to_string(), "https://a.example/r".to_string()];
        assert_eq!(decide(&one, &acc), Verdict::Give("https://a.example/r".into()));
        let two = vec!["https://a.example/r".to_string(), "https://b.example/r".to_string()];
        assert_eq!(decide(&two, &acc), Verdict::Give(LOCAL_DERIVATION_LICENCE.into()));
        let missing = vec!["https://a.example/r".to_string(), "https://c.example/r".to_string()];
        assert_eq!(decide(&missing, &acc), Verdict::Missing(vec!["https://c.example/r".into()]));
    }
}
