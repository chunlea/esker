# 0031 — Rails compatibility is a measured number, not a claim

## Context

Phase 6a built a node that speaks PostgreSQL's wire protocol and executes a subset of PostgreSQL's
SQL, and it built three contracts to keep that subset honest (`docs/plans/phase-6a.md` §1):
**C1** no valid PG-19 statement is a parse error, **C2** parsed-but-unimplemented is `0A000` naming
the construct, **C3** what we do execute matches PostgreSQL 19 exactly. Those contracts are
checked against a real PostgreSQL 19beta1 in a container, statement by statement, and they have
held for every unit since.

They do not answer the question wave B exists for: **can a real application run on this?**

C1, C2 and C3 are each a statement about *one statement at a time*. An application is not a
statement. Rails boots by asking the catalog what the schema is, wraps its tests in a transaction
it rolls back, issues `SAVEPOINT` for nested cases, reads `serial` columns back through `RETURNING`,
counts with `SELECT COUNT(*)`, and joins through its associations — and every one of those is a
place where a subset can be individually honest and collectively useless. A node can answer `0A000`
correctly for `count(*)` and be a node no Rails application can use.

There is a second problem, which is that "compatible with PostgreSQL" is not falsifiable as
usually stated. Every database that speaks the wire protocol says it. The claim is worth nothing
without a number attached and a way for a reader to reproduce it.

## Decision

**ActiveRecord's own test suite is the oracle, and its pass rate is the compatibility number.**

`rails/rails`'s `activerecord/test` directory, run with `ARCONN=postgresql` against this node
instead of against a real server, produces a pass/fail per test. That number, and the list of tests
excluded from it with a reason each, is the whole of what this project will claim about Rails
compatibility. It lives in `docs/bench/rails-scoreboard.md` and it is regenerated, not edited.

This is CockroachDB's discipline and it is taken deliberately: they publish a pass rate against the
ORM's own suite together with an exclusion list where every entry names the feature it is waiting
on. A suite written by the framework's authors tests the things the framework actually does, in the
order it actually does them, and it cannot be curated into flattery by the database's authors.

### The first number is published whatever it is

The first run's pass rate is the baseline. It will be low. Publishing a low number is the point:
an exclusion list is only honest if it was written *after* the run rather than chosen to produce a
number. A test excluded before it has ever been run is a test somebody guessed about.

Three rules govern the exclusion list:

1. **Every entry names its reason and the unit that would close it.** "Fails" is not a reason.
   `0A000 numeric` is a reason; so is "needs `pg_catalog.pg_index`".
2. **An entry that starts passing is deleted, and the deletion is a commit.** An exclusion list
   that only grows is a list nobody re-runs.
3. **No entry may be added for a wrong answer.** A test excluded because this node returns
   *different data* is a bug, not an exclusion, and it goes on the divergence table in
   `docs/plans/phase-6a.md` §10a where a reader will find it — or it gets fixed.

### The isolation level is the one caveat the number cannot absorb

This node runs Percolator transactions: snapshot isolation, with a write-write conflict surfacing
as `40001 serialization_failure`. PostgreSQL's default is `READ COMMITTED`, which does not produce
`40001` at all for the statements Rails issues, and **Rails does not retry** — `ActiveRecord::Base`
has no built-in retry loop around a transaction block, and a `40001` reaches the application as an
exception.

That is a real difference and it is not fixable by making the SQL layer cleverer. The response is
the one CockroachDB's adapter takes: **the retry lives in the adapter, not in the database.** Their
`activerecord-cockroachdb-adapter` absorbs `40001` and re-runs the block. Any measurement here that
requires retry-on-conflict is measured with that pattern in place and says so on the scoreboard
line; a test that fails *only* because of an unretried `40001` is recorded as such and is not
counted as a compatibility gap in the SQL surface, because it is not one.

The scoreboard therefore carries two numbers: the raw pass rate, and the pass rate with conflict
retry. The difference between them is the price of snapshot isolation, stated rather than hidden.

## The first thing the oracle decided: there is no `numeric`, and it decides `sum` and `avg`
differently

Unit 1 is aggregates, and aggregates are where the six stored types (ADR 0030 — `int8`, `text`,
`bool`, `bytea`, `timestamptz`, `float8`) meet a seventh that PostgreSQL has and this node does
not. Every claim below was measured on the container and is recorded line by line in
`crates/esker-sql/tests/corpus/pg19_aggregate.txt`.

PostgreSQL types **`sum(bigint)` and `avg(bigint)` as `numeric`**. We cannot. The question is what
to do about each, and the measurement gives two different answers, which is why they are decided
separately rather than together.

**`sum(bigint)` is implemented as `int8`.** For every input that does not overflow, `numeric`'s
text and `int8`'s text are *the same characters*: PostgreSQL printed `32` and `-5`, and an `int8`
prints `32` and `-5`. The divergence is confined to the `RowDescription` type OID (20 rather than
1700) and to the inputs that overflow, where PostgreSQL cannot — it printed
`18446744073709551614` for two `bigint` maxima — and where we return an error rather than a wrapped
number. A wrapped number is the failure mode this whole project is built to refuse, and an error
naming the overflow is the honest half.

**`avg(bigint)` is refused with `0A000`.** Here the texts are *not* the same characters, and no
choice of an existing type makes them so. PostgreSQL's `avg(bigint)` is `numeric` with sixteen
fractional digits — measured: `8.0000000000000000`, `2.5000000000000000`, `8.3333333333333333` —
and the nearest `float8` renderings are `8`, `2.5` and `8.333333333333334`. Answering with a
`float8` would be a different value in the last digit and a different type at the client, where
Rails' `pg` gem maps `numeric` to `BigDecimal` and `float8` to `Float`. `avg(float8)` **is**
implemented, because there PostgreSQL returns `double precision` and we are exactly right.

So the rule that decides both, and that will decide the next type question the same way:

> Implement it when our type reproduces PostgreSQL's *text* for every input that does not error.
> Refuse it by name when it cannot.

`avg` over an integer column is therefore the first entry on wave B's numeric backlog, and the
number of ActiveRecord tests it fails is the argument for building `numeric` — an argument made of
measurements rather than of taste. When `numeric` lands, the `0A000` becomes an answer and no
application that was working breaks, which is what refusing rather than approximating bought.

### The sibling: what the columnar evaluator already decided

`esker-columnar`'s fragment evaluator defined these semantics first
(`docs/plans/phase-7-columnar.md` M2), because at the time the row side had no aggregates at all to
match. The row side now matches it: `count(*)` counts every row, `count(col)` skips NULLs, `sum`
and `min` and `max` over no rows or only NULLs are NULL rather than zero, `min`/`max` order by
`pg_cmp`, a NULL grouping key forms one group of its own, and **`sum(int8)` overflowing is an error
on both sides.**

One thing does not match, and it is recorded here so that milestone M4 — the planner routing a
query to either replica — treats it as a decision rather than discovers it as a bug:

* **`avg` is not in the columnar aggregate set.** `Aggregate` has `CountStar`, `Count`, `Sum`,
  `Min` and `Max` and no `Avg`. The row side refuses `avg(int8)` and implements `avg(float8)`, so
  M4 must either add `Avg` to the fragment protocol — a format change, and therefore an ADR — or
  route any query containing `avg` to the row side. Routing is the cheaper answer and it is
  correct today.
* **`min`/`max` over `bool` is `42883` on both sides, and that is parity rather than a shortfall.**
  Measured: PostgreSQL 19 has no `min(boolean)`. Implementing one would have been the divergence.

## Consequences

- Compatibility becomes falsifiable. Anyone can run the harness and get the same number, and a
  regression shows up as a number going down rather than as an application failing in production.
- The exclusion list becomes the roadmap. The units after aggregates — sequences and `RETURNING`,
  savepoints, joins, `pg_catalog` — are ordered by what the suite cannot get past, not by what
  looked next.
- We accept a dependency on somebody else's test suite, including its churn between Rails versions.
  The scoreboard records the Rails commit it was run against, for the same reason the value corpus
  records `19beta1`: a number without the version it was measured on is not reproducible.
- The `40001` caveat is permanent for as long as this node is snapshot-isolated, and it is the one
  place where "run Rails on Esker" needs something on the Ruby side. Naming it in this ADR means
  the scoreboard does not have to re-argue it every time it is regenerated.
