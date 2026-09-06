//! `quote_identifier`: the one place a name is written into a definition.
//!
//! Every `pg_get_*def` on a real server prints a name through the same C function, and so does
//! every renderer here — `pg_constraint`'s five and `pg_index`'s three. It lived privately in
//! `pg_index` with the keyword half declared as a divergence; the keywords are measured now, so it
//! is one function in one place and the divergence is closed.

/// An identifier as PostgreSQL's `quote_identifier` writes it into a definition.
///
/// **Two rules, and the second is data.** A name is left bare only when it is what it would parse
/// back as — a leading lower-case letter or underscore, then lower-case letters, digits and
/// underscores — *and* is not a keyword PostgreSQL quotes. Everything else is delimited, with an
/// embedded `"` doubled.
///
/// Measured on 19beta1 through `quote_ident`, which is the same C function every `pg_get_*def`
/// calls:
///
/// ```text
/// plain    -> plain        position -> "position"    A     -> "A"      1a  -> "1a"
/// _a       -> _a           user     -> "user"        aB    -> "aB"     ""  -> """"
/// a_b_c    -> a_b_c        int      -> "int"         a b   -> "a b"    a"b -> "a""b"
/// name     -> name         setof    -> "setof"       a$b   -> "a$b"    über -> "über"
/// ```
///
/// **`$` is not a bare character here**, though the grammar admits it in an identifier: `a$b`
/// comes back quoted. That is not a guess about the parser, it is what the printer does, and the
/// per-column form of `pg_get_indexdef` says the same.
///
/// # Why the keyword list is carried, and why it is *this* list
///
/// This was declared as a divergence rather than approximated, and the reason was sound: a
/// keyword list taken from `sqlparser` is a different set and would quote `name` and `value`,
/// which a real server leaves bare. The answer is not to guess but to measure —
/// [`QUOTED_KEYWORDS`] is `SELECT word FROM pg_get_keywords() WHERE quote_ident(word) <> word`,
/// which is every keyword whose `catcode` is not `U`: the 78 reserved, the 23 reserved that may
/// still name a type or function, and the 64 unreserved that may not. The other 346 keywords are
/// left bare, `name` and `value` among them.
///
/// This list says what a *printer* quotes and is not a statement about what this node's parser
/// reserves — a different question, answered elsewhere, and deliberately not shared.
#[must_use]
pub fn quote_identifier(name: &str) -> String {
    let simple = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if simple && QUOTED_KEYWORDS.binary_search(&name).is_err() {
        return name.to_owned();
    }
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Every keyword PostgreSQL 19beta1's `quote_ident` delimits, in sort order for the binary search.
///
/// Captured rather than transcribed:
///
/// ```sql
/// SELECT word FROM pg_get_keywords() WHERE quote_ident(word) <> word ORDER BY word
/// ```
///
/// 165 of the 511 keywords that build reports. The predicate is exactly `catcode <> 'U'`, checked
/// per category on the same server: C 64/64, R 78/78, T 23/23, U 0/346.
static QUOTED_KEYWORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "graph_table",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

#[cfg(test)]
mod tests {
    use super::{QUOTED_KEYWORDS, quote_identifier};

    /// **The probe table, straight off 19beta1**: `SELECT id, quote_ident(id) FROM (VALUES …)`.
    ///
    /// It is here rather than in a corpus because `quote_ident` is a pure function of a string —
    /// there is no server state to replay — and because a table of pairs is the whole
    /// specification.
    #[test]
    fn every_probe_answers_what_quote_ident_answered() {
        for (written, quoted) in [
            // Keywords, one from each category that quote_ident delimits.
            ("position", "\"position\""),
            ("user", "\"user\""),
            ("order", "\"order\""),
            ("table", "\"table\""),
            ("select", "\"select\""),
            ("between", "\"between\""),
            ("coalesce", "\"coalesce\""),
            ("nullif", "\"nullif\""),
            ("greatest", "\"greatest\""),
            ("left", "\"left\""),
            ("right", "\"right\""),
            ("like", "\"like\""),
            ("ilike", "\"ilike\""),
            ("inner", "\"inner\""),
            ("outer", "\"outer\""),
            ("natural", "\"natural\""),
            ("full", "\"full\""),
            ("current_date", "\"current_date\""),
            ("setof", "\"setof\""),
            ("int", "\"int\""),
            ("integer", "\"integer\""),
            ("boolean", "\"boolean\""),
            // **Keywords a real server leaves bare** — the half that makes a borrowed keyword list
            // wrong, and the reason this one is measured.
            ("name", "name"),
            ("value", "value"),
            ("type", "type"),
            ("key", "key"),
            ("data", "data"),
            ("text", "text"),
            ("day", "day"),
            ("year", "year"),
            ("zone", "zone"),
            ("simple", "simple"),
            // Shape.
            ("a", "a"),
            ("_a", "_a"),
            ("a1", "a1"),
            ("a_b_c", "a_b_c"),
            ("_", "_"),
            ("A", "\"A\""),
            ("aB", "\"aB\""),
            ("1a", "\"1a\""),
            ("a b", "\"a b\""),
            ("a-b", "\"a-b\""),
            ("", "\"\""),
            ("über", "\"über\""),
            ("ORDER", "\"ORDER\""),
            ("Order", "\"Order\""),
            // **`$` is not bare**, though the grammar admits it in an identifier.
            ("a$b", "\"a$b\""),
            ("$a", "\"$a\""),
            // An embedded quote is doubled, and an apostrophe is not special.
            ("a\"b", "\"a\"\"b\""),
            ("a'b", "\"a'b\""),
        ] {
            assert_eq!(quote_identifier(written), quoted, "quote_ident({written})");
        }
    }

    /// The table is sorted, because the lookup is a binary search — and it is the size the capture
    /// reported, because a table that lost entries in an edit would quietly stop quoting.
    #[test]
    fn the_keyword_table_is_sorted_and_whole() {
        assert_eq!(QUOTED_KEYWORDS.len(), 165);
        assert!(QUOTED_KEYWORDS.windows(2).all(|pair| pair[0] < pair[1]));
        // Both ends, so a truncation at either shows.
        assert_eq!(QUOTED_KEYWORDS.first(), Some(&"all"));
        assert_eq!(QUOTED_KEYWORDS.last(), Some(&"xmltable"));
    }
}
