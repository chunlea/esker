//! `pg_index`, computed from the index definitions — and the primary key, which has none.
//!
//! What `ActiveRecord`'s `indexes()` reads, joined to `pg_class` twice:
//!
//! ```sql
//! SELECT DISTINCT i.relname, d.indisunique, d.indkey, pg_get_indexdef(d.indexrelid),
//!        pg_catalog.obj_description(i.oid, 'pg_class'), d.indisvalid, ARRAY(…) AS columns
//!   FROM pg_class t
//!   INNER JOIN pg_index d ON t.oid = d.indrelid
//!   INNER JOIN pg_class i ON d.indexrelid = i.oid
//!   LEFT JOIN pg_namespace n ON n.oid = t.relnamespace
//!  WHERE i.relkind IN ('i', 'I') AND d.indisprimary = 'f'
//!    AND t.relname = $table AND n.nspname = ANY (current_schemas(false))
//!  ORDER BY i.relname
//! ```
//!
//! Three oids in one statement — `t.oid`, `d.indrelid`, `d.indexrelid`, `i.oid` — which is why
//! [`crate::catalog::pg_relations`] exists and why nothing here computes one.
//!
//! # What the capture settled
//!
//! * **A primary key has a `pg_index` row**, `indisprimary` and `indisunique` both `t`, and its
//!   `indexrelid` is *not* its `indrelid`. Here the row key **is** the primary key and there is no
//!   separate index to describe — and the row is still the right answer, for the reason
//!   `pg_class` reports the constraint as a relation: a client can name `t_pkey`, and every
//!   statement `ActiveRecord` writes assumes it can find it.
//! * **`indkey` is an `int2vector`, and it prints space-separated**: a two-column index is `3 4`.
//!   `ActiveRecord` reads it with `row[2].split(" ").map(&:to_i)` — a *text* operation on the
//!   rendered value — so `text` here carries the same characters and means the same thing. That is
//!   the rule this phase follows for a type it does not have: the column is provided where the
//!   client's use of it is text-shaped, and refused where the use needs the array type
//!   (`pg_constraint.conkey`, which is subscripted in SQL).
//! * **`pg_get_indexdef` never raises.** An oid with no index behind it is **NULL** — including
//!   the oid of a *table*, which is the case a caller is most likely to hit — and so is a NULL
//!   argument.
//! * **The three-argument form is three different functions.** `pg_get_indexdef(oid, 0, true)` is
//!   the whole definition **without** the `public.` qualifier the one-argument form prints;
//!   `(oid, n, …)` for `n >= 1` is the *column name alone*; and past the last column it is the
//!   **empty string**, not NULL and not an error.

use std::borrow::Cow;

use crate::backend::Txn;
use crate::catalog::pg_relations::{RelKind, RelationRow, Relations};
use crate::catalog::{IndexKey, KeyPart, SchemaState, TableDef};
use crate::error::Result;
use crate::value::{ColumnType, Datum};

/// Every `pg_index` row this tenant has: one per index, and one per primary key.
pub fn rows(txn: &dyn Txn, tenant: u64) -> Result<Vec<Vec<Datum>>> {
    let relations = Relations::read(txn, tenant)?;
    let mut rows = Vec::new();
    for relation in relations.rows() {
        let Some(table) = relations.table(relation) else {
            continue;
        };
        let Some(key) = key_of(relation, table) else {
            continue;
        };
        rows.push(vec![
            Datum::Int8(relation.oid),
            Datum::Int8(oid_of_table(&relations, relation.table_id)),
            // **`indnatts` counts the payload too** — four for a two-column key with two
            // included columns — and `indnkeyatts` is what tells the halves apart. Both are
            // `smallint`, measured, not `integer`.
            Datum::Int2(i16::try_from(key.keys.len() + key.include.len()).unwrap_or(i16::MAX)),
            Datum::Int2(i16::try_from(key.keys.len()).unwrap_or(i16::MAX)),
            Datum::Bool(key.unique),
            Datum::Bool(key.nulls_not_distinct),
            Datum::Bool(key.primary),
            Datum::Bool(key.valid),
            Datum::Text(key.indkey(table)),
            Datum::Text(key.indoption()),
            // `indexprs` and `indpred` hold a `pg_node_tree` on a real server and the **printed
            // text** here, for the reason `pg_attrdef.adbin` does: `pg_get_expr` is the only way
            // a client reads either, and here that function is the identity
            // (`crate::plan::expr::CatalogFunc::PgGetExpr`). NULL where there is none, which is
            // what a client tests for — `ActiveRecord` prints a partial index's `WHERE` from it.
            key.indexprs().map_or(Datum::Null, Datum::Text),
            key.predicate.map_or(Datum::Null, |predicate| {
                Datum::Text(parenthesised(predicate))
            }),
            // **Last**, for the reason every late column of `pg_type` is last: `SELECT *` expands
            // in this order and a column inserted in PostgreSQL's own slot would move every one
            // after it. A real server puts it right after `indisprimary`.
            Datum::Bool(key.exclusion),
        ]);
    }
    Ok(rows)
}

/// `pg_get_indexdef(indexrelid [, column, pretty])`.
///
/// NULL for an oid that names no index — a table's oid included, measured — which is what makes it
/// safe to call over a column of oids nobody has filtered.
#[must_use]
pub fn index_definition(relations: &Relations, oid: Option<i64>, column: Option<i32>) -> Datum {
    let Some(oid) = oid else {
        return Datum::Null;
    };
    let Some(relation) = relations.by_oid(oid) else {
        return Datum::Null;
    };
    let Some(table) = relations.table(relation) else {
        return Datum::Null;
    };
    let Some(key) = key_of(relation, table) else {
        return Datum::Null;
    };
    let parts = key.listed(table);
    match column {
        // One column of it, by position, and the **empty string** past the last one. Measured:
        // `pg_get_indexdef(ix, 3, true)` over a two-column index is `''`, not NULL.
        Some(at) if at >= 1 => Datum::Text(
            usize::try_from(at - 1)
                .ok()
                .and_then(|at| key.per_column(table).get(at).cloned())
                .unwrap_or_default(),
        ),
        // Column 0 is the whole definition **unqualified** — the one difference between the two
        // forms, and it is not decoration: `ON ia` rather than `ON public.ia`.
        Some(_) => Datum::Text(definition(relation, table, &key, &parts, false)),
        None => Datum::Text(definition(relation, table, &key, &parts, true)),
    }
}

/// `CREATE [UNIQUE ]INDEX <name> ON [public.]<table> USING btree (<key>)[ WHERE (<predicate>)]`.
///
/// Every index a `CREATE INDEX` builds is a btree, which is not a simplification: `USING` is
/// refused by name in that DDL, so btree is the only method a user can ask for. An `EXCLUDE`
/// constraint's index is `gist`, which is what the constraint recorded — see [`Key::method`].
///
/// The `WHERE` is **re-printed parenthesised** whatever was written — `WHERE published_on IS NOT
/// NULL` comes back `WHERE (published_on IS NOT NULL)` and `WHERE (a > 10)` comes back
/// `WHERE (a > 10)`, one pair either way. Measured; and it matters beyond looks, because
/// `ActiveRecord` recovers a partial index's predicate by scanning this string.
/// An identifier as PostgreSQL's `quote_ident` writes it into a definition.
///
/// **Quoted unless it is already what it would parse back as**: a leading letter or underscore,
/// then letters, digits, underscores and `$`, all lower case. `"Quoted"` and `"Col A"` get their
/// quotes and `plain` does not — measured on 19beta1, in both the whole definition and the
/// per-column form. It matters beyond looks: `ActiveRecord` recovers an index's columns by
/// reading this string, and a mixed-case name written bare comes back as a different name.
///
/// **A reserved keyword is quoted too and is not quoted here** — `"select"` as a column name is
/// measured and diverges. PostgreSQL quotes its *reserved* words, which is a specific list this
/// node does not carry; `sqlparser`'s keyword lists are a different set and using one would quote
/// `name` and `value`, which a real server leaves bare. Declared rather than approximated.
fn quote_identifier(name: &str) -> String {
    let bare = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
    if bare {
        return name.to_owned();
    }
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn definition(
    relation: &RelationRow,
    table: &TableDef,
    key: &Key<'_>,
    parts: &[String],
    qualified: bool,
) -> String {
    let mut out = format!(
        "CREATE {}INDEX {} ON {}{}{} USING {} ({})",
        if key.unique { "UNIQUE " } else { "" },
        quote_identifier(&relation.name),
        // **`ON ONLY` for an index on a partitioned table** — the word `ONLY` in the definition of
        // the index that covers *every* partition, which reads backwards and is what a real server
        // prints. It says the index relation itself holds no entries: the partitions' own indexes
        // do. Measured, and it does not change when partitions are attached or detached.
        if table.partition_by.is_some() {
            "ONLY "
        } else {
            ""
        },
        if qualified { "public." } else { "" },
        quote_identifier(&table.name),
        key.method,
        parts.join(", ")
    );
    // **`INCLUDE (…)` comes first of the three tails**: after the key list and before the
    // predicate — `USING btree (firm_id) INCLUDE (name) WHERE (account_id IS NOT NULL)`, measured,
    // and `pg_indexes.indexdef` carries the same string.
    if !key.include.is_empty() {
        let printed: Vec<String> = key
            .include
            .iter()
            .filter_map(|&at| table.columns.get(at))
            .map(|column| quote_identifier(&column.name))
            .collect();
        out.push_str(" INCLUDE (");
        out.push_str(&printed.join(", "));
        out.push(')');
    }
    // **After the key list and before the `WHERE`**, which is the order a real server prints
    // them in: `USING btree (a, b) NULLS NOT DISTINCT WHERE (c IS NOT NULL)`. Measured, and it is
    // printed for a **non-unique** index too, where it can refuse nothing.
    if key.nulls_not_distinct {
        out.push_str(" NULLS NOT DISTINCT");
    }
    if let Some(predicate) = key.predicate {
        out.push_str(" WHERE ");
        out.push_str(&parenthesised(predicate));
    }
    out
}

/// One pair of parentheses around a stored expression, and never two.
///
/// The text a `WHERE` is stored with has had its own outer pair removed when it was lowered
/// (`crate::parse::lower::unwrap_nested`), so this is where PostgreSQL's pair goes back on — and
/// each operand of an `AND`/`OR` chain gets a pair of its own, which is the server's own rule for
/// re-printing a boolean (`crate::catalog::parenthesised_operands`).
fn parenthesised(expr: &str) -> String {
    format!("({})", crate::catalog::parenthesised_operands(expr))
}

/// One index's key, however it is stored.
///
/// A primary key and an index are two records of different shapes describing the same thing, and
/// every column of `pg_index` is a function of this — so they are read into one and the row is
/// built once.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each one mirrors a boolean column of pg_index -- indisunique, indisprimary, \
              indisvalid, indnullsnotdistinct -- and grouping them would name a thing the catalog \
              does not have"
)]
struct Key<'a> {
    /// `NULLS NOT DISTINCT`: a column of `pg_index` and **not** part of the key, which is why it
    /// prints after the column list rather than inside it.
    nulls_not_distinct: bool,
    /// Borrowed from the index, and **owned** for a primary key — whose parts are columns that
    /// live in `TableDef::primary_key` as bare positions and have no `IndexKey` to point at.
    keys: Cow<'a, [IndexKey]>,
    unique: bool,
    primary: bool,
    valid: bool,
    /// `indisexclusion`, and it is **not** `indisunique`: an exclusion index refuses a row an
    /// operator relates, not one that is equal, and a real server reports `f`/`t` where a unique
    /// index reports `t`/`f`. Measured.
    exclusion: bool,
    /// The access method `pg_get_indexdef` names and `pg_am` agrees with. `btree` for everything
    /// this node builds; `gist` for an exclusion constraint, which is recorded and enforced by a
    /// scan (`crate::exec::dml::check_exclusions`).
    method: &'static str,
    /// A partial index's predicate as it is stored — one pair of parentheses short of how it
    /// prints ([`parenthesised`]).
    predicate: Option<&'a str>,
    /// `INCLUDE (…)`: the non-key payload columns, by position. Empty for a primary key and for
    /// every index that names none.
    include: &'a [usize],
}

impl Key<'_> {
    /// `indkey`: the attribute numbers, space-separated — an `int2vector`'s own text.
    ///
    /// The numbers are `pg_attribute`'s, not positions in `TableDef::columns`
    /// ([`super::pg_relations::attnum_of`]): a keyless table hides a column in slot 0, and
    /// numbering from there would make `a.attnum = ANY(i.indkey)` name the column after the one
    /// the index is on.
    ///
    /// An expression part is **`0`**, which is what makes the whole column readable: attribute
    /// numbers are one-based there, so zero is free, and `ActiveRecord` branches on exactly this
    /// (`indkey.include?(0)`) to decide whether to believe its column list or re-read the
    /// definition text. A two-part key over `a` and `lower(b)` is `2 0`, measured.
    ///
    /// **The included columns are in it**, after the key's, and only `indnkeyatts` separates the
    /// two halves: the suite's index is `2 3 4 5` with `indnatts = 4` and `indnkeyatts = 2`. A
    /// client that reads `indkey` alone reports a four-column index — and `ActiveRecord`'s schema
    /// dumper is such a client.
    fn indkey(&self, table: &TableDef) -> String {
        self.keys
            .iter()
            .map(|key| {
                key.position()
                    .map_or(0, |at| super::pg_relations::attnum_of(table, at))
            })
            .chain(
                self.include
                    .iter()
                    .map(|&at| super::pg_relations::attnum_of(table, at)),
            )
            .map(|attnum| attnum.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `indexprs`: the key's expressions, in key order, or `None` when every part is a column.
    ///
    /// Comma-joined, which is what `pg_get_expr` over a node **list** prints — measured for a
    /// two-expression key.
    fn indexprs(&self) -> Option<String> {
        let printed: Vec<String> = self
            .keys
            .iter()
            .filter_map(|key| match &key.part {
                KeyPart::Column(_) => None,
                KeyPart::Expression { expr, shape, .. } => Some(shape.printed(expr)),
            })
            .collect();
        (!printed.is_empty()).then(|| printed.join(", "))
    }

    /// The key parts as the definition lists them, and as the per-column form prints them.
    ///
    /// A column is its name. An expression is its printed form, wrapped in **one more** pair of
    /// parentheses unless it is a bare call: `lower(b)` stays `lower(b)` where `(b IS NULL)`
    /// becomes `((b IS NULL))` and `1` becomes `(1)`. Measured across a call, an operator, a
    /// comparison, a cast and two constants — the rule is about the node, not about the text,
    /// which is why [`IndexKey::Expression::call`] is stored rather than guessed at here.
    fn listed(&self, table: &TableDef) -> Vec<String> {
        self.keys
            .iter()
            .map(|key| {
                let part = match &key.part {
                    KeyPart::Column(at) => table
                        .columns
                        .get(*at)
                        .map(|column| quote_identifier(&column.name))
                        .unwrap_or_default(),
                    KeyPart::Expression { expr, shape, .. } => shape.listed(expr),
                };
                format!("{part}{}", key.order.suffix())
            })
            .collect()
    }

    /// The key parts as `pg_get_indexdef(oid, n, pretty)` prints them one at a time.
    ///
    /// The same as [`Key::listed`] except for a **value** expression, which the key list wraps
    /// and this does not — the one cell where PostgreSQL's two forms disagree
    /// ([`crate::catalog::ExprShape`]).
    fn per_column(&self, table: &TableDef) -> Vec<String> {
        self.keys
            .iter()
            .map(|key| match &key.part {
                KeyPart::Column(at) => table
                    .columns
                    .get(*at)
                    .map(|column| quote_identifier(&column.name))
                    .unwrap_or_default(),
                KeyPart::Expression { expr, shape, .. } => shape.per_column(expr),
            })
            .collect()
    }

    /// `indoption`: one bitmask per key part, space-separated, like [`Key::indkey`].
    ///
    /// PostgreSQL's `INDOPTION_DESC` and `INDOPTION_NULLS_FIRST` — a two-column key over an
    /// ascending part and a `DESC` one is `0 3`, measured.
    fn indoption(&self) -> String {
        self.keys
            .iter()
            .map(|key| key.order.indoption().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The key a relation is, or `None` for one that is not an index at all.
fn key_of<'a>(relation: &RelationRow, table: &'a TableDef) -> Option<Key<'a>> {
    match relation.kind {
        RelKind::Exclusion => {
            let exclude = table.excludes.get(relation.exclude_at?)?;
            Some(Key {
                nulls_not_distinct: false,
                // One part, always an expression: a scalar key needs `btree_gist` and is refused,
                // so there is no column form. `ExprShape::Call` is what `daterange(a, b)` is, and
                // it is the one shape the key list prints without adding a pair of its own.
                keys: Cow::Owned(vec![IndexKey {
                    part: KeyPart::Expression {
                        expr: exclude.key.clone(),
                        shape: crate::catalog::ExprShape::Call,
                        ty: ColumnType::Text,
                    },
                    order: crate::catalog::KeyOrder::ASCENDING,
                }]),
                // **Not unique.** The `&&` lives only in `pg_constraint`; the index behind an
                // exclusion constraint is a plain one, measured.
                unique: false,
                primary: false,
                // Nothing: `INCLUDE` is a `CREATE INDEX` clause, and this index is a constraint's.
                include: &[],
                predicate: exclude.predicate.as_deref(),
                valid: true,
                exclusion: true,
                method: "gist",
            })
        }
        RelKind::Index => {
            let index = table.indexes.get(relation.index_at?)?;
            Some(Key {
                nulls_not_distinct: index.nulls_not_distinct,
                keys: Cow::Borrowed(&index.keys),
                unique: index.unique,
                primary: false,
                exclusion: false,
                method: "btree",
                predicate: index.predicate.as_deref(),
                // **`indisvalid` is the schema state, read honestly.** An index that is not
                // `Public` is one no reader may use (ADR 0020), and `indisvalid` is exactly the
                // column a client checks before trusting one — `ActiveRecord`'s `indexes()`
                // selects it. Reporting `t` for a half-built index would be the wrong answer in
                // the one place a client asked the right question.
                valid: index.state == SchemaState::Public,
                include: &index.include,
            })
        }
        RelKind::PrimaryKey => Some(Key {
            exclusion: false,
            method: "btree",
            // A primary key's columns are `NOT NULL`, so the clause could change nothing and a
            // real server reports `f` for it. Measured.
            nulls_not_distinct: false,
            keys: table
                .primary_key
                .iter()
                .map(|at| IndexKey::column(*at))
                .collect(),
            unique: true,
            primary: true,
            valid: true,
            predicate: None,
            // **A primary key never has one.** `ALTER TABLE … ADD PRIMARY KEY … INCLUDE` exists
            // on a real server and does not reach this node: the action itself is `0A000`.
            include: &[],
        }),
        RelKind::Table | RelKind::Sequence => None,
    }
}

/// A table's own oid, out of the same snapshot — never recomputed.
fn oid_of_table(relations: &Relations, table_id: u64) -> i64 {
    relations
        .rows()
        .find(|row| row.kind == RelKind::Table && row.table_id == table_id)
        .map_or(0, |row| row.oid)
}

/// The columns of `pg_index`, in PostgreSQL's own order.
pub const INDEX_COLUMNS: &[(&str, ColumnType)] = &[
    ("indexrelid", ColumnType::Int8),
    ("indrelid", ColumnType::Int8),
    ("indnatts", ColumnType::Int2),
    // **The column that separates the key from the payload**, and PostgreSQL puts it right here,
    // straight after the total. `smallint` like its neighbour.
    ("indnkeyatts", ColumnType::Int2),
    ("indisunique", ColumnType::Bool),
    ("indnullsnotdistinct", ColumnType::Bool),
    ("indisprimary", ColumnType::Bool),
    ("indisvalid", ColumnType::Bool),
    ("indkey", ColumnType::Text),
    // Like `indkey` an `int2vector` on a real server, and text here for the same reason: the way
    // a client uses it is text-shaped. `ActiveRecord` reads the ordering out of
    // `pg_get_indexdef`'s string rather than from here, so this is the honest record of a fact
    // rather than a column anything depends on.
    ("indoption", ColumnType::Text),
    ("indexprs", ColumnType::Text),
    ("indpred", ColumnType::Text),
    // **Last**, see the row it fills.
    ("indisexclusion", ColumnType::Bool),
];
