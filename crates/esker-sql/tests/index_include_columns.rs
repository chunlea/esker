//! **`pg_get_indexdef(index, n, true)` names an `INCLUDE` column too.**
//!
//! `SchemaIndexIncludeColumnsTest#test_schema_dumps_index_included_columns` expects
//!
//! ```ruby
//! t.index ["firm_id", "type"], name: "company_include_index", include: ["name", "account_id"]
//! ```
//!
//! and got `["firm_id", "type", "", ""]` — the payload positions as empty strings.
//!
//! # Why the empty strings are the bug and not the extra positions
//!
//! `ActiveRecord`'s `indexes` query asks for **one call per subscript of `indkey`**, which
//! includes the payload:
//!
//! ```sql
//! ARRAY(SELECT pg_get_indexdef(d.indexrelid, k + 1, true)
//!       FROM generate_subscripts(d.indkey, 1) AS k ORDER BY k) AS columns
//! ```
//!
//! and then drops the payload **by name**:
//!
//! ```ruby
//! columns.reject! { |c| include_columns.include?(c) }   # include_columns from the INCLUDE (…) clause
//! ```
//!
//! So it needs every position to answer with its column's name; `""` matches nothing in
//! `["name", "account_id"]` and survives the reject. Measured on 19beta1 — all four positions
//! answer, and the last two are the payload:
//!
//! ```text
//! k | pg_get_indexdef(…, k+1, true)      indkey  | indnatts | indnkeyatts
//! 0 | firm_id                            1 2 3 4 |        4 |           2
//! 1 | type
//! 2 | name
//! 3 | account_id
//! ```
//!
//! **This is not "stop at `indnkeyatts`".** The caller is right to read the whole `indkey`; what
//! was short is the per-column form, which mapped the key parts and not the payload.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE companies (firm_id int8, type text, name text, account_id int8)",
    "CREATE INDEX company_include_index ON companies (firm_id, type) INCLUDE (name, account_id)",
];

/// Each of the four positions, by name.
#[test]
fn every_indkey_position_answers_with_its_column() {
    let mut node = parity::Node::new(FIXTURE);
    for (at, expected) in [(1, "firm_id"), (2, "type"), (3, "name"), (4, "account_id")] {
        assert_eq!(
            node.rows(&format!(
                "SELECT pg_get_indexdef('company_include_index'::regclass, {at}, true)"
            )),
            [[expected.to_owned()]],
            "position {at}"
        );
    }
    // And past the last one it is the empty string, not NULL — measured, and the reason the
    // payload's empty strings were not obviously wrong.
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('company_include_index'::regclass, 9, true)"),
        [[String::new()]]
    );
}

/// **The query `ActiveRecord` actually sends**, so the fix is checked where the suite reads it.
#[test]
fn the_adapters_columns_array_names_the_payload() {
    let mut node = parity::Node::new(FIXTURE);
    let rows = node.rows(
        "SELECT ARRAY(SELECT pg_get_indexdef(d.indexrelid, k + 1, true) \
         FROM generate_subscripts(d.indkey, 1) AS k ORDER BY k) \
         FROM pg_index d JOIN pg_class i ON d.indexrelid = i.oid \
         WHERE i.relname = 'company_include_index'",
    );
    assert_eq!(rows, [["{firm_id,type,name,account_id}".to_owned()]]);
}

/// The `INCLUDE (…)` clause the adapter matches those names against, so both halves of its filter
/// are pinned in one place.
#[test]
fn the_definition_carries_the_include_clause() {
    let mut node = parity::Node::new(FIXTURE);
    let rows = node.rows("SELECT pg_get_indexdef('company_include_index'::regclass)");
    let definition = &rows[0][0];
    assert!(
        definition.contains("INCLUDE (name, account_id)"),
        "the adapter recovers the payload by scanning this: {definition}"
    );
}
