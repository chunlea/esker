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
            Datum::Int2(i16::try_from(key.keys.len()).unwrap_or(i16::MAX)),
            Datum::Bool(key.unique),
            // `NULLS NOT DISTINCT` is a clause this node's `CREATE INDEX` refuses by name, so no
            // index here has it. Measured: the default is `f` and the clause prints after the
            // column list when it is `t`.
            Datum::Bool(false),
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
/// Every index here is a btree, which is not a simplification: `USING` is refused by name in the
/// DDL, so btree is the only access method this node has and naming another would be a claim.
///
/// The `WHERE` is **re-printed parenthesised** whatever was written — `WHERE published_on IS NOT
/// NULL` comes back `WHERE (published_on IS NOT NULL)` and `WHERE (a > 10)` comes back
/// `WHERE (a > 10)`, one pair either way. Measured; and it matters beyond looks, because
/// `ActiveRecord` recovers a partial index's predicate by scanning this string.
fn definition(
    relation: &RelationRow,
    table: &TableDef,
    key: &Key<'_>,
    parts: &[String],
    qualified: bool,
) -> String {
    let mut out = format!(
        "CREATE {}INDEX {} ON {}{} USING btree ({})",
        if key.unique { "UNIQUE " } else { "" },
        relation.name,
        if qualified { "public." } else { "" },
        table.name,
        parts.join(", ")
    );
    if let Some(predicate) = key.predicate {
        out.push_str(" WHERE ");
        out.push_str(&parenthesised(predicate));
    }
    out
}

/// One pair of parentheses around a stored expression, and never two.
///
/// The text a `WHERE` is stored with has had its own outer pair removed when it was lowered
/// (`crate::parse::lower::unwrap_nested`), so this is where PostgreSQL's pair goes back on.
fn parenthesised(expr: &str) -> String {
    format!("({expr})")
}

/// One index's key, however it is stored.
///
/// A primary key and an index are two records of different shapes describing the same thing, and
/// every column of `pg_index` is a function of this — so they are read into one and the row is
/// built once.
struct Key<'a> {
    /// Borrowed from the index, and **owned** for a primary key — whose parts are columns that
    /// live in `TableDef::primary_key` as bare positions and have no `IndexKey` to point at.
    keys: Cow<'a, [IndexKey]>,
    unique: bool,
    primary: bool,
    valid: bool,
    /// A partial index's predicate as it is stored — one pair of parentheses short of how it
    /// prints ([`parenthesised`]).
    predicate: Option<&'a str>,
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
    fn indkey(&self, table: &TableDef) -> String {
        self.keys
            .iter()
            .map(|key| {
                key.position()
                    .map_or(0, |at| super::pg_relations::attnum_of(table, at))
                    .to_string()
            })
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
                        .map(|column| column.name.clone())
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
                    .map(|column| column.name.clone())
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
        RelKind::Index => {
            let index = table.indexes.get(relation.index_at?)?;
            Some(Key {
                keys: Cow::Borrowed(&index.keys),
                unique: index.unique,
                primary: false,
                predicate: index.predicate.as_deref(),
                // **`indisvalid` is the schema state, read honestly.** An index that is not
                // `Public` is one no reader may use (ADR 0020), and `indisvalid` is exactly the
                // column a client checks before trusting one — `ActiveRecord`'s `indexes()`
                // selects it. Reporting `t` for a half-built index would be the wrong answer in
                // the one place a client asked the right question.
                valid: index.state == SchemaState::Public,
            })
        }
        RelKind::PrimaryKey => Some(Key {
            keys: table
                .primary_key
                .iter()
                .map(|at| IndexKey::column(*at))
                .collect(),
            unique: true,
            primary: true,
            valid: true,
            predicate: None,
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
];
