//! Contract C3 for aggregates: replay `tests/corpus/pg19_aggregate.txt` and require our answer.
//!
//! The corpus is 145 probes put to a real PostgreSQL 19beta1 over a fixture this test rebuilds,
//! recording the rows, the types `\gdesc` declared for them, or the SQLSTATE and message it
//! refused with. Nothing in it was written from documentation, which is the point: an aggregate is
//! a pile of small rules — what a NULL does, what no rows at all do, which types have a `min` —
//! and every one of them is a place to be confidently wrong from memory.
//!
//! # Divergences are held from both sides
//!
//! Two lists, because there are two kinds. [`TYPE_DIVERGENCES`] is where the **rows agree** and the
//! declared type does not — every one of them is `sum(bigint)`, which PostgreSQL types `numeric`
//! and this node types `bigint`, printing the same characters for every input that does not
//! overflow (ADR 0031). [`DIVERGENCES`] is where the answer itself differs.
//!
//! Both are checked in **both directions**: an unlisted divergence fails, and so does a listed one
//! that has started agreeing. Closing a gap cannot be absorbed silently, and neither can opening
//! one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::value::PgType;

/// The fixture the corpus was captured over, and the reason the whole file is one comparison
/// rather than a hundred assertions: both servers see the same five rows.
const FIXTURE: &[&str] = &[
    "CREATE TABLE agg (id int8 PRIMARY KEY, g text, n int8, f float8, b bool, ts timestamptz, \
     y bytea)",
    "INSERT INTO agg VALUES \
     (1, 'a',  10,   1.5,   true,  '2024-01-01 00:00:00+00', '\\x01'), \
     (2, 'a',  20,   2.5,   false, '2024-06-01 12:00:00+00', '\\x02'), \
     (3, 'b',  NULL, NULL,  NULL,  NULL,                     NULL), \
     (4, 'b',  -5,   0.5,   true,  '2023-01-01 00:00:00+00', '\\xff'), \
     (5, NULL, 7,    'NaN', false, '2025-01-01 00:00:00+00', '\\x00')",
    "CREATE TABLE wide (id int8 PRIMARY KEY, n int8, g text)",
    "CREATE TABLE big (id int8 PRIMARY KEY, n int8)",
    "INSERT INTO big VALUES (1, 9223372036854775807), (2, 9223372036854775807)",
];

/// The rows agree; the type in `RowDescription` does not.
///
/// Every entry is `sum(bigint)` and every entry has one reason, ADR 0031: PostgreSQL's `sum` over
/// a `bigint` is `numeric` and this node has no `numeric`. For every input that does not overflow
/// an `int8`, the two print **the same characters** — which is what makes this a type divergence a
/// client sees only in the OID rather than a wrong number. The input that *does* overflow is in
/// [`DIVERGENCES`], where it belongs.
const TYPE_DIVERGENCES: &[&str] = &[
    "SELECT sum(n) FROM wide",
    "SELECT sum(n) FROM agg WHERE n > 1000",
    "SELECT sum(n) FROM agg",
    "SELECT sum(DISTINCT n) FROM agg",
    "SELECT g, count(n), sum(n), min(n), max(n) FROM agg GROUP BY g ORDER BY g",
    "SELECT g, sum(n) FROM agg GROUP BY g ORDER BY g",
    "SELECT sum(n) FROM agg HAVING sum(n) IS NOT NULL",
    "SELECT sum(n) FROM agg GROUP BY g ORDER BY sum(n) NULLS LAST",
    "SELECT sum(n) FROM big GROUP BY id ORDER BY id",
];

/// Queries this node answers differently, each with its reason.
///
/// A `0A000` here is contract C2 working — the construct is named rather than approximated — and
/// the two that are *not* `0A000` are the ones to read: `sum` overflowing, which is the visible
/// edge of ADR 0031, and the group order, which PostgreSQL does not promise and this node does.
const DIVERGENCES: &[(&str, &str)] = &[
    // ADR 0031, and the whole of what an int8 sum costs.
    (
        "SELECT sum(n) FROM big",
        "PostgreSQL's sum(bigint) is numeric and cannot overflow; ours is int8 and answers 22003 \
         rather than wrapping",
    ),
    // avg over an integer column: numeric with sixteen fractional digits, which no float8 renders.
    (
        "SELECT avg(n) FROM wide",
        "avg(bigint) is numeric there and 0A000 here",
    ),
    (
        "SELECT avg(n) FROM agg",
        "avg(bigint) is numeric there and 0A000 here",
    ),
    (
        "SELECT avg(n) FROM agg WHERE id = 1",
        "avg(bigint) is numeric there and 0A000 here",
    ),
    (
        "SELECT avg(n) FROM agg WHERE id IN (1, 4)",
        "avg(bigint) is numeric there and 0A000 here",
    ),
    (
        "SELECT avg(n) FROM agg WHERE id IN (1, 2)",
        "avg(bigint) is numeric there and 0A000 here",
    ),
    (
        "SELECT avg(n) FROM agg WHERE id IN (1, 2, 4)",
        "avg(bigint) is numeric there and 0A000 here",
    ),
    (
        "SELECT avg(id) FROM agg",
        "avg(bigint) is numeric there and 0A000 here",
    ),
    (
        "SELECT avg(n)::text FROM agg",
        "a cast, and avg(bigint) under it",
    ),
    (
        "SELECT avg(n), sum(n), count(n), min(n), max(n) FROM agg GROUP BY g ORDER BY g",
        "avg(bigint) is numeric there and 0A000 here",
    ),
    // The group order, which is a promise PostgreSQL does not make and this node does.
    (
        "SELECT g, count(*) FROM agg GROUP BY g",
        "with no ORDER BY, PostgreSQL returns groups in hash order and this node returns them in \
         pg_cmp order of the key — deterministic, and a superset of what PostgreSQL guarantees",
    ),
    (
        "SELECT count(*) FROM agg GROUP BY g LIMIT 1",
        "the same: with no ORDER BY, which group is first is PostgreSQL's hash order and ours is \
         the smallest key",
    ),
    // Contract C2: parsed, named, not executed. Each is a unit of its own or explicitly out of
    // scope in `docs/plans/phase-9-rails.md` §5.
    (
        "SELECT count(*) FROM agg, wide",
        "a comma-separated FROM list",
    ),
    (
        "SELECT count(*) FROM agg a JOIN agg b ON a.id = b.id",
        "a table alias",
    ),
    (
        "SELECT sum(a.n) FROM agg a JOIN agg b ON a.id = b.id",
        "a table alias",
    ),
    ("SELECT count(*) FROM (SELECT 1) s", "a derived table"),
    (
        "SELECT g, count(*) FROM agg GROUP BY GROUPING SETS ((g), ())",
        "GROUP BY GROUPING SETS",
    ),
    (
        "SELECT count(*) FILTER (WHERE n > 0) FROM agg",
        "an aggregate FILTER clause",
    ),
    ("SELECT count(*) OVER () FROM agg", "a window function"),
    (
        "SELECT count(*) FROM wide GROUP BY 'x'",
        "a non-integer constant in GROUP BY is 42601 there and an ordinary one-group key here",
    ),
    (
        "SELECT bool_and(b), bool_or(b) FROM agg",
        "bool_and and bool_or are not among the five aggregates",
    ),
    (
        "SELECT string_agg(g, ',') FROM agg",
        "string_agg is not among the five aggregates",
    ),
    (
        "SELECT array_agg(n) FROM agg",
        "array_agg, and there is no array type",
    ),
    ("SELECT count(*) + 1 FROM agg", "arithmetic"),
    ("SELECT sum(n) + 0 FROM agg", "arithmetic"),
    (
        "SELECT DISTINCT ON (g) g, n FROM agg ORDER BY g, n",
        "SELECT DISTINCT ON",
    ),
    (
        "SELECT pg_typeof(count(*)), pg_typeof(sum(n)), pg_typeof(avg(n)), pg_typeof(sum(f)), \
         pg_typeof(avg(f)) FROM agg",
        "pg_typeof, which needs the catalog unit",
    ),
];

#[test]
fn every_aggregate_answers_the_way_postgresql_19_does() {
    let mut node = Node::new();
    let mut checked = 0;
    let mut mismatched = Vec::new();
    let mut type_mismatched = Vec::new();
    let mut agreed_after_all = Vec::new();

    for (line_number, query, expected) in corpus() {
        let listed = DIVERGENCES.iter().find(|(sql, _)| *sql == query);
        let actual = node.answer(&query);

        if listed.is_some() {
            if actual == expected {
                agreed_after_all.push(format!("line {line_number}: {query}"));
            }
            checked += 1;
            continue;
        }

        match (&expected, &actual) {
            (
                Answer::Rows { types, rows },
                Answer::Rows {
                    types: ours,
                    rows: theirs,
                },
            ) if rows == theirs && types != ours => {
                if !TYPE_DIVERGENCES.contains(&query.as_str()) {
                    type_mismatched.push(format!(
                        "line {line_number}: {query}\n  PostgreSQL: {types:?}\n  Esker:      {ours:?}"
                    ));
                }
            }
            _ if actual == expected => {
                if TYPE_DIVERGENCES.contains(&query.as_str()) {
                    agreed_after_all.push(format!("line {line_number}: {query}"));
                }
            }
            _ => mismatched.push(format!(
                "line {line_number}: {query}\n  PostgreSQL: {expected}\n  Esker:      {actual}"
            )),
        }
        checked += 1;
    }

    assert!(
        mismatched.is_empty(),
        "{} of {checked} probes disagree with PostgreSQL 19 and are not listed as \
         divergences:\n\n{}",
        mismatched.len(),
        mismatched.join("\n\n")
    );
    assert!(
        type_mismatched.is_empty(),
        "{} probes have the right rows and an unlisted type divergence:\n\n{}",
        type_mismatched.len(),
        type_mismatched.join("\n\n")
    );
    assert!(
        agreed_after_all.is_empty(),
        "{} probes are listed as divergences and now agree with PostgreSQL -- delete the \
         entries:\n\n{}",
        agreed_after_all.len(),
        agreed_after_all.join("\n")
    );
    assert!(
        checked > 140,
        "only {checked} probes ran; the corpus did not load"
    );
}

/// What one probe answered: rows with their declared types, or a refusal.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Rows {
        types: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Refused(String),
}

impl std::fmt::Display for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answer::Refused(message) => write!(f, "!{message}"),
            Answer::Rows { types, rows } => write!(
                f,
                "{}\t{}",
                types.join(","),
                if rows.is_empty() {
                    "-".to_owned()
                } else {
                    rows.iter()
                        .map(|row| row.join("|"))
                        .collect::<Vec<_>>()
                        .join(" ; ")
                }
            ),
        }
    }
}

struct Node {
    executor: Executor,
}

impl Node {
    fn new() -> Self {
        let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let mut node = Node {
            executor: Executor::new(backend, Arc::new(Catalog::new()), 1),
        };
        for statement in FIXTURE {
            node.run(statement)
                .unwrap_or_else(|error| panic!("the fixture did not load: {statement}\n{error}"));
        }
        node
    }

    fn run(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = self.executor.execute(&parsed, &Params::NONE)?;
        }
        Ok(last)
    }

    /// One probe, in the corpus's own shape.
    fn answer(&mut self, sql: &str) -> Answer {
        match self.run(sql) {
            Err(error) => {
                let mut message = format!("{} {error}", error.sqlstate());
                if let Some(detail) = error.detail() {
                    let _ = write!(message, " DETAIL: {detail}");
                }
                if let Some(hint) = error.hint() {
                    let _ = write!(message, " HINT: {hint}");
                }
                Answer::Refused(message)
            }
            Ok(Outcome::Done { tag }) => Answer::Refused(format!("not a query: {tag}")),
            Ok(Outcome::Rows { fields, rows, .. }) => Answer::Rows {
                types: fields
                    .iter()
                    .map(|field| type_name(field.type_oid).to_owned())
                    .collect(),
                rows: rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|value| {
                                value.map_or_else(
                                    || "\\N".to_owned(),
                                    |bytes| String::from_utf8(bytes).unwrap(),
                                )
                            })
                            .collect()
                    })
                    .collect(),
            },
        }
    }
}

/// The name `\gdesc` prints for an OID, which is the name [`PgType`] already knows.
fn type_name(oid: u32) -> &'static str {
    esker_sql::value::ColumnType::ALL
        .into_iter()
        .find(|ty| ty.oid() == oid)
        .map_or("?", PgType::name)
}

/// `query <tab> types <tab> rows`, or `query <tab> !SQLSTATE message`.
fn corpus() -> Vec<(usize, String, Answer)> {
    include_str!("corpus/pg19_aggregate.txt")
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|(index, line)| {
            let mut fields = line.split('\t');
            let query = fields.next().expect("a query").to_owned();
            let second = fields
                .next()
                .unwrap_or_else(|| panic!("line {}: no answer", index + 1));
            let answer = match second.strip_prefix('!') {
                Some(message) => Answer::Refused(message.to_owned()),
                None => Answer::Rows {
                    types: second.split(',').map(str::to_owned).collect(),
                    rows: match fields.next().expect("rows") {
                        "-" => Vec::new(),
                        rows => rows
                            .split(" ; ")
                            .map(|row| row.split('|').map(str::to_owned).collect())
                            .collect(),
                    },
                },
            };
            (index + 1, query, answer)
        })
        .collect()
}
