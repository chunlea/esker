//! `regproc`: an oid that prints as a **function's** name.
//!
//! OID 24, `typlen` 4, `typcategory` `N`, `typinput` `regprocin`, `typarray` 1008. It is
//! `pg_type.typinput`'s type, and the only column of it this node has — a real server declares it
//! on forty, all in catalogs this node does not serve
//! ([ADR 0098](../../../../docs/adr/0098-regproc-is-an-oid-that-prints-as-a-function.md)).
//!
//! # An oid no function has prints as the number
//!
//! Measured: `42::regproc` is `int4in` and **`24::regproc` is `24`**. There are far more oids
//! without a function than `pg_proc` has rows, so the digits are the common case. That is why the
//! name rides in the datum beside the oid, exactly as a `regtype`'s does — `esker-keys` must not
//! have a catalog (invariant 7), so the name is resolved where the value is produced.
//!
//! # It compares as an oid, and that is what refuses a name
//!
//! `typinput = 'array_in'` is **`22P02 invalid input syntax for type oid: "array_in"`** on a real
//! server, because the unadorned literal is resolved against the oid the comparison is really
//! about. `typinput = 'array_in'::regproc` and `typinput::text = 'array_in'` both answer 636.
//! This node's own `tests/array_delimiter.rs` wrote the first form and passed, because the column
//! was `text` here — a test passing on the mechanism it was not testing.

use crate::error::{Result, SqlError};

/// Every input function this node names in `pg_type.typinput`, with the oid a real server gives
/// it.
///
/// **Measured**, one query against 19beta1 —
/// `SELECT proname, oid FROM pg_proc WHERE proname IN (…) AND pronamespace = 'pg_catalog'` — and
/// the list of names is exactly what `pg_catalog::typinput` can return, so a type added there
/// without a row here prints its digits and is caught by
/// `tests/reg_proc.rs::every_typinput_resolves_to_a_function`.
///
/// **`domain_in`, `enum_in` and `record_in` were the three that got away**, and the way they did
/// is worth keeping: `TypeKind::typinput` started answering them for a **user** type, and the test
/// above reads `pg_type`, where a fresh node has no user types at all — so the rule was already
/// broken and nothing could see it. It went red the day the five `information_schema` domains
/// became *built-in* rows (`debts-v1.1.md` #37, ADR 0103) and one of those four names finally
/// appeared in a row the test reads. `range_in` was here only because a range is also a built-in
/// type. Their oids are 19beta1's own, measured with the same one query.
const BUILT_IN: &[(&str, u32)] = &[
    ("array_in", 750),
    ("bit_in", 1564),
    ("boolin", 1242),
    ("box_in", 123),
    ("bpcharin", 1044),
    ("byteain", 1244),
    ("cash_in", 886),
    ("charin", 1245),
    ("cidr_in", 1267),
    ("circle_in", 1450),
    ("date_in", 1084),
    ("domain_in", 2597),
    ("enum_in", 3506),
    ("float4in", 200),
    ("float8in", 214),
    ("inet_in", 910),
    ("int2in", 38),
    ("int2vectorin", 40),
    ("int4in", 42),
    ("int8in", 460),
    ("interval_in", 1160),
    ("json_in", 321),
    ("jsonb_in", 3806),
    ("line_in", 1490),
    ("lseg_in", 119),
    ("macaddr_in", 436),
    ("namein", 34),
    ("numeric_in", 1701),
    ("oidin", 1798),
    ("oidvectorin", 54),
    ("path_in", 121),
    ("point_in", 117),
    ("poly_in", 347),
    ("range_in", 3834),
    ("record_in", 2290),
    ("regclassin", 2218),
    // **`regprocin` is in this list because `regproc` is a type this node has**, and its own
    // `typinput` is itself. `tests/reg_proc.rs::every_typinput_resolves_to_a_function` found it
    // the first time it ran.
    ("regprocin", 44),
    ("regtypein", 2220),
    ("textin", 46),
    ("time_in", 1143),
    ("timestamp_in", 1312),
    ("timestamptz_in", 1150),
    ("tsqueryin", 3612),
    ("tsvectorin", 3610),
    ("uuid_in", 2952),
    ("varbit_in", 1579),
    ("varcharin", 1046),
    ("void_in", 2298),
    ("xml_in", 2893),
];

/// The four an **extension** brings, whose oids a real server allocates at `CREATE EXTENSION`.
///
/// They are not constants anywhere: `citextin`'s oid differs between two databases that both have
/// the extension, because it comes from the same counter every user object does. So these carry
/// this node's own, taken from the user range and stable within a build — the *name* is what a
/// client reads off `typinput`, and it is right either way. Declared in
/// `tests/reg_proc.rs::an_extensions_input_function_has_no_fixed_oid`.
const EXTENSION: &[(&str, u32)] = &[
    ("citextin", 16_500),
    ("hstore_in", 16_501),
    ("lquery_in", 16_502),
    ("ltree_in", 16_503),
];

/// The oid a function name has, or `None` for a name no function here carries.
#[must_use]
pub fn oid_of(name: &str) -> Option<u32> {
    BUILT_IN
        .iter()
        .chain(EXTENSION)
        .find(|(known, _)| *known == name)
        .map(|(_, oid)| *oid)
}

/// What `regprocout` writes: the function's name, or the oid's digits when none has it.
#[must_use]
pub fn to_text(oid: u32) -> String {
    name_of(oid).map_or_else(|| oid.to_string(), str::to_owned)
}

/// The name an oid prints as, or `None` when no function has it.
#[must_use]
pub fn name_of(oid: u32) -> Option<&'static str> {
    BUILT_IN
        .iter()
        .chain(EXTENSION)
        .find(|(_, known)| *known == oid)
        .map(|(name, _)| *name)
}

/// PostgreSQL's `regprocin`.
///
/// A name is looked up; **digits are an oid**, taken as written whether or not a function has it,
/// which is the half that lets `24::regproc` round-trip. A name nothing has is
/// `42883 function "x" does not exist` — an *undefined function*, not a syntax error, because the
/// input function resolves rather than parses. Whitespace is trimmed and a schema qualification is
/// accepted and dropped: `'pg_catalog.int4in'::regproc` is `int4in`, measured.
pub fn from_text(text: &str) -> Result<u32> {
    let body = text.trim();
    if let Ok(oid) = body.parse::<u32>() {
        return Ok(oid);
    }
    let bare = body.rsplit_once('.').map_or(body, |(_, name)| name);
    // **The name is quoted in the message**, which is the oracle's own sentence:
    // `'nosuchfn'::regproc` is `42883 function "nosuchfn" does not exist`.
    oid_of(bare).ok_or_else(|| SqlError::UndefinedFunction(format!("\"{bare}\"")))
}

#[cfg(test)]
mod tests {
    use super::{from_text, name_of, to_text};

    #[test]
    fn an_oid_no_function_has_prints_as_the_number() {
        assert_eq!(to_text(42), "int4in");
        assert_eq!(to_text(24), "24");
        assert_eq!(name_of(24), None);
    }

    #[test]
    fn a_name_resolves_and_digits_are_taken_as_written() {
        assert_eq!(from_text("int4in").unwrap(), 42);
        assert_eq!(from_text("  int4in ").unwrap(), 42);
        assert_eq!(from_text("pg_catalog.int4in").unwrap(), 42);
        assert_eq!(from_text("24").unwrap(), 24);
        assert!(from_text("nosuchfn").is_err());
    }

    #[test]
    fn no_two_functions_share_an_oid() {
        let mut oids: Vec<u32> = super::BUILT_IN
            .iter()
            .chain(super::EXTENSION)
            .map(|(_, oid)| *oid)
            .collect();
        oids.sort_unstable();
        let before = oids.len();
        oids.dedup();
        assert_eq!(before, oids.len(), "two input functions share an oid");
    }
}
