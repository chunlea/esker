//! `INSERT`, `UPDATE` and `DELETE`, lowered — and the `RETURNING` all three share.

use crate::plan::{Expr, SelectItem};

/// `RETURNING`, which is a target list over the rows a statement wrote.
///
/// The same [`SelectItem`] a `SELECT` projects, deliberately: `RETURNING *`, `RETURNING id`,
/// `RETURNING id AS new_id` and `RETURNING t.*` are the same four shapes with the same three
/// answers, and giving them a second type would be a second place for them to drift.
///
/// **Which row it sees is the whole of the semantics.** An `INSERT` returns the row as *stored*,
/// so a column filled from its `DEFAULT` comes back with that value; an `UPDATE` returns the row
/// as it is *after* the assignments; a `DELETE` returns the row as it was before it went. All
/// three were measured.
pub type Returning = Vec<SelectItem>;

/// `INSERT INTO t (cols) VALUES (...), (...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// The table, folded.
    pub table: String,
    /// The column list as written, or `None` for `INSERT INTO t VALUES ...`, which means every
    /// column in declaration order.
    pub columns: Option<Vec<String>>,
    /// One row per `VALUES` tuple. A row may be shorter than the column list — PostgreSQL fills
    /// the rest with NULL rather than refusing — but never longer.
    ///
    /// **`DEFAULT VALUES` is one row of no expressions**, which is the shortest such row and needs
    /// no variant of its own: the executor starts every row at each column's own default and runs
    /// the sequences after the values, so a row that names nothing is a row of defaults with its
    /// `bigserial` drawn. A row of NULLs, which is the tempting reading of the syntax, would be a
    /// different statement. An empty `rows` is a different thing again and never happens: it would
    /// be an `INSERT` that writes nothing at all.
    pub rows: Vec<Vec<Expr>>,
    /// `RETURNING`, over the rows as stored.
    pub returning: Option<Returning>,
    /// `ON CONFLICT …`, which is what `insert_all` and `upsert_all` compile to.
    pub on_conflict: Option<OnConflict>,
}

/// `ON CONFLICT [(cols)] DO NOTHING | DO UPDATE SET …`.
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflict {
    /// The arbiter key, or **empty** for a bare `ON CONFLICT`, which takes any unique index.
    ///
    /// A *key* and not an index name: PostgreSQL infers the index from it, so a list that matches
    /// no unique index is `42P10` rather than a name that does not resolve.
    pub target: Vec<ConflictKey>,
    /// `ON CONFLICT (…) WHERE <predicate>`: the index predicate, as the statement wrote it.
    ///
    /// **It selects a partial index and nothing else.** PostgreSQL infers an index whose predicate
    /// is *implied by* this one; this crate has no implication machinery (`catalog::IndexDef`'s
    /// own doc says so), so the match is on the text, which is the honest subset of that rule and
    /// is what `ActiveRecord` sends — `insert_all(unique_by: :index_name)` repeats the index's
    /// `where:` verbatim. `None` means the statement wrote no predicate, which infers only an
    /// index that has none.
    pub predicate: Option<String>,
    /// What to do with a row that conflicts.
    pub action: ConflictAction,
}

/// One entry of an `ON CONFLICT` target: a column, or an expression over the row.
///
/// **The same two shapes an index's key has** (`catalog::KeyPart`), because inference is the
/// question "is this key that index's key". `ON CONFLICT (lower(external_id))` names an index
/// created `ON books ((lower(external_id)))`, and `sqlparser` 0.62.0's target is a `Vec<Ident>`
/// with nowhere to put the call — so an expression entry reaches here through
/// `crate::parse::strip_on_conflict_target`, which replaces it with a placeholder identifier and
/// carries the text beside it.
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictKey {
    /// A column of the target table, folded the way every identifier is.
    Column(String),
    /// An expression, as the statement wrote it.
    Expression(String),
}

/// The two halves of `ON CONFLICT`.
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictAction {
    /// `DO NOTHING`: the row is not written, and `RETURNING` does not answer for it.
    DoNothing,
    /// `DO UPDATE SET c = …`, over the row **already there**.
    ///
    /// `excluded.c` in one of these expressions is the *proposed* row's column — the row that
    /// would have been inserted — which is why both rows are in scope while they are evaluated.
    DoUpdate(Vec<(String, Expr)>),
}

/// `UPDATE t [AS a] SET a = ..., b = ... [FROM x JOIN y ON ...] WHERE ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// The table, folded.
    pub table: String,
    /// `UPDATE t AS a` — the name the rest of the statement must use for the row being written.
    ///
    /// **An alias replaces the name, it does not add one.** Once it is written, a reference to the
    /// table's real name resolves against the `FROM` entry of that name if there is one and is an
    /// error if there is not — which is exactly what makes the `update_all` shape below work:
    /// `UPDATE "c" "__active_record_update_alias" … FROM "c" …` has two relations of one table,
    /// and `"c"."id"` is the one being *read*.
    pub alias: Option<String>,
    /// `FROM x` — the left-most relation the statement reads but does not write, or `None` for
    /// the ordinary single-table `UPDATE`.
    ///
    /// This is what a *joined* `update_all` sends: `ActiveRecord` cannot put a join on an `UPDATE`,
    /// so it aliases the target, puts the join in a `FROM`, and ties the two together in the
    /// `WHERE`. The same three fields a [`crate::plan::Select`] carries, because it is the same
    /// clause — and the rows this statement writes are the rows that `SELECT` would return.
    pub from: Option<crate::plan::TableRef>,
    /// The joins of the `FROM` clause, in the order written. Empty for a statement with none.
    pub joins: Vec<crate::plan::Join>,
    /// Column name and the expression to put in it, in the order written. PostgreSQL evaluates
    /// every one against the row as it was *before* the statement, so `SET a = b, b = a` swaps
    /// them rather than assigning `a` twice.
    ///
    /// **The name is never qualified.** `SET a.body = …` is not a spelling PostgreSQL accepts even
    /// when `a` is the target's own alias, which is why this stays a bare name beside a `FROM`.
    pub assignments: Vec<(String, Expr)>,
    /// `WHERE`. Absent means every row.
    pub filter: Option<Expr>,
    /// `RETURNING`, over the rows **after** the assignments.
    pub returning: Option<Returning>,
}

impl Update {
    /// The `FROM` clause as one join chain hanging off the target.
    ///
    /// **The `FROM` entry is joined to the target with no condition.** `UPDATE t a SET … FROM x
    /// WHERE …` means `t CROSS JOIN x` with the `WHERE` doing the tying, which is exactly what a
    /// comma in a `SELECT`'s `FROM` means — so nothing is approximated by building it as one, and
    /// a join written *inside* the `FROM` keeps its own `ON`.
    ///
    /// Empty for the ordinary single-table `UPDATE`, which is what makes one code path of both.
    #[must_use]
    pub fn chain(&self) -> Vec<crate::plan::Join> {
        self.from
            .iter()
            .map(|from| crate::plan::Join {
                table: from.clone(),
                kind: crate::plan::JoinKind::Inner,
                on: None,
                using: Vec::new(),
            })
            .chain(self.joins.iter().cloned())
            .collect()
    }
}

/// `DELETE FROM t WHERE ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    /// The table, folded.
    pub table: String,
    /// `WHERE`. Absent means every row.
    pub filter: Option<Expr>,
    /// `RETURNING`, over the rows as they were before they went.
    pub returning: Option<Returning>,
}
