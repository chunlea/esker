//! The catalog functions that print a definition, starting with `format_type`.
//!
//! `pg_catalog` says what a relation *is* as rows; these say what it *looks like* as text. They
//! are the half of the catalog surface `ActiveRecord` reads with string operations rather than
//! with joins — `format_type` in `columns()`, `pg_get_indexdef` in `indexes()`,
//! `pg_get_constraintdef` in `foreign_keys()` — and they are ordinary row functions: a value of
//! their arguments, evaluated once per row, with no state.
//!
//! # `format_type(oid, typmod)`, and the four rules a reader would get wrong
//!
//! Measured against PostgreSQL 19beta1
//! (`esker-rails-harness/captures/pg19_format_type.txt`, 28 statements):
//!
//! * **It almost never fails.** An oid with no `pg_type` row is not an error, it is the three
//!   characters `???`; oid `0` is a single `-`; a NULL oid is NULL. A node that raised instead
//!   would break `columns()` on the first column whose type it does not have — which is exactly
//!   the case a client asks about.
//! * **A typmod on a type that takes none is silently ignored**: `format_type(23, 4)` is
//!   `integer`, where `'integer(4)'::regtype` is `42601`. The two halves of the round trip do not
//!   agree about what is legal, and this is the permissive half.
//! * **Each family's "no typmod" threshold is its own, and it is not `-1`.** For `varchar` and
//!   `character` the typmod is the length **plus four**, so `0`, `1` and `4` all print bare and
//!   `5` is the first real one — `character varying(1)`. For `timestamp` the typmod is the
//!   precision itself, so **`0` is a real precision** and prints `timestamp(0) without time zone`;
//!   only a negative is bare.
//! * **A NULL typmod and a typmod of `-1` are different arguments**, and for exactly one type they
//!   give different answers: `format_type(1042, NULL)` is `character` and `format_type(1042, -1)`
//!   is `bpchar`. PostgreSQL calls the distinction `typemod_given`, and `character(n)` is the one
//!   type whose bare spelling is not its parameterised one. Everything else is unmoved by it.
//!
//! It does not clamp and it does not validate: `format_type(1114, 7)` is
//! `timestamp(7) without time zone`, a precision no `CREATE TABLE` will store, because DDL clamps
//! `7` to `6` with a `WARNING` and this function prints what it is handed.
//!
//! # Whose types does it know?
//!
//! This server's, which is the rule `pg_type` already follows (`crate::catalog::pg_catalog`). An
//! oid this node has no type for is `???` — and that is the answer a *real* server gives for an
//! oid **it** has no row for, so the shape of the answer is identical and only the set of oids
//! differs. `format_type(1082, NULL)` is `date` there and `???` here, declared in the corpus, and
//! it closes one type at a time as types arrive.

use crate::value::{self, ColumnType, Datum, PgType};

/// What a real server prints for an oid that has no `pg_type` row.
const UNKNOWN_TYPE: &str = "???";

/// What a real server prints for `InvalidOid`.
const INVALID_OID: &str = "-";

/// `format_type(oid, typmod)`.
///
/// `oid` is `None` for SQL NULL, and so is `typmod` — which is not the same as `Some(-1)`; see the
/// module note.
#[must_use]
pub fn format_type(oid: Option<i64>, typmod: Option<i32>) -> Datum {
    let Some(oid) = oid else {
        return Datum::Null;
    };
    if oid == 0 {
        return Datum::Text(INVALID_OID.to_owned());
    }
    let Some(ty) = type_of_oid(oid) else {
        return Datum::Text(UNKNOWN_TYPE.to_owned());
    };
    Datum::Text(spell(ty, typmod))
}

/// The name and typmod together, as `format_type` writes them.
fn spell(ty: ColumnType, typmod: Option<i32>) -> String {
    match typmod {
        // A typmod was given and it is not negative: print the parameterised spelling, which
        // `value::format_type` already holds because two wire surfaces are defined as it.
        Some(typmod) if typmod >= 0 => value::format_type(ty, typmod),
        // A typmod was given and it is negative. `character` is the one type this differs for:
        // `bpchar` is what a `character` column with no length is called, and `value::format_type`
        // answers it for exactly this input.
        Some(_) => value::format_type(ty, value::NO_TYPMOD),
        // No typmod argument at all — `format_type(oid, NULL)`. Every type prints its bare SQL
        // name, `character` included, which is the one place this differs from the line above.
        None => match ty {
            ColumnType::Bpchar => ty.name().to_owned(),
            other => value::format_type(other, value::NO_TYPMOD),
        },
    }
}

/// The type an oid names, if this server has it.
///
/// Derived from [`ColumnType::ALL`] rather than written out, for the reason `pg_type`'s rows are:
/// a type cannot be added to this node and left out of the function that prints it.
#[must_use]
pub fn type_of_oid(oid: i64) -> Option<ColumnType> {
    ColumnType::ALL
        .into_iter()
        .find(|ty| i64::from(ty.oid()) == oid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture's own rows, for the types this node has. The corpus replays all 28 statements;
    /// these are the four rules stated one at a time, so a failure names which one moved.
    #[test]
    fn the_four_rules_are_the_captured_ones() {
        // An unknown oid is not an error, and neither is oid 0.
        assert_eq!(format_type(Some(999_999), None), Datum::Text("???".into()));
        assert_eq!(
            format_type(Some(999_999), Some(4)),
            Datum::Text("???".into())
        );
        assert_eq!(format_type(Some(0), None), Datum::Text("-".into()));
        assert_eq!(format_type(None, None), Datum::Null);
        assert_eq!(format_type(None, Some(4)), Datum::Null);

        // A typmod on a type that takes none is ignored.
        assert_eq!(format_type(Some(23), None), Datum::Text("integer".into()));
        assert_eq!(
            format_type(Some(23), Some(-1)),
            Datum::Text("integer".into())
        );
        assert_eq!(
            format_type(Some(23), Some(4)),
            Datum::Text("integer".into())
        );

        // `varchar` is length + 4, so 0, 1 and 4 are all bare and 5 is the first real one.
        for bare in [None, Some(-1), Some(0), Some(1), Some(4)] {
            assert_eq!(
                format_type(Some(1043), bare),
                Datum::Text("character varying".into()),
                "varchar with typmod {bare:?}"
            );
        }
        assert_eq!(
            format_type(Some(1043), Some(5)),
            Datum::Text("character varying(1)".into())
        );
        assert_eq!(
            format_type(Some(1043), Some(9)),
            Datum::Text("character varying(5)".into())
        );
        assert_eq!(
            format_type(Some(1043), Some(1028)),
            Datum::Text("character varying(1024)".into())
        );

        // `timestamp` is the precision itself, so 0 is real and only a negative is bare.
        assert_eq!(
            format_type(Some(1114), None),
            Datum::Text("timestamp without time zone".into())
        );
        assert_eq!(
            format_type(Some(1114), Some(-2)),
            Datum::Text("timestamp without time zone".into())
        );
        assert_eq!(
            format_type(Some(1114), Some(0)),
            Datum::Text("timestamp(0) without time zone".into())
        );
        assert_eq!(
            format_type(Some(1114), Some(3)),
            Datum::Text("timestamp(3) without time zone".into())
        );
        // It does not clamp: 7 is a precision no column can hold.
        assert_eq!(
            format_type(Some(1114), Some(7)),
            Datum::Text("timestamp(7) without time zone".into())
        );
        assert_eq!(
            format_type(Some(1184), Some(6)),
            Datum::Text("timestamp(6) with time zone".into())
        );

        // A NULL typmod and a typmod of -1 differ for `character` and for nothing else.
        assert_eq!(
            format_type(Some(1042), None),
            Datum::Text("character".into())
        );
        assert_eq!(
            format_type(Some(1042), Some(-1)),
            Datum::Text("bpchar".into())
        );
        assert_eq!(
            format_type(Some(1042), Some(7)),
            Datum::Text("character(3)".into())
        );
    }

    /// Every type this node has answers, which is what keeps the function from being written out.
    #[test]
    fn every_type_this_node_has_is_named() {
        for ty in ColumnType::ALL {
            let named = format_type(Some(i64::from(ty.oid())), None);
            assert_eq!(
                named,
                Datum::Text(spell(ty, None)),
                "{ty:?} is not reachable by its own oid"
            );
            assert_ne!(named, Datum::Text(UNKNOWN_TYPE.to_owned()), "{ty:?}");
        }
    }
}
