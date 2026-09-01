# Phase 9 plan — a PostgreSQL that Rails can talk to, scored by Rails' own tests

Status: **unit 0 landed** (this file, [ADR 0031](../adr/0031-rails-compatibility-is-measured.md),
`crates/esker-sql/tests/corpus/pg19_aggregate.txt`). §8 records progress per unit.

Design: [ADR 0031](../adr/0031-rails-compatibility-is-measured.md). Constitution: `CLAUDE.md`.
The compatibility contract this inherits whole: `docs/plans/phase-6a.md` §1 — **C1** every valid
PG-19 statement parses, **C2** parsed-but-unimplemented is `0A000` naming the construct, **C3**
what we execute matches PostgreSQL 19 exactly. The divergence table this phase adds rows to:
`docs/plans/phase-6a.md` §10a.

Lane: `crates/esker-sql/src/**` and `crates/esker-sql/tests/**` except `tests/joint_gate.rs` and
`tests/pd_wiring.rs`, which belong to another lane and are not touched here.

## 1. What "Rails can talk to it" is measured by

Not by a claim. By `activerecord/test` with `ARCONN=postgresql` pointed at this node, producing a
pass rate and an exclusion list where every entry names its reason and the unit that closes it —
`docs/bench/rails-scoreboard.md`, regenerated rather than edited. ADR 0031 is the argument for why
this is the oracle and what the isolation-level caveat costs; it is not repeated here.

The consequence for *this file* is the ordering. The units below are not a tour of SQL features.
They are what a Rails application does between `rails new` and a green test, in the order it does
it:

| What Rails does | What it needs | Unit |
|---|---|---|
| `Model.count`, `.sum`, `.average`, `.group` | aggregates, `GROUP BY`, `HAVING`, `DISTINCT` | 1 |
| `Model.create!` and reads the id back | `serial`/`identity`, a sequence, `RETURNING` | 2 |
| wraps every test in a transaction it rolls back, nested cases included | `SAVEPOINT` / `ROLLBACK TO` / `RELEASE` | 3 |
| `has_many`, `includes`, `joins` | `INNER`/`LEFT JOIN`, `ON` and `USING` | 4 |
| boots, and dumps the schema | `pg_catalog` and `information_schema`, read-only | 5 |
| everything above, scored | the harness | 6 |

## 2. Units, in order, each its own commit

### Unit 0 — the ADR, this plan, and the aggregate capture ✅

ADR 0031 decides that the suite is the oracle, that the exclusion list has three rules, and — the
part that changes code — how a missing `numeric` is answered: **implement it where our type
reproduces PostgreSQL's text for every input that does not error, refuse it by name where it
cannot.** That rule makes `sum(int8)` an `int8` and `avg(int8)` a `0A000`, and it was arrived at
from `tests/corpus/pg19_aggregate.txt` rather than from taste.

The capture is 115 probes against the `esker-pg19` container, and it is unit 1's specification.
Four of its lines would have been got wrong by reading rather than measuring, and they are called
out at the top of the file: `min(boolean)` does not exist, a false `HAVING` over an ungrouped
aggregate returns **no rows**, `GROUP BY` may name an output alias where `HAVING` may not, and
`sum(bigint)` over two `int8` maxima is a number no `int8` holds.

### Unit 1 — aggregates: `count`, `sum`, `min`, `max`, `avg`, `GROUP BY`, `HAVING`, `DISTINCT`

The executor grows one node and the planner grows one decision. Semantics come from the capture,
and they match `esker-columnar`'s fragment evaluator (`docs/plans/phase-7-columnar.md` M2) except
where ADR 0031 records why not.

* **`plan::Expr::Aggregate`** — a call, its argument, and whether `DISTINCT` was written. Nested
  aggregates and aggregates in `WHERE` are `42803` with PostgreSQL's own messages.
* **`Node::Aggregate`** — grouping keys, aggregate calls, and a `HAVING` predicate over the
  aggregated row. It cannot stream: it drains its input, bounded the way `Sort` is, and answers
  `53400` past the bound rather than allocating without limit.
* **Groups come back in `pg_cmp` order of their keys.** PostgreSQL promises no order at all
  without an `ORDER BY` (measured: it returned the NULL group first). Ours is deterministic, which
  is a superset of what PostgreSQL guarantees and what a byte-comparing harness needs — the same
  choice the columnar evaluator made, for the same reason.
* **The empty-input rules**, which are two rules and not one: an **ungrouped** aggregate over no
  rows is one row (`count` 0, everything else NULL); a **grouped** one is no rows at all.
* **`SELECT DISTINCT`** and `count(DISTINCT col)`. `DISTINCT ON` is `0A000` naming itself.
* **Type resolution**: `count` → `int8`; `sum`/`min`/`max` → the argument's type; `avg(float8)` →
  `float8`. `sum` over `text`, `bool`, `bytea`, `timestamptz` and `min`/`max` over `bool` are
  `42883` with PostgreSQL's message, `DETAIL` and `HINT` — parity, not a shortfall.
* **`avg(int8)` is `0A000`** naming `numeric`, per ADR 0031, and is the first entry on the numeric
  backlog.

### Unit 2 — sequences, `serial`/`identity`, and `RETURNING`

A sequence is a catalog record with its own kind byte and its own golden, next to the table and
index records in the `'m'` key space (`crates/esker-sql/src/catalog/record.rs`). `serial` and
`bigserial` are the column-level spellings that create one; `GENERATED ... AS IDENTITY` parses
already (phase-6a §9 G07 is the `CREATE SEQUENCE` *options*, not the column form).

`nextval` and `currval` are the two verbs. The hard part is not the counter, it is that a counter
under Percolator either serialises every insert through one key or stops being gap-free —
PostgreSQL's own sequences are **not** gap-free either (a rolled-back transaction consumes its
value), which is the licence to cache a block per session. Captured before it is decided.

`INSERT`/`UPDATE`/`DELETE ... RETURNING` is the other half, and it is the half Rails cannot work
without: `Model.create!` reads the id back through it.

### Unit 3 — `SAVEPOINT`, `ROLLBACK TO`, `RELEASE`

Rails wraps each test in a transaction and rolls it back, and nests with savepoints. Percolator has
no nested transaction, so this is an **undo log over the write buffer**: a savepoint is a mark, a
`ROLLBACK TO` truncates the buffer to it, a `RELEASE` drops the mark. Designed honestly against the
buffer rather than approximated — the failure mode to avoid is a `ROLLBACK TO` that leaves a write
behind, which is a wrong answer with no error. PostgreSQL's semantics for a savepoint that does not
exist (`3B001`), and for the state of a transaction after an error inside a savepoint, are captured
before any of it is written.

### Unit 4 — `INNER` and `LEFT JOIN`, `ON` and `USING`

One inner join and one cross join exist today (`plan::Join`, `Node::NestedLoop`, `Probe`). This
unit adds `LEFT` — which is the one that must not drop rows — a second join, and `USING`. Nested
loop only; correctness over speed, and a bench afterwards rather than a plan built for one. The
row-count explosion gets the guard `Sort` already has: a bound, and `53400` naming it, rather than
an allocation on a client's behalf.

### Unit 5 — `pg_catalog` and `information_schema`, read-only

ActiveRecord boots by interrogating the catalog. This unit does not build a catalog *storage*
surface — it **translates** ours: `pg_class`, `pg_attribute`, `pg_type`, `pg_index`, `pg_namespace`,
`pg_constraint`, and `information_schema.tables` / `.columns`, each a read-only view over the `'m'`
key space.

The queries are not guessed. A real `rails new` is run against the PG19 container with statement
logging on, and the statements AR 8.x actually issues at boot and at `db:schema:dump` are the
specification. Anything outside that set is out of scope for this unit.

### Unit 6 — the scoreboard

A harness that points `activerecord/test`'s `config.yml` at this node, runs the suite, and writes
`docs/bench/rails-scoreboard.md`: the Rails commit, the raw pass rate, the pass rate with conflict
retry, and one line per excluded test with its reason. The first run's number is the baseline
whatever it is.

## 3. The test ladder

Each rung is a thing that either works or does not, and none of them is reached by asserting
about it in Rust:

1. **`pg` gem raw connect** — `PG.connect` and one `SELECT 1`, over a socket.
2. **`ActiveRecord::Base.establish_connection` + one migration** — this is where `pg_catalog`
   stops being optional.
3. **A scaffolded CRUD app** — create, read, update, destroy, through the real adapter.
4. **The full ActiveRecord suite**, with the exclusion list.

Rungs 1–3 are scripts under `tools/rails/`, runnable by hand and by the harness. Rung 4 is unit 6.

Inside the crate the existing shapes carry the weight, and each unit adds to all four:
`tests/slt/*.slt` for the surface, `tests/lowering.rs` for the C2 refusals, a `*_parity.rs`
replaying that unit's capture corpus, and `tests/real_backend.rs` for the same answers against
three real stores rather than the in-memory fake.

## 4. Risks

* **The suite is somebody else's, and it moves.** Mitigated by recording the Rails commit on the
  scoreboard, the way the value corpus records `19beta1`.
* **A low first number reads as failure.** It is not; it is a baseline, and ADR 0031's rule 3 is
  what keeps it from being gamed. The number that matters is the second one.
* **`40001` under a framework that does not retry.** ADR 0031: the retry belongs in the adapter,
  the scoreboard carries both numbers, and the difference is the stated price of snapshot
  isolation.
* **`numeric` is load-bearing for more than `avg`.** Money columns, `decimal` in migrations, and
  `avg` all want it. This phase refuses each by name and counts what that costs, so that building
  it is a decision with a number behind it.
* **Two lanes share `esker-sql`.** This one owns `src/**` and every test but `joint_gate.rs` and
  `pd_wiring.rs`. Explicit pathspecs on every `git add`, and `git status` before and after.

## 5. What this phase will NOT do

* **No `pg_catalog` write path.** The catalog views are read-only translations. `INSERT INTO
  pg_class` is `0A000`, as is `ALTER SYSTEM` and every other administrative surface phase-6a §9
  already classifies as such.
* **No `plpgsql`, no `CREATE FUNCTION`, no triggers.** They parse (C1) and they are `0A000` (C2).
* **No `numeric` type.** Refused by name and counted, per ADR 0031. It gets its own ADR and its own
  unit when the count justifies it.
* **No window functions, no CTEs, no subqueries in `FROM`.** Each is `0A000` today and stays so;
  a subquery is the next thing after this phase, not inside it.
* **No planner cost model.** The join stays nested-loop and rule-based. Benchmarks are recorded
  (`docs/bench/`) and are not gates, per `CLAUDE.md`.
* **No adapter fork.** The conflict-retry pattern is used as CockroachDB's adapter uses it; writing
  and maintaining an `activerecord-esker-adapter` is not this phase.

## 6. Progress

| Unit | State | Commit |
|---|---|---|
| 0 — ADR, plan, aggregate capture | **done** | this commit |
| 1 — aggregates | in progress | |
| 2 — sequences and `RETURNING` | not started | |
| 3 — savepoints | not started | |
| 4 — joins | not started | |
| 5 — `pg_catalog` | not started | |
| 6 — the scoreboard | not started | |
