//! `GROUP BY` and the five aggregates over it: the grouping key, the accumulators, and the
//! rewrite that turns a query written about rows into a plan about groups.
//!
//! # One row of a group is not one row of a table
//!
//! Everything above [`crate::plan::Node::Aggregate`] is evaluated against a row that does not
//! exist in any table: **the grouping keys, followed by the aggregate values**, in that order.
//! `SELECT g, count(*) FROM t GROUP BY g` projects positions 0 and 1 of that row, and its
//! `HAVING` and its `ORDER BY` are resolved against the same two positions.
//!
//! That rewrite is what [`Aggregation::rewrite`] does, and it is also where `42803` comes from:
//! an expression that survives the rewrite still holding a column reference is a column the query
//! neither grouped by nor aggregated, which is the one thing a grouped query may not ask for.
//! PostgreSQL says so by name and so does this — `column "t.n" must appear in the GROUP BY clause
//! or be used in an aggregate function`, with the qualifier, because in a join it is the only form
//! that says which table.
//!
//! # The semantics are the columnar evaluator's, and both come from a capture
//!
//! `esker-columnar`'s fragment evaluator defined these first (`docs/plans/phase-7-columnar.md`
//! M2), when there were no aggregates on this side to match. The row side now agrees with it,
//! rule for rule, and every rule was measured on a real PostgreSQL 19 rather than recalled —
//! `tests/corpus/pg19_aggregate.txt` is the file, and `tests/aggregate_parity.rs` replays it.
//!
//! | Rule | |
//! |---|---|
//! | `count(*)` counts every row, including one whose every column is NULL | it reads no value, so there is nothing to be NULL |
//! | `count(col)` skips NULLs | and is therefore a different aggregate wearing the same name |
//! | `sum`, `min`, `max`, `avg` over no rows or only NULLs are **NULL**, not zero | a zero here is a wrong answer that looks like data |
//! | `min`/`max` order by [`PgDatum::pg_cmp`] — `NaN` largest, `-0.0` equal to `0.0`, text by bytes | the order everything else in this system uses |
//! | a NULL grouping key is one group of its own | `pg_cmp` makes NULL equal only to NULL |
//! | groups come back in `pg_cmp` order of their keys | see below |
//! | `sum(int8)` overflowing is an error | ADR 0031: PostgreSQL's `sum(bigint)` is `numeric` and cannot overflow; a wrapped number would be silently wrong |
//! | `sum(float8)` and `avg(float8)` accumulate left to right in row order | floating-point addition is not associative, so the order is part of the answer |
//!
//! **The group order is a promise PostgreSQL does not make.** A real server returned the NULL
//! group first for one query here and last for the same query with an `ORDER BY`; the order is
//! whatever its hash table gave. Ours is `pg_cmp` order of the key, which is deterministic, is a
//! superset of what PostgreSQL guarantees, and — because `pg_cmp` puts NULL last — is the same
//! order `ORDER BY <key>` would have produced anyway.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::error::{Result, SqlError};
use crate::exec::query::Scope;
use crate::plan::{
    AggregateCall, AggregateFunc, AggregateSpec, Expr, Literal, Select, SelectItem, SortKey,
};
use crate::value::{ColumnType, Datum};
use crate::value::{PgDatum, PgType};

/// The most groups one aggregation will hold, before `53400` rather than an unbounded allocation
/// on a client's behalf. The same bound, for the same reason, as `Sort`'s.
pub(super) const GROUP_LIMIT: usize = 1_000_000;

/// A grouping key, ordered the way SQL orders values.
///
/// A newtype rather than a `Vec<Datum>` because [`Datum`]'s own `PartialEq` is **bitwise** — it
/// exists so a round-trip test cannot pass by turning `-0.0` into `0.0` — and grouping needs the
/// other comparison, the one a user sees: `-0.0` and `0.0` are one group and two `NaN`s are one
/// group, which is what a real server does. Deriving `Ord` here would have grouped by bytes and
/// been wrong in exactly the two places nobody tests.
#[derive(Debug, Clone)]
pub(super) struct GroupKey(pub(super) Vec<Datum>);

impl PartialEq for GroupKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for GroupKey {}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // `pg_cmp` already puts NULL last, which is why a group table walked in key order comes
        // out in the same order `ORDER BY <key>` would have given.
        self.0
            .iter()
            .zip(&other.0)
            .map(|(left, right)| left.pg_cmp(right))
            .find(|ordering| !ordering.is_eq())
            .unwrap_or_else(|| self.0.len().cmp(&other.0.len()))
    }
}

/// What the planner worked out about one query's grouping.
///
/// `None` from [`Aggregation::build`] means the query does not aggregate at all and nothing below
/// this module runs — which is every `SELECT` phase 6a already executed.
pub(super) struct Aggregation {
    /// The grouping keys, resolved against the **input** row.
    pub(super) keys: Vec<Expr>,
    /// Their types, so a rewritten reference to one carries a type.
    key_types: Vec<ColumnType>,
    /// One per distinct aggregate call in the statement, in output-row order after the keys.
    pub(super) specs: Vec<AggregateSpec>,
    /// The result types of those calls.
    spec_types: Vec<ColumnType>,
    /// `HAVING`, resolved against the **output** row.
    pub(super) having: Option<Expr>,
    /// Whether a `GROUP BY` was written — the empty-input rule, and nothing else.
    pub(super) grouped: bool,
}

impl Aggregation {
    /// Whether a statement aggregates at all.
    ///
    /// Three ways in, and `HAVING` is the one that surprises: `SELECT 1 FROM t HAVING true`
    /// aggregates, because `HAVING` filters a group and a query with no `GROUP BY` still has one.
    fn wanted(select: &Select) -> bool {
        !select.group_by.is_empty()
            || select.having.is_some()
            || select
                .projection
                .iter()
                .filter_map(|item| match item {
                    SelectItem::Expr { expr, .. } => Some(expr),
                    SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => None,
                })
                .chain(select.order_by.iter().map(|item| &item.expr))
                .any(contains_aggregate)
    }

    /// The type an aggregate call answers with, and the refusal when it has no answer.
    ///
    /// Both refusals are measured. `sum(text)` and `min(boolean)` do not exist on a real server
    /// either, so `42883` there is **parity**; `avg(bigint)` does exist and is `numeric`, which
    /// this node has no way to be right with, so it is `0A000` naming the type it would need
    /// (`docs/adr/0031-rails-compatibility-is-measured.md`).
    ///
    /// **Reachable from outside the aggregation** — `crate::exec::query::expr_type` and
    /// `crate::exec::bind` both ask it — because an aggregate's type is needed *before* the
    /// aggregation exists. A parameter takes its type from the other side of a comparison, and on
    /// a real server that includes `sum(salary) > $1` (`tests/having_bind.rs`); a second copy of
    /// this table living in the type inference is how `sum(int8)` would come to be `bigint` in one
    /// place and `numeric` in the other.
    pub(super) fn result_type(func: AggregateFunc, arg: Option<ColumnType>) -> Result<ColumnType> {
        let Some(arg) = arg else {
            // `count(*)`.
            return Ok(ColumnType::Int8);
        };
        let undefined = || {
            Err(SqlError::UndefinedAggregate {
                func: func.name(),
                argument: arg.name(),
            })
        };
        match func {
            AggregateFunc::Count => Ok(ColumnType::Int8),
            // **The argument's array type**, which is what a real server declares: `array_agg` of
            // an `int4` is an `integer[]`. This node has four array types (ADR 0047), so an
            // aggregate over any other element has no type to name and keeps `text` — the value is
            // the same array either way, and only the declared type differs.
            AggregateFunc::ArrayAgg => {
                Ok(esker_keys::array::ArrayValue::array_of(arg).unwrap_or(ColumnType::Text))
            }
            // **`sum` widens, and the width it goes to is not uniform.** Measured: `int2` and
            // `int4` sum to `bigint`, `int8` sums to **numeric** — which is why an `int8` sum
            // cannot overflow on a real server — and a `numeric` sums to `numeric`. A `float8`
            // stays itself.
            AggregateFunc::Sum => match arg {
                // **The one `sum` that stays its argument's type and can overflow.** An `int8`
                // widens to `numeric` on a real server precisely so that it cannot; a money has
                // nowhere wider to go, so `sum(money)` past the range is `22003 money out of
                // range` there and here.
                ColumnType::Money => Ok(ColumnType::Money),
                ColumnType::Int2 | ColumnType::Int4 => Ok(ColumnType::Int8),
                ColumnType::Int8 | ColumnType::Numeric => Ok(ColumnType::Numeric),
                ColumnType::Double => Ok(arg),
                // **A `time` sums to an `interval`, and so does an `interval`.** Measured, and it
                // is the pair that says the aggregate set is not per type but per (aggregate,
                // type): `min(time)` is a `time` where `sum(time)` is an `interval`, because
                // twenty-six and a half hours is not a time of day — `sum(time)` over
                // `01:00`, `02:00` and `23:30` is `26:30:00`.
                ColumnType::Interval | ColumnType::Time => Ok(ColumnType::Interval),
                _ => undefined(),
            },
            // Every type has an ordering here, and `bool` is the one PostgreSQL has no aggregate
            // for. Refusing it is being right rather than being incomplete.
            AggregateFunc::Min | AggregateFunc::Max => match arg {
                // A third and fourth totally-ordered type with no aggregate over it, after
                // `bool`: `min(jsonb)` is `42883 function min(jsonb) does not exist` on a real
                // server even though `<` works and `ORDER BY` works. Refusing is being right.
                // And a **fifth**: `min(uuid)` is `42883` on a real server too, where `<`,
                // `>`, `uuid_cmp` and `ORDER BY` all work. Checked against `pg_proc` in the
                // capture rather than inferred — no `min` or `max` takes 2950. This is the
                // finding ADR 0031 wrote down as a rule after `bool`: **the aggregate set is per
                // type and cannot be derived from whether the type is ordered.**
                // **`min`/`max` over a range does not exist on a real server either**, measured:
                // `42883 function min(tsrange) does not exist`. Answering it would be this node
                // answering where PostgreSQL raises, which ADR 0031 ranks as the worst class.
                ColumnType::TsRange
                | ColumnType::TstzRange
                | ColumnType::Int4Range
                | ColumnType::DateRange
                | ColumnType::NumRange
                | ColumnType::Int8Range
                | ColumnType::FloatRange
                | ColumnType::VarcharRange
                | ColumnType::TsRangeArray
                | ColumnType::TstzRangeArray
                | ColumnType::Int4RangeArray
                | ColumnType::DateRangeArray
                | ColumnType::NumRangeArray
                | ColumnType::Int8RangeArray
                | ColumnType::PointArray
                | ColumnType::Bool
                | ColumnType::Json
                | ColumnType::Jsonb
                | ColumnType::Uuid
                // **And a bit string**, measured: `min(bit)` is `42883 function min(bit) does
                // not exist` on a real server even though `<` works and `ORDER BY` works. The
                // seventh member of the list ADR 0031 turned into a rule — the aggregate set is
                // per type and cannot be derived from whether the type is ordered.
                | ColumnType::Bit
                | ColumnType::VarBit
                // **And an `xml`**, measured: `min(xml)` is `42883 function min(xml) does not
                // exist` — the eighth member of that list, and the one that could not have been
                // guessed from `json` being in it, since `json` has no ordering at all and this
                // refusal is about the aggregate rather than the order.
                | ColumnType::Xml
                // **And an `ltree`**, which is the sharpest of the list: the type is fully
                // ordered *and* indexable on a real server — `ORDER BY path` and `CREATE INDEX`
                // both work — and `min(ltree)` is still `42883 function min(ltree) does not
                // exist`. Nothing about the ordering implies the aggregate; ADR 0031's rule, one
                // type longer.
                | ColumnType::Ltree
                // **And all seven geometric shapes**, measured one at a time:
                // `min(point)`, `min(box)`, `min(lseg)`, `min(path)`, `max(polygon)`,
                // `min(circle)` and `max(line)` are each `42883 function min(<type>) does not
                // exist`. The ninth entry on ADR 0031's list, and the one that shows the rule best:
                // an `lseg`'s `=` **answers** — `'…'::lseg = '…'::lseg` is `t` — and its `min`
                // still does not exist, because the aggregate needs a btree family and equality
                // alone is not one (`tests/captures/pg19_geometric_array.txt`).
                | ColumnType::Point
                | ColumnType::Box
                | ColumnType::Lseg
                | ColumnType::Path
                | ColumnType::Polygon
                | ColumnType::Circle
                | ColumnType::Line
                // **And a `macaddr`**, which is the one in this family reasoning gets wrong:
                // `ORDER BY` over one works on both servers and `min(macaddr)` is still
                // `42883 function min(macaddr) does not exist`. Not derivable from its two
                // neighbours either — a `cidr` decays and an `inet` keeps itself, three rules for
                // three types (`tests/captures/pg19_cidr_aggregate.txt`).
                | ColumnType::MacAddr => undefined(),
                // Measured: `min(varchar)` and `max(varchar)` come back as **`text`** on a real
                // server, and `min(character(n))` comes back as **`bpchar`**. The string family
                // does not decay uniformly — `bpchar` has a `min` of its own where `varchar`
                // borrows `text`'s — and that asymmetry is captured rather than smoothed over
                // (`tests/corpus/pg19_typmod.txt`). The value does not change either way; the
                // declared type does.
                // **And a `name`, which borrows `text`'s the way `varchar` does** — measured,
                // `min('x'::name)` is a `text` on a real server. It is the one rule in the `name`
                // unit that runs *away* from the type: every other derivation keeps `name` and
                // this one drops it, because a real server has no `min(name)` and coerces the
                // argument (`tests/captures/pg19_name_array.txt`).
                ColumnType::Varchar | ColumnType::Name => Ok(ColumnType::Text),
                // **A `cidr` decays to `inet`, which is the same rule one category along.**
                // `inet` is the preferred type of the network category the way `text` is of the
                // string one, and a real server has no `min(cidr)` to keep the type with —
                // measured, `pg_typeof(min(c::cidr))` is `inet` there. The **value** is unchanged,
                // mask included: a `cidr` through `inet`'s output function is the same characters,
                // which is what makes this a declared type rather than an answer.
                ColumnType::Cidr => Ok(ColumnType::Inet),
                _ => Ok(arg),
            },
            // **Every integer width averages to `numeric`**, and so does a `numeric`. The
            // refusal that used to stand here said this node had no numeric type to reproduce
            // the answer with; it has had one since ADR 0045, and the scale is
            // `numeric::div_scale`'s — sixteen *significant* digits, not sixteen fractional
            // ones, which is why `3/3` prints twenty places and `5/3` sixteen.
            AggregateFunc::Avg => match arg {
                ColumnType::Double => Ok(ColumnType::Double),
                ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 | ColumnType::Numeric => {
                    Ok(ColumnType::Numeric)
                }
                // **An interval averages to an interval**, and it is the one average that is not
                // a division in a number type at all: the sum is kept exactly and divided once,
                // through `interval / n`, whose cascade rounds twice on the way down. `avg(time)`
                // comes here too and is an `interval` for the same reason `sum(time)` is.
                ColumnType::Interval | ColumnType::Time => Ok(ColumnType::Interval),
                _ => undefined(),
            },
        }
    }

    /// Plans the aggregation of one statement, or `None` if it does not aggregate.
    ///
    /// Order matters here and is PostgreSQL's: `WHERE` is checked for aggregates **first**,
    /// because `WHERE count(*) > 1` is a different error from anything the grouping keys could
    /// produce and a user who wrote it wants to be told which clause was wrong.
    pub(super) fn build(select: &Select, scope: &Scope<'_>) -> Result<Option<Self>> {
        if let Some(filter) = &select.filter
            && contains_aggregate(filter)
        {
            return Err(SqlError::AggregateNotAllowed(
                "aggregate functions are not allowed in WHERE",
            ));
        }
        if !Self::wanted(select) {
            return Ok(None);
        }

        // A grouping key may be an output alias or a **position** in the target list. Both are
        // PostgreSQL's, and a position out of range is `42P10` naming the position.
        let mut keys = Vec::new();
        let mut key_types = Vec::new();
        for expr in &select.group_by {
            let written = match expr {
                Expr::Literal(Literal::Integer(position)) => {
                    ordinal(*position, select, "GROUP BY")?
                }
                other => super::query::dealias(other, select),
            };
            if contains_aggregate(&written) {
                return Err(SqlError::AggregateNotAllowed(
                    "aggregate functions are not allowed in GROUP BY",
                ));
            }
            let resolved = super::query::resolve(&written, scope)?;
            // `GROUP BY g, g` is one key on a real server, and a duplicate here would put a second
            // copy of the same value in every output row.
            if keys.contains(&resolved) {
                continue;
            }
            let ty = super::query::expr_type(&resolved, scope)?;
            // **A grouping key needs an equality operator class**, and it is the same list
            // `count(DISTINCT x)` reads three functions down: `GROUP BY j` over a `json` column is
            // `42883 could not identify an equality operator for type json` on a real server,
            // measured, and grouping it here from `pg_cmp`'s text comparison answered where a real
            // server raises. A `jsonb` is deliberately not on the list — it has a btree opclass
            // and groups fine there.
            if !crate::value::has_equality_operator(ty) {
                return Err(SqlError::NoEqualityOperator(ty.name()));
            }
            key_types.push(ty);
            keys.push(resolved);
        }

        // **PostgreSQL's functional dependency**, applied by widening the grouping rather than by
        // relaxing the check. A `GROUP BY` that contains a table's primary key leaves one row per
        // group *of that table*, so every other column of it has exactly one value and needs no
        // aggregate — and grouping by `(f.id, f.name)` is the same grouping as `(f.id)` when
        // `f.id` is a key: same groups, same rows, same counts. So the dependent columns simply
        // join the keys, and every check below sees a query that groups by them.
        //
        // It is **per table**: `GROUP BY f.id` frees `f`'s columns and none of `a`'s, which is
        // what [`crate::exec::query::Scope::key_of`] answers. `ActiveRecord` writes this shape
        // constantly — `group(:id)` on a relation selecting `*`.
        widen_for_dependencies(&mut keys, &mut key_types, select, scope)?;

        // Every aggregate call the statement makes, once each: two `count(*)`s in one target list
        // are one accumulator and one output position.
        let ungrouped = keys.is_empty();
        let mut specs: Vec<AggregateSpec> = Vec::new();
        let mut spec_types = Vec::new();
        let mut collect = |expr: &Expr| -> Result<()> {
            check_not_nested(expr)?;
            for call in aggregate_calls(expr) {
                // Every one of the five takes exactly one argument, and PostgreSQL's refusal for
                // any other arity names the **types** of what was written -- which is why the
                // check is here, where they are known, rather than in the lowering.
                if !call.star && call.args.len() != 1 {
                    let mut arguments = Vec::with_capacity(call.args.len());
                    for arg in &call.args {
                        let resolved = super::query::resolve(arg, scope)?;
                        arguments.push(super::query::expr_type(&resolved, scope)?.name());
                    }
                    return Err(SqlError::UndefinedAggregateArity {
                        func: call.func.name(),
                        arguments: arguments.join(", "),
                    });
                }
                let spec = resolve_aggregate(call, scope)?;
                if specs.contains(&spec) {
                    continue;
                }
                spec_types.push(Self::result_type(spec.func, spec.arg_type)?);
                specs.push(spec);
            }
            Ok(())
        };
        for item in &select.projection {
            if let SelectItem::Expr { expr, .. } = item {
                collect(expr)?;
            }
        }
        if let Some(having) = &select.having {
            collect(having)?;
        }
        for item in &select.order_by {
            collect(&super::query::dealias(&item.expr, select))?;
        }

        let mut aggregation = Aggregation {
            keys,
            key_types,
            specs,
            spec_types,
            having: None,
            // `GROUP BY ()` writes the clause and contributes no key, and it is the grand total:
            // one row even over an empty table (measured). So the empty-input rule keys off the
            // keys that survived rather than off whether the clause was written.
            grouped: !ungrouped,
        };
        // `HAVING` is resolved and rewritten last, and **without** `dealias`: `GROUP BY gg` may
        // name an output alias and `HAVING c > 0` may not — measured, a real server answers
        // `42703 column "c" does not exist` for the second. The two clauses do not share a scope.
        aggregation.having = select
            .having
            .as_ref()
            .map(|having| {
                let resolved = super::query::resolve(having, scope)?;
                // The type check comes **before** the rewrite, because PostgreSQL's does:
                // `HAVING g` over an ungrouped query is `42804 argument of HAVING must be type
                // boolean, not type text` there, not the `42803` the grouping rule would give.
                // Measured, and the two are a statement about different mistakes.
                aggregation.check_boolean(&resolved, scope)?;
                aggregation.rewrite(&resolved, scope)
            })
            .transpose()?;
        Ok(Some(aggregation))
    }

    /// `HAVING` must be a boolean, and PostgreSQL names the type it got instead.
    ///
    /// Its own check rather than `crate::exec::query`'s, for two reasons: it runs on the resolved
    /// expression *before* the rewrite, so that a non-boolean beats the grouping rule to the
    /// answer the way it does on a real server; and an aggregate call has no type until this
    /// module has resolved its argument, so nothing outside here can name one.
    fn check_boolean(&self, expr: &Expr, scope: &Scope<'_>) -> Result<()> {
        let ty = match expr {
            Expr::Binary { .. }
            | Expr::Not(_)
            | Expr::IsNull { .. }
            | Expr::InList { .. }
            | Expr::Literal(Literal::Bool(_) | Literal::Null) => return Ok(()),
            Expr::Ordinal { ty, .. } => *ty,
            Expr::Aggregate(call) => self
                .specs
                .iter()
                .position(|spec| spec.func == call.func && spec.distinct == call.distinct)
                .and_then(|at| self.spec_types.get(at).copied())
                .unwrap_or(ColumnType::Int8),
            other => super::query::expr_type(other, scope).unwrap_or(ColumnType::Text),
        };
        if ty == ColumnType::Bool {
            return Ok(());
        }
        Err(SqlError::DatatypeMismatch(format!(
            "argument of HAVING must be type boolean, not type {}",
            ty.name()
        )))
    }

    /// The type of output column `at`, for `RowDescription`.
    fn output_type(&self, at: usize) -> ColumnType {
        self.key_types
            .get(at)
            .or_else(|| self.spec_types.get(at - self.key_types.len()))
            .copied()
            .unwrap_or(ColumnType::Text)
    }

    /// Rewrites an expression resolved against the input row into one over the **output** row.
    ///
    /// A grouping key becomes its position; an aggregate call becomes its position after the keys;
    /// anything else recurses. What cannot be rewritten is a column reference that is neither, and
    /// that is `42803` — the whole rule of a grouped query, enforced in one place.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per expression that holds another; the list being complete is the point"
    )]
    pub(super) fn rewrite(&self, expr: &Expr, scope: &Scope<'_>) -> Result<Expr> {
        if let Some(at) = self.keys.iter().position(|key| key == expr) {
            return Ok(Expr::Ordinal {
                at,
                ty: self.output_type(at),
                // An aggregate's output carries no typmod: `min(c)` over a `character(3)` is
                // `bpchar` with none on a real server, and a grouping key has already been
                // through whatever coercion its own comparison needed.
                typmod: crate::value::NO_TYPMOD,
            });
        }
        Ok(match expr {
            Expr::Aggregate(call) => {
                let at = self
                    .specs
                    .iter()
                    .position(|spec| {
                        spec.func == call.func
                            && spec.distinct == call.distinct
                            // **The clause is part of the identity.** `array_agg(n ORDER BY n)`
                            // and `array_agg(n ORDER BY n DESC)` in one statement are two
                            // aggregates, not one written twice; matching without it collapsed
                            // them onto the first one's accumulator and answered the same array
                            // for both.
                            && resolve_aggregate_order_by(call, scope)
                                .is_ok_and(|ours| spec.order_by == ours)
                            && match (&spec.arg, call.arg()) {
                                (None, None) => true,
                                (Some(theirs), Some(ours)) => super::query::resolve(ours, scope)
                                    .is_ok_and(|ours| theirs == &ours),
                                _ => false,
                            }
                    })
                    .ok_or_else(|| {
                        SqlError::Internal(
                            "an aggregate reached the rewrite without being collected".to_owned(),
                        )
                    })?;
                let at = self.keys.len() + at;
                Expr::Ordinal {
                    at,
                    ty: self.output_type(at),
                    typmod: crate::value::NO_TYPMOD,
                }
            }
            // The one failure this function exists for. Resolution has already turned the name
            // into a position, so the qualified name is put back for the message.
            //
            // A column **functionally determined** by a grouping key never reaches here: it was
            // added to the keys before any rewriting ran (`widen_for_dependencies`), so the
            // is-it-a-key arm at the top of this function answers first.
            Expr::Ordinal { at, .. } => {
                return Err(SqlError::GroupingError(scope.qualified_name(*at)));
            }
            Expr::Binary { op, left, right } => Expr::Binary {
                op: *op,
                left: Box::new(self.rewrite(left, scope)?),
                right: Box::new(self.rewrite(right, scope)?),
            },
            Expr::Not(operand) => Expr::Not(Box::new(self.rewrite(operand, scope)?)),
            Expr::InList {
                operand,
                list,
                negated,
            } => Expr::InList {
                operand: Box::new(self.rewrite(operand, scope)?),
                list: list
                    .iter()
                    .map(|item| self.rewrite(item, scope))
                    .collect::<Result<Vec<_>>>()?,
                negated: *negated,
            },
            Expr::IsNull { operand, negated } => Expr::IsNull {
                operand: Box::new(self.rewrite(operand, scope)?),
                negated: *negated,
            },
            // **Every expression that holds another has to descend**, and the reason is not
            // symmetry: an aggregate left un-rewritten inside one reaches the row evaluator, which
            // has no group to read it from and answers `XX000`. `pg_typeof(array_agg(i))`,
            // `array_length(array_agg(i), 1)` and `abs(min(n))` were all that internal error —
            // valid SQL, answered with a bug report. The arms below are every variant with a
            // child; the `other` at the end is the leaves.
            Expr::Negate(operand) => Expr::Negate(Box::new(self.rewrite(operand, scope)?)),
            Expr::Arithmetic {
                op,
                left,
                right,
                ty,
            } => Expr::Arithmetic {
                op: *op,
                left: Box::new(self.rewrite(left, scope)?),
                right: Box::new(self.rewrite(right, scope)?),
                ty: *ty,
            },
            Expr::Scalar { func, operand } => Expr::Scalar {
                func: *func,
                operand: Box::new(self.rewrite(operand, scope)?),
            },
            Expr::ToText {
                operand,
                strip_blanks,
                enum_labels,
            } => Expr::ToText {
                operand: Box::new(self.rewrite(operand, scope)?),
                strip_blanks: *strip_blanks,
                enum_labels: enum_labels.clone(),
            },
            Expr::CatalogFunc(call) => {
                let mut rewritten = call.clone();
                for arg in &mut rewritten.args {
                    *arg = self.rewrite(arg, scope)?;
                }
                Expr::CatalogFunc(rewritten)
            }
            Expr::AnyArray { operand, array } => Expr::AnyArray {
                operand: Box::new(self.rewrite(operand, scope)?),
                array: Box::new(self.rewrite(array, scope)?),
            },
            Expr::Subscript {
                operand,
                index,
                element,
            } => Expr::Subscript {
                operand: Box::new(self.rewrite(operand, scope)?),
                index: Box::new(self.rewrite(index, scope)?),
                element: *element,
            },
            Expr::Like {
                operand,
                pattern,
                negated,
                case_insensitive,
                escape,
            } => Expr::Like {
                operand: Box::new(self.rewrite(operand, scope)?),
                pattern: Box::new(self.rewrite(pattern, scope)?),
                negated: *negated,
                case_insensitive: *case_insensitive,
                escape: *escape,
            },
            Expr::Coalesce(args) => Expr::Coalesce(
                args.iter()
                    .map(|arg| self.rewrite(arg, scope))
                    .collect::<Result<Vec<_>>>()?,
            ),
            Expr::Case {
                branches,
                otherwise,
            } => Expr::Case {
                branches: branches
                    .iter()
                    .map(|branch| {
                        Ok(crate::plan::CaseBranch {
                            when: self.rewrite(&branch.when, scope)?,
                            then: self.rewrite(&branch.then, scope)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                otherwise: otherwise
                    .as_deref()
                    .map(|expr| self.rewrite(expr, scope))
                    .transpose()?
                    .map(Box::new),
            },
            // A subquery is a constant of the outer row, so the sub-plan is passed through — but
            // its **operand** is an expression of that row like any other, and
            // `HAVING max(k) IN (SELECT …)` needs the `max(k)` rewritten into the aggregated row.
            Expr::Subquery(sub) if sub.operands.is_empty() => expr.clone(),
            Expr::Subquery(sub) => {
                let mut rewritten = sub.clone();
                rewritten.operands = sub
                    .operands
                    .iter()
                    .map(|operand| self.rewrite(operand, scope))
                    .collect::<Result<Vec<_>>>()?;
                Expr::Subquery(rewritten)
            }
            other => other.clone(),
        })
    }
}

/// `ORDER BY 2`, `GROUP BY 1`: a position in the target list, one-based.
///
/// PostgreSQL's message names the clause and the position, and `GROUP BY 0` gets the same one as
/// `GROUP BY 5` — measured, both are `42P10 GROUP BY position N is not in select list`.
pub(super) fn ordinal(position: i64, select: &Select, clause: &str) -> Result<Expr> {
    let item = usize::try_from(position)
        .ok()
        .filter(|position| *position >= 1)
        .and_then(|position| select.projection.get(position - 1));
    match item {
        Some(SelectItem::Expr { expr, .. }) => Ok(expr.clone()),
        // A `*` is not one column, so a position cannot name it. PostgreSQL expands the wildcard
        // before numbering and this crate does not, so the honest answer is the same `42P10`
        // rather than a position that means something different here.
        _ => Err(SqlError::InvalidColumnReference(format!(
            "{clause} position {position} is not in select list"
        ))),
    }
}
/// Adds every column a grouping key **functionally determines** to the keys.
///
/// PostgreSQL accepts a bare column when the `GROUP BY` contains its table's primary key, because
/// grouping by a key leaves one row per group of that table. Honouring it by widening the grouping
/// rather than by relaxing the check is what makes it obviously correct: grouping by `(f.id,
/// f.name)` is the same grouping as `(f.id)` when `f.id` is a key — the same groups, the same
/// rows, and the same `count(*)` in each.
///
/// **Per table.** A column is added only when *every* primary-key column of **its own** table is
/// already a key, so `GROUP BY f.id` frees `f`'s columns and leaves `a`'s as the `42803` they are.
/// A table with no declared primary key determines nothing.
///
/// The select list and the `HAVING` are both walked, because a real server allows the dependency
/// in both — measured, `GROUP BY f.id HAVING f.name = 'one'` runs.
fn widen_for_dependencies(
    keys: &mut Vec<Expr>,
    key_types: &mut Vec<ColumnType>,
    select: &Select,
    scope: &Scope<'_>,
) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let grouped: Vec<usize> = keys
        .iter()
        .filter_map(|key| match key {
            Expr::Ordinal { at, .. } => Some(*at),
            _ => None,
        })
        .collect();
    let mut determined: Vec<Expr> = Vec::new();
    let mut consider = |expr: &Expr| -> Result<()> {
        let resolved = super::query::resolve(&super::query::dealias(expr, select), scope)?;
        let mut bare = Vec::new();
        bare_ordinals(&resolved, &mut bare);
        for column in bare {
            let Expr::Ordinal { at, .. } = &column else {
                continue;
            };
            let Some(key) = scope.key_of(*at) else {
                continue;
            };
            if key.iter().all(|column| grouped.contains(column))
                && !grouped.contains(at)
                && !determined.contains(&column)
            {
                determined.push(column);
            }
        }
        Ok(())
    };
    for item in &select.projection {
        match item {
            SelectItem::Expr { expr, .. } => consider(expr)?,
            // **A star is every column it expands to, and skipping it was the bug.** The check
            // below sees the expansion — it is what `SELECT *` returns — so a widening that looked
            // only at written expressions freed nothing for the one shape `ActiveRecord` writes
            // most: `Model.group(:id)` on a relation selecting everything. Measured on PG 19:
            // `SELECT * FROM fam GROUP BY fam.id` answers rows.
            //
            // The expansion is [`crate::exec::query::Scope::expand`], the same call the target
            // list itself uses, rather than a second walk of the scope — a `USING` merge and the
            // written order are both in it, and a copy would have to know that too.
            SelectItem::Wildcard => consider_expansion(scope, None, &mut consider)?,
            SelectItem::QualifiedWildcard(qualifier) => {
                consider_expansion(scope, Some(qualifier), &mut consider)?;
            }
        }
    }
    if let Some(having) = &select.having {
        consider(having)?;
    }
    // **And the `ORDER BY`**, which is the clause `ActiveRecord` actually reaches this through and
    // the one this function was missing. `Company.includes(:comments).order(:rating).ids` sends
    //
    //   SELECT "companies"."id" FROM "companies"
    //     LEFT OUTER JOIN "comments" ON "comments"."company" = "companies"."id"
    //     GROUP BY "companies"."id" ORDER BY "companies"."rating" ASC
    //
    // and the ordered column is nowhere else in the statement: not in the select list, not in the
    // `HAVING`. So the dependency was measured, implemented and then not applied to the one shape
    // the suite sends — `42803 column "companies.rating" must appear in the GROUP BY clause` for a
    // query a real server answers with fifteen rows
    // (`calculations_test#test_ids_with_includes_and_non_primary_key_order`).
    //
    // Measured with the rest of the family in `tests/corpus/pg19_group_by_key.txt`: `DESC`, an
    // expression over the dependent column, a second key beside it, and both tables' keys grouped
    // are all accepted, and the refusals stay refusals — the *other* table's column, a table whose
    // key is not grouped, a composite key only half grouped, and a `UNIQUE NOT NULL` column, which
    // determines nothing on a real server because only the primary key does.
    for item in &select.order_by {
        consider(&item.expr)?;
    }
    for key in determined {
        key_types.push(super::query::expr_type(&key, scope)?);
        keys.push(key);
    }
    Ok(())
}

/// Offers each column a star expands to for the same dependency test a written column gets.
///
/// A qualifier naming no relation in the query is `42P01`, and it is [`Scope::expand`]'s answer
/// rather than one invented here — the target list would refuse the same statement with the same
/// sentence a line later.
fn consider_expansion(
    scope: &Scope<'_>,
    qualifier: Option<&str>,
    consider: &mut impl FnMut(&Expr) -> Result<()>,
) -> Result<()> {
    for (at, column) in scope.expand(qualifier)? {
        consider(&Expr::Ordinal {
            at,
            ty: column.ty,
            typmod: column.typmod,
        })?;
    }
    Ok(())
}

/// Every column reference in a **resolved** expression that is not inside an aggregate.
///
/// A column inside `count(…)` is answered by the aggregate and needs no grouping; one outside is
/// what the `42803` is about, and therefore what a functional dependency has to cover. Written
/// here rather than reusing a general walker because that distinction — descend into everything
/// *except* an aggregate's arguments — is the whole of what it is for.
fn bare_ordinals(expr: &Expr, found: &mut Vec<Expr>) {
    walk(expr, &mut |node| {
        if matches!(node, Expr::Ordinal { .. }) {
            found.push(node.clone());
        }
    });
}

/// Whether an expression contains an aggregate call anywhere inside it.
///
/// `pub(crate)` because the lowering asks it too: `FOR UPDATE` is `0A000` over a target list with
/// an aggregate in it, and a second walker written there would be a second opinion about what an
/// aggregate is.
pub(crate) fn contains_aggregate(expr: &Expr) -> bool {
    !aggregate_calls(expr).is_empty()
}

/// Every aggregate call in an expression, outermost first.
///
/// A call **inside** another call is `42803` rather than a second entry: PostgreSQL says
/// `aggregate function calls cannot be nested`, and a `sum(count(*))` that quietly computed
/// something would be worse than a refusal.
fn aggregate_calls(expr: &Expr) -> Vec<&AggregateCall> {
    let mut found = Vec::new();
    walk(expr, &mut |expr| {
        if let Expr::Aggregate(call) = expr {
            found.push(&**call);
        }
    });
    found
}

/// Every node of an expression, **stopping at an aggregate**: the call is visited and its
/// arguments are not.
///
/// One traversal with two callers, because they want the same shape for opposite reasons.
/// [`aggregate_calls`] wants the calls; [`bare_ordinals`] wants the columns that are *not* under
/// one, since a column inside `count(…)` is answered by the aggregate and needs no grouping. Two
/// copies of this list would be two opinions about which expressions hold another, and that list
/// having a blind spot is what made `pg_typeof(array_agg(i))` an internal error once.
fn walk<'a>(expr: &'a Expr, visit: &mut impl FnMut(&'a Expr)) {
    match expr {
        Expr::Aggregate(_) => visit(expr),
        // **Every expression that holds another**, and this list is the reason
        // `pg_typeof(array_agg(i))` used to be an internal error: an aggregate the collector does
        // not see is an aggregate the statement is not planned around, so it survives into the row
        // evaluator, which has no group to read it from. The blind spot was one `_ => {}`, and it
        // was the same one in `Aggregation::rewrite`.
        Expr::Not(operand)
        | Expr::IsNull { operand, .. }
        | Expr::Negate(operand)
        | Expr::Scalar { operand, .. }
        | Expr::ToText { operand, .. } => walk(operand, visit),
        Expr::Binary { left, right, .. }
        | Expr::Arithmetic { left, right, .. }
        | Expr::AnyArray {
            operand: left,
            array: right,
        }
        | Expr::Subscript {
            operand: left,
            index: right,
            ..
        }
        | Expr::Like {
            operand: left,
            pattern: right,
            ..
        } => {
            walk(left, visit);
            walk(right, visit);
        }
        Expr::CatalogFunc(call) => {
            for arg in &call.args {
                walk(arg, visit);
            }
        }
        Expr::Coalesce(args) => {
            for arg in args {
                walk(arg, visit);
            }
        }
        Expr::Case {
            branches,
            otherwise,
        } => {
            for branch in branches {
                walk(&branch.when, visit);
                walk(&branch.then, visit);
            }
            if let Some(otherwise) = otherwise {
                walk(otherwise, visit);
            }
        }
        Expr::InList { operand, list, .. } => {
            walk(operand, visit);
            for item in list {
                walk(item, visit);
            }
        }
        // The operand only, and **not** into the sub-select: `WHERE count(*) IN (SELECT …)` is
        // `42803` because the aggregate is in the `WHERE`, while `WHERE id IN (SELECT count(*) …)`
        // is an ordinary statement whose aggregate belongs to a different query.
        Expr::Subquery(sub) => {
            for operand in &sub.operands {
                walk(operand, visit);
            }
        }
        other => visit(other),
    }
}

/// `sum(count(*))` and friends.
pub(super) fn check_not_nested(expr: &Expr) -> Result<()> {
    for call in aggregate_calls(expr) {
        if call.args.iter().any(contains_aggregate) {
            return Err(SqlError::AggregateNotAllowed(
                "aggregate function calls cannot be nested",
            ));
        }
        // **A set-returning call inside an aggregate is its own sentence**, and PostgreSQL adds a
        // HINT about `LATERAL`. `count(generate_series(1,3))` asks an aggregate to fold a set that
        // the projection would have expanded into rows *around* it — the two cannot both happen,
        // and a real server says which one loses.
        if call.args.iter().any(|arg| {
            let mut found = false;
            super::bind::descend(arg, &mut |expr| {
                found |= matches!(expr, Expr::SetFunc(_));
            });
            found
        }) {
            return Err(SqlError::SetFunctionNotAllowed(
                "aggregate function calls cannot contain set-returning function calls".to_owned(),
            ));
        }
    }
    Ok(())
}

/// One aggregate, mid-fold.
#[derive(Debug)]
pub(super) struct Accumulator {
    func: AggregateFunc,
    /// The aggregate's own `ORDER BY`, kept because the sort happens at [`Accumulator::finish`] —
    /// the order of a group is not known until the group is complete.
    order_by: Vec<SortKey>,
    /// The values already folded in, for a `DISTINCT` call. One `BTreeSet` per aggregate rather
    /// than per group is not possible — distinctness is per group — so this lives here.
    seen: Option<BTreeSet<GroupKey>>,
    state: State,
}

#[derive(Debug)]
enum State {
    /// `count(*)` and `count(col)`, which differ only in whether a NULL reaches here.
    Count(i64),
    /// `sum(float8)`, and the running half of `avg(float8)`.
    SumFloat(Option<f64>),
    /// `min`/`max`, holding the best value seen.
    Extreme(Option<Datum>),
    /// `avg(float8)`: the sum, and how many values went into it.
    AvgFloat { sum: f64, seen: i64 },
    /// `sum(int2)` and `sum(int4)`, which widen to an `int8` and cannot overflow it.
    SumWide(Option<i64>),
    /// `sum(money)`, which stays a money and therefore **can** overflow — cents in an `i64` with
    /// nothing wider to widen to.
    SumMoney(Option<i64>),
    /// `sum(interval)` and `sum(time)`, and the running half of the two averages over them.
    ///
    /// The three fields are added **independently and exactly** — nothing normalises between them
    /// and nothing is divided until [`Accumulator::finish`] — because `avg` is its sum divided
    /// once, not a running mean, and each division rounds.
    Interval {
        total: Option<crate::value::interval::Interval>,
        seen: i64,
    },
    /// `sum(int8)` and `sum(numeric)`, both of which answer a `numeric`.
    SumNumeric(Option<esker_keys::numeric::Numeric>),
    /// `avg` over any exact type: the running sum as a decimal, and the count to divide it by.
    ///
    /// The count is kept rather than the average, because an average cannot be folded — the mean
    /// of two means is not the mean.
    AvgNumeric {
        sum: Option<esker_keys::numeric::Numeric>,
        seen: i64,
    },
    /// `array_agg`: every value, with the sort key it was collected under.
    ///
    /// The only state here that is **not** constant in the group's size, which is the price of an
    /// aggregate that keeps its inputs rather than folding them. Bounded by the same
    /// [`GROUP_LIMIT`] the group table and the `DISTINCT` set are, and for the same reason.
    Gather {
        /// The argument's type, which is the array's element type. `None` for an argument whose
        /// type this node could not name — the value is then gathered as text, the way every
        /// `array_agg` was before ADR 0047 gave arrays a type of their own.
        element: Option<ColumnType>,
        /// Each value with the sort key its own `ORDER BY` gave it.
        values: Vec<(Vec<Datum>, Datum)>,
    },
}

/// One aggregate call, resolved into the spec the accumulator is built from.
fn resolve_aggregate(call: &AggregateCall, scope: &Scope<'_>) -> Result<AggregateSpec> {
    let arg = call
        .arg()
        .map(|arg| super::query::resolve(arg, scope))
        .transpose()?;
    // **An argument with no type is `42725` for three of the six**, and it is not a blanket rule:
    // `min`, `max` and `count` resolve an `unknown` to `text` and answer, while `sum`, `avg` and
    // `array_agg` have one candidate per input type and cannot choose. Each of the six was put to
    // a real server; a node that defaulted all of them to `text` would answer where three of them
    // raise, which is ADR 0031's worst class.
    if matches!(
        call.func,
        AggregateFunc::Sum | AggregateFunc::Avg | AggregateFunc::ArrayAgg
    ) && arg.as_ref().is_some_and(is_unknown)
    {
        return Err(SqlError::AmbiguousFunction {
            func: call.func.name(),
        });
    }
    let arg_type = arg
        .as_ref()
        .map(|arg| super::query::expr_type(arg, scope))
        .transpose()?;
    // **`DISTINCT` needs an equality *operator class*, which is not the same as an `=` that
    // answers.** `count(DISTINCT a_line_segment)` is
    // `42883 could not identify an equality operator for type lseg` on a real server while
    // `'…'::lseg = '…'::lseg` is `t` — and without this the count was answered from `pg_cmp`'s
    // text comparison, a number where PostgreSQL raises, which ADR 0031 ranks worst.
    // **The array gets its *own* name in the message**: `count(DISTINCT x)` over an `xml[]` is
    // `could not identify an equality operator for type xml[]`, not for `xml`. Measured, and the
    // same for `json[]` and `point[]`, which is why `ty.name()` and not the element's.
    if call.distinct
        && let Some(ty) = arg_type
        && !crate::value::has_equality_operator(ty)
    {
        return Err(SqlError::NoEqualityOperator(ty.name()));
    }
    Ok(AggregateSpec {
        func: call.func,
        arg,
        distinct: call.distinct,
        arg_type,
        order_by: resolve_aggregate_order_by(call, scope)?,
    })
}

/// Whether an argument is PostgreSQL's `unknown`: a **quoted string**, which carries no type until
/// something else gives it one.
///
/// **A bare NULL is not included, and the reason it could not be has gone.** It is an `unknown` on
/// a real server too — `array_agg(NULL)` is the same `42725` there — and the under-reach was that
/// this crate dropped the cast on a NULL at lowering, so `NULL` and `NULL::int4` arrived here as
/// the same `Literal::Null` and a rule that refused one would have refused the other.
/// `Literal::TypedNull` ended that: `array_agg(NULL::int4)` is `{NULL}` here as it is there, and
/// widening this function to `Literal::Null` would no longer take it with it. What the widening
/// still owes is a capture — only `array_agg`'s half of the bare-NULL family has been put to the
/// oracle, and the six aggregates did not agree with each other on the quoted-string form either.
/// Until then the bare NULL stays a declared divergence in `tests/aggregate_type.rs`.
fn is_unknown(arg: &Expr) -> bool {
    matches!(arg, Expr::Literal(Literal::String(_)))
}

/// One aggregate's own `ORDER BY`, resolved against the **input** row.
///
/// Its keys are expressions of what is being aggregated, not of the grouped output — which is why
/// they resolve in the same scope as the argument beside them and not in the one the projection
/// sees. The default null placement follows the direction, exactly as the query's own `ORDER BY`
/// resolves it.
fn resolve_aggregate_order_by(call: &AggregateCall, scope: &Scope<'_>) -> Result<Vec<SortKey>> {
    call.order_by
        .iter()
        .map(|item| {
            Ok(SortKey {
                expr: super::query::resolve(&item.expr, scope)?,
                descending: item.descending,
                nulls_first: item
                    .nulls_first
                    .unwrap_or(crate::catalog::KeyOrder::of(item.descending).nulls_first),
            })
        })
        .collect()
}

/// A running total plus one more value, starting from nothing.
///
/// The `None` start is what makes a sum over no rows NULL rather than zero.
fn add_numeric(
    total: Option<&esker_keys::numeric::Numeric>,
    addend: &esker_keys::numeric::Numeric,
) -> esker_keys::numeric::Numeric {
    match total {
        Some(total) => crate::value::numeric::sum(total, addend),
        None => addend.clone(),
    }
}

/// One interval added into a running total, or `22008`.
///
/// Nothing normalises between the three fields, so this is three independent additions and the sum
/// of `'1 mon'` and `'30 days'` is `1 mon 30 days` — two values a real server calls *equal* and
/// prints differently.
fn fold_interval(
    total: &mut Option<crate::value::interval::Interval>,
    seen: &mut i64,
    months: i32,
    days: i32,
    micros: i64,
) -> Result<()> {
    let running = total.unwrap_or(crate::value::interval::Interval {
        months: 0,
        days: 0,
        micros: 0,
    });
    *total = Some(crate::value::interval::Interval {
        months: running
            .months
            .checked_add(months)
            .ok_or(SqlError::IntervalOutOfRange)?,
        days: running
            .days
            .checked_add(days)
            .ok_or(SqlError::IntervalOutOfRange)?,
        micros: running
            .micros
            .checked_add(micros)
            .ok_or(SqlError::IntervalOutOfRange)?,
    });
    *seen += 1;
    Ok(())
}

/// The `numeric` an exact-typed value adds into an average.
///
/// Every width, because `avg` is chosen from the *declared* type and the datum beside it may be any
/// integer this crate holds — an `int4`-declared literal arrives as an `Int8` (ADR 0087), and an
/// `int2` column as an `Int2`.
fn exact_addend(value: &Datum) -> Result<esker_keys::numeric::Numeric> {
    Ok(match value {
        Datum::Int2(value) => crate::value::numeric::of_i64(i64::from(*value)),
        Datum::Int4(value) => crate::value::numeric::of_i64(i64::from(*value)),
        Datum::Int8(value) => crate::value::numeric::of_i64(*value),
        Datum::Numeric(value) => value.clone(),
        other => {
            return Err(SqlError::Internal(format!(
                "avg accumulated a {other:?}, which its type check refuses"
            )));
        }
    })
}

impl Accumulator {
    /// A fresh accumulator for one group.
    pub(super) fn new(spec: &AggregateSpec) -> Self {
        let state = match (spec.func, spec.arg_type) {
            (AggregateFunc::Count, _) => State::Count(0),
            (AggregateFunc::Sum, Some(ColumnType::Money)) => State::SumMoney(None),
            (AggregateFunc::Sum, Some(ColumnType::Int2 | ColumnType::Int4)) => State::SumWide(None),
            (AggregateFunc::Sum, Some(ColumnType::Int8 | ColumnType::Numeric)) => {
                State::SumNumeric(None)
            }
            // One state for both, because a sum and an average over an interval differ only in
            // whether the count is used at the end.
            (
                AggregateFunc::Sum | AggregateFunc::Avg,
                Some(ColumnType::Interval | ColumnType::Time),
            ) => State::Interval {
                total: None,
                seen: 0,
            },
            (
                AggregateFunc::Avg,
                Some(ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 | ColumnType::Numeric),
            ) => State::AvgNumeric { sum: None, seen: 0 },
            (AggregateFunc::Avg, _) => State::AvgFloat { sum: 0.0, seen: 0 },
            (AggregateFunc::Sum, _) => State::SumFloat(None),
            (AggregateFunc::Min | AggregateFunc::Max, _) => State::Extreme(None),
            (AggregateFunc::ArrayAgg, element) => State::Gather {
                element,
                values: Vec::new(),
            },
        };
        Accumulator {
            func: spec.func,
            order_by: spec.order_by.clone(),
            seen: spec.distinct.then(BTreeSet::new),
            state,
        }
    }

    /// Folds one value in. `Datum::Null` is the argument's value, not its absence: `count(*)`
    /// passes a non-NULL placeholder, so a NULL here always means the column was NULL.
    pub(super) fn push(&mut self, value: &Datum, sort_key: Vec<Datum>) -> Result<()> {
        // Every aggregate but `count(*)` skips NULLs, and `count(*)` never sees one — **except
        // `array_agg`, which collects them.** It is the one aggregate whose result has a place to
        // put a NULL, so dropping them would silently shorten the array: measured,
        // `array_agg(n ORDER BY n)` over a column with a NULL is `{10,20,30,NULL}` and not
        // `{10,20,30}`.
        if matches!(value, Datum::Null) && self.func != AggregateFunc::ArrayAgg {
            return Ok(());
        }
        // `DISTINCT` is one rule for all five rather than five implementations of it: a value
        // already folded into this group is dropped before it reaches the state.
        if let Some(seen) = &mut self.seen {
            // Bounded for the same reason the group table and the sort are: a `count(DISTINCT c)`
            // over a column with a billion values is a billion values held on a client's behalf.
            if seen.len() == GROUP_LIMIT {
                return Err(SqlError::ConfigurationLimitExceeded(format!(
                    "a DISTINCT aggregate over more than {GROUP_LIMIT} values needs more memory                      than this node will use; add a WHERE"
                )));
            }
            if !seen.insert(GroupKey(vec![value.clone()])) {
                return Ok(());
            }
        }
        match (&mut self.state, value) {
            (State::Count(count), _) => *count += 1,
            // `int2` and `int4` widen into an `int8`, which their sum cannot overflow: the
            // widest sum of `i32`s is bounded by the row count, and the group table is bounded.
            (State::SumMoney(total), Datum::Money(value)) => {
                let sum = total.unwrap_or(0);
                *total = Some(sum.checked_add(*value).ok_or(SqlError::MoneyOutOfRange)?);
            }
            (State::SumWide(total), Datum::Int2(value)) => {
                *total = Some(total.unwrap_or(0) + i64::from(*value));
            }
            (State::SumWide(total), Datum::Int4(value)) => {
                *total = Some(total.unwrap_or(0) + i64::from(*value));
            }
            // **An `int4` by declaration whose datum is still an `i64`.** The literal ladder
            // narrowed *types* and not values — ADR 0030's six stored types are unchanged — so
            // `sum(1)` now resolves as `sum(int4)`, which is the `bigint` a real server answers,
            // while the datum arriving here is an `Int8`. Checked rather than bare, because the
            // bound above is an argument about `i32`s and does not cover this one.
            (State::SumWide(total), Datum::Int8(value)) => {
                *total = Some(
                    total
                        .unwrap_or(0)
                        .checked_add(*value)
                        .ok_or(SqlError::BigintOutOfRange)?,
                );
            }
            // **`sum(int8)` is a `numeric` and cannot overflow**, which is the whole reason
            // PostgreSQL widens it — `9223372036854775807 + 1` is a value there, not `22003`.
            (State::SumNumeric(total), Datum::Int8(value)) => {
                let addend = crate::value::numeric::of_i64(*value);
                *total = Some(add_numeric(total.as_ref(), &addend));
            }
            (State::SumNumeric(total), Datum::Numeric(value)) => {
                *total = Some(add_numeric(total.as_ref(), value));
            }
            // **Three fields added independently, and checked**: a real server's
            // `sum('2147483647 days', '1 day')` is `22008 interval out of range` rather than a
            // wrapped day count, and a saturating add here would answer a value that looks like
            // data.
            (
                State::Interval { total, seen },
                Datum::Interval {
                    months,
                    days,
                    micros,
                },
            ) => fold_interval(total, seen, *months, *days, *micros)?,
            // **A `time` folds in as the duration it is being treated as**, which is how
            // `sum(time)` over `01:00`, `02:00` and `23:30` is `26:30:00` — an interval past a
            // day, and not a time of day at all.
            (State::Interval { total, seen }, Datum::Time(micros)) => {
                fold_interval(total, seen, 0, 0, *micros)?;
            }
            (State::AvgNumeric { sum, seen }, value) => {
                *sum = Some(add_numeric(sum.as_ref(), &exact_addend(value)?));
                *seen += 1;
            }
            (State::SumFloat(total), Datum::Double(value)) => {
                *total = Some(total.unwrap_or(0.0) + value);
            }
            (State::AvgFloat { sum, seen }, Datum::Double(value)) => {
                *sum += value;
                *seen += 1;
            }
            // **Every value kept, with the key it sorts under** — the fold happens at `finish`,
            // because the order is not known until the group is complete.
            (State::Gather { values, .. }, value) => {
                if values.len() == GROUP_LIMIT {
                    return Err(SqlError::ConfigurationLimitExceeded(format!(
                        "an array_agg over more than {GROUP_LIMIT} values needs more memory than                          this node will use; add a WHERE"
                    )));
                }
                values.push((sort_key, value.clone()));
            }
            (State::Extreme(best), value) => {
                let replace = match best {
                    None => true,
                    Some(best) => {
                        let ordering = value.pg_cmp(best);
                        match self.func {
                            AggregateFunc::Min => ordering.is_lt(),
                            _ => ordering.is_gt(),
                        }
                    }
                };
                if replace {
                    *best = Some(value.clone());
                }
            }
            // The planner resolved the argument's type and built the state from it, so a value of
            // another type here is a planner bug rather than a user's mistake.
            (state, value) => {
                return Err(SqlError::Internal(format!(
                    "{value:?} reached a {state:?} accumulator"
                )));
            }
        }
        Ok(())
    }

    /// The group's value.
    ///
    /// The empty case is the one worth reading: `count` is **zero** and everything else is
    /// **NULL**, which is PostgreSQL's rule and the reason a sum over no rows must not be `0` — a
    /// zero there is a wrong answer that looks like data.
    ///
    /// It returns a `Result` for one member: `avg(interval)` divides here, and a division that
    /// pushes a field out of range is `22008` rather than a wrapped number.
    pub(super) fn finish(&self) -> Result<Datum> {
        Ok(match &self.state {
            State::Count(count) => Datum::Int8(*count),
            State::SumWide(total) => total.map_or(Datum::Null, Datum::Int8),
            State::Interval {
                total: Some(total), ..
            } if self.func == AggregateFunc::Sum => Datum::Interval {
                months: total.months,
                days: total.days,
                micros: total.micros,
            },
            // **The exact sum, divided once.** Not a running mean: PostgreSQL's average over
            // intervals is `interval_div(sum, count)` and its cascade rounds twice, so a fold
            // would round once per row.
            State::Interval {
                total: Some(total),
                seen,
            } => crate::value::temporal::divide_interval(
                total.months,
                total.days,
                total.micros,
                *seen,
            )?,
            State::SumMoney(total) => total.map_or(Datum::Null, Datum::Money),
            State::SumNumeric(total) => total.clone().map_or(Datum::Null, Datum::Numeric),
            // A sum over no rows is NULL, and so is an average over none — the same rule, and
            // the reason the divisor is never zero below.
            State::AvgNumeric {
                sum: Some(sum),
                seen,
            } => crate::value::numeric::mean(sum, *seen).map_or(Datum::Null, Datum::Numeric),
            State::SumFloat(total) => total.map_or(Datum::Null, Datum::Double),
            State::Extreme(best) => best.clone().unwrap_or(Datum::Null),
            // An average over nothing is NULL, whichever accumulator held it — the same rule
            // as a sum over nothing, and the reason neither divisor is ever zero.
            State::AvgFloat { seen: 0, .. }
            | State::AvgNumeric { seen: 0, .. }
            | State::AvgNumeric { sum: None, .. }
            // The interval pair is here for the same rule, and it is why the divisor in the
            // arm above is never zero: an accumulator that saw nothing has no sum to divide.
            | State::Interval { total: None, .. } => Datum::Null,
            #[allow(
                clippy::cast_precision_loss,
                reason = "the count is the divisor PostgreSQL's own float8 average divides by"
            )]
            State::AvgFloat { sum, seen } => Datum::Double(sum / (*seen as f64)),
            // **Over no rows this is NULL, not an empty array.** Measured, and it is the answer
            // that surprises: `array_agg(id) FROM t WHERE false` is NULL where `count(id)` is 0.
            // An empty array would make `array_length(…, 1)` answer NULL for a different reason
            // and `IS NULL` answer false, so the two are not interchangeable.
            State::Gather { values, .. } if values.is_empty() => Datum::Null,
            State::Gather { element, values } => {
                let mut values = values.clone();
                // Stable, so values with equal keys keep the order they arrived in — which is the
                // input order, and is what a real server's sort does with them too.
                values.sort_by(|(left, _), (right, _)| {
                    super::cursor::compare_values(&self.order_by, left, right)
                });
                let values: Vec<Datum> = values.into_iter().map(|(_, value)| value).collect();
                match element {
                    // **A real array value, not its text.** What the declared type says the column
                    // is, the datum now is — so `pg_typeof` reads `integer[]` off the value and a
                    // client is sent the array's own oid. An element type this node cannot name
                    // keeps the text form below, which is what every `array_agg` used to answer.
                    Some(element) => Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
                        *element,
                        1,
                        values
                            .into_iter()
                            .map(|value| (!matches!(value, Datum::Null)).then_some(value))
                            .collect(),
                    )),
                    None => Datum::Text(crate::value::vector::Array::write(
                        &values.iter().map(Datum::to_text).collect::<Vec<_>>(),
                    )),
                }
            }
        })
    }
}
