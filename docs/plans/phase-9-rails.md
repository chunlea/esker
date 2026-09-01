# Phase 9 plan — a PostgreSQL that Rails can talk to, scored by Rails' own tests

Status: **units 0 and 1 landed.** §8 records progress per unit, §6 the divergences and §7 what unit 1 changed.

Design: [ADR 0031](../adr/0031-rails-compatibility-is-measured.md). Constitution: `CLAUDE.md`.
The compatibility contract this inherits whole: `docs/plans/phase-6a.md` §1 — **C1** every valid
PG-19 statement parses, **C2** parsed-but-unimplemented is `0A000` naming the construct, **C3**
what we execute matches PostgreSQL 19 exactly. The divergences this phase adds are §6 of this
file — they belong beside `docs/plans/phase-6a.md` §10a and are here because that file is another
lane's while this phase runs.

Lane: `crates/esker-sql/src/**` and `crates/esker-sql/tests/**` except `tests/joint_gate.rs` and
`tests/pd_wiring.rs`, which belong to another lane and are not touched here.

## 0. The repository boundary, which is a rule and not a preference

**This repository is 100% Rust plus documentation. No Ruby and no Rails code enters it — not a
`Gemfile`, not a `config.yml`, not a runner script.** `CLAUDE.md`'s dependency policy is a statement
about what this project *is*, and a Ruby harness checked in beside the Rust would make "pure Rust,
as few crates as possible" a claim with an asterisk on it.

The harness therefore lives outside the tree, at
`/Users/chunlea/workspace/lab/esker-rails-harness/`: the `rails/rails` checkout, the `config.yml`
that points ActiveRecord at this node, the runner, and the exclusion-list runner. It is never
committed here.

What crosses the boundary back into the repository is exactly three kinds of thing, and each of
them is a plain artefact a Rust test or a reader can use:

1. **The SQL ActiveRecord issues, captured as a plain-text fixture** under
   `crates/esker-sql/tests/corpus/`, in the same convention `docs/plans/phase-6d.md` established —
   statement, then what a real PostgreSQL 19 answered. A Rust test replays it. This is how AR's
   boot queries become a specification without a line of Ruby.
2. **The scoreboard results**, as a markdown table in `docs/bench/rails-scoreboard.md`, together
   with the exact commands that reproduce the run against the out-of-tree harness.
3. **Rust features**, which is everything in `crates/esker-sql/`.

A number nobody can reproduce is not a measurement, so the reproduce-it commands are part of the
scoreboard rather than a `README` in someone's home directory.

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

The capture is 145 probes against the `esker-pg19` container, and it is unit 1's specification.
Six of its lines would have been got wrong by reading rather than measuring, and they are called
out at the top of the file: `min(boolean)` does not exist, a false `HAVING` over an ungrouped
aggregate returns **no rows**, `GROUP BY` may name an output alias where `HAVING` may not,
`sum(bigint)` over two `int8` maxima is a number no `int8` holds, `GROUP BY ()` is the grand total
rather than a grouped query, and the wrong *number* of arguments names the argument **types**
anyway.

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
index records in the `'m'` key space (`crates/esker-sql/src/catalog/record.rs`).

**`serial` is `0A000` naming itself, and `bigserial` is the path.** `serial` *is* `int4` — it is
shorthand for an `integer` column with a default — and this crate has no `int4`: `CREATE TABLE t
(a int4)` is already `0A000 the type INT`, and answering `serial` with an `int8` would accept every
value between 2^31 and 2^63 that a real server refuses with `22003`. That is ADR 0031's rule in the
direction it cares most about — accepting what the oracle rejects — so `serial` is refused by the
same sentence as the type it stands for. It costs nothing that matters here: Rails has defaulted to
`bigint` primary keys since 5.1, so an ActiveRecord 8 schema asks for `bigserial` or an identity
column and never for `serial`. `GENERATED ... AS IDENTITY` parses already (phase-6a §9 G07 is the
`CREATE SEQUENCE` *options*, not the column form).

`nextval`, `currval`, `setval` and `lastval` are the four verbs, and they run in a `SELECT` with
no `FROM` — which is where every client writes one. Over a table a sequence function is a side
effect **per row** on a real server (measured: `SELECT nextval('s') FROM t` over four rows answers
1, 2, 3, 4), so that shape is `0A000` naming the function rather than quietly running once and
handing a client one number where it expected four.

The argument is a **name, not a string**: `nextval('W1_ID_SEQ')` finds `w1_id_seq` because the text
inside the quotes folds exactly as an identifier does, and a `public.` qualifier is a schema
reference rather than part of the name — `pg_get_serial_sequence` answers `public.t_id_seq` and
clients pass that straight back.

The hard part was never the counter, it is that a counter under Percolator either serialises every
insert through one key or stops being gap-free — PostgreSQL's own sequences are **not** gap-free
either (a rolled-back transaction consumes its value), which is the licence to cache a block per
session. `setval` therefore has to **discard the session's block**, or the next `nextval` would
keep handing out numbers reserved before it and the statement would have done nothing visible.
That is the one place the batch could have been silently wrong, and it is a test of its own —
the bug would show up as the *second* value after a `setval`, not the first.

`DEFAULT` written where a value goes is not a value: it is the column keeping its own default, so
a `bigserial` still takes a number and a column with no default gets NULL. It is also **not** an
explicit value, so a `GENERATED ALWAYS` column accepts it where it refuses a number — the one
place the two clauses have to be told apart, and measured on both.

`INSERT`/`UPDATE`/`DELETE ... RETURNING` is the other half, and it is the half Rails cannot work
without: `Model.create!` reads the id back through it. **Landed first**, because it is independent
of the sequence and it is what every other unit's tests will want to write.

`RETURNING` reuses the `SELECT` target list whole — one `lower_projection`, one `Scope`, one set of
name-and-type rules — so `RETURNING *` returns what `SELECT *` returns, in the same order, under
the same names, and there is no second place for the two to drift. Which row it sees is the whole
of the semantics and all three were measured: an `INSERT` answers with the row **as stored**, so a
column filled from its `DEFAULT` comes back with that value; an `UPDATE` with the row **after** the
assignments; a `DELETE` with the row as it was, gathered before it goes because afterwards there is
nothing to read.

Two things the corpus could not hold, so they are tests of their own. The **command tag** is
unchanged by a `RETURNING` — `psql` prints a result set where it would have printed the tag, so the
container could not be asked, and the assertion is against the tags this crate already pins for the
same statement without one. And a prepared `RETURNING` **describes its columns**: answering "no
columns" and then sending some is the one thing a `Describe` exists to prevent.

*Paid:* `tests/parity_harness/` is the shared replay, extracted at the third corpus as promised —
`aggregate_parity.rs`, `returning.rs` and `sequence.rs` were three copies of one loop and are now
one. It grew a `Done` answer so a corpus can hold DDL, which is what made a stateful sequence
corpus possible at all.

**Where the sequence lives, and why the table record did not change.** A sequence is keyed by the
column it fills — `'m' ++ "sql" ++ 'q' ++ tenant ++ table ++ column` — so one table's sequences are
a prefix scan, done where the table is loaded and cached with it. The table record has a format
version and readers on both sides of it; a feature that can be added without touching it is a
feature that cannot break one. Its **value** is a second record, for the reason the row-id counter
already gives: the definition is written once and the counter on every allocation, so one record
for both would rewrite a definition to hand out a number.

`nextval` runs in a transaction of its own, which is the semantics rather than an implementation
detail: a statement that fails or rolls back has still consumed its value. That is what licenses a
sequence to leave gaps, and therefore what licenses `SEQUENCE_BATCH` to be larger than
PostgreSQL's `CACHE 1` default — a divergence in the *size* of the gaps, not in whether there are
any, and `CACHE n` is a sequence option PostgreSQL has with exactly this behaviour.

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

The harness — `config.yml` pointed at this node, the runner, the exclusion-list runner — is built
**in `/Users/chunlea/workspace/lab/esker-rails-harness/`** and stays there (§0). What lands in this
repository is `docs/bench/rails-scoreboard.md`: the Rails commit and the Esker commit, the raw pass
rate, the pass rate with conflict retry, one line per excluded test with its reason and the unit
that closes it, and the exact commands that reproduce the run. The first run's number is the
baseline whatever it is.

## 3. The test ladder

Each rung is a thing that either works or does not, and none of them is reached by asserting
about it in Rust:

1. **`pg` gem raw connect** — `PG.connect` and one `SELECT 1`, over a socket.
2. **`ActiveRecord::Base.establish_connection` + one migration** — this is where `pg_catalog`
   stops being optional.
3. **A scaffolded CRUD app** — create, read, update, destroy, through the real adapter.
4. **The full ActiveRecord suite**, with the exclusion list.

Rungs 1–3 are Ruby, so all four live in the out-of-tree harness (§0). What each rung contributes to
*this* repository is the SQL it made this node answer, captured as a corpus fixture — a rung is not
finished when it runs, it is finished when its statements are a Rust test.

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

## 6. The divergence table this phase adds

`docs/plans/phase-6a.md` §10a holds the whole surface diffed against PostgreSQL 19, and every row
below belongs beside those. They are **here** rather than there because `phase-6a.md` is another
lane's file while this phase is running; folding them in is a one-commit merge when the lanes
close, and a divergence recorded in the wrong file is better than one recorded nowhere.

| Divergence | Why | Where it is written down |
|---|---|---|
| `sum(int8)` is an `int8`, and overflowing it is `22003` | PostgreSQL's `sum(bigint)` is `numeric` and cannot overflow. For every input that does not overflow the two print **the same characters**, so the divergence a client can see is the `RowDescription` OID (20, not 1700) and the error on the inputs PostgreSQL absorbs. ADR 0031's rule: implement it where our type reproduces PostgreSQL's text, refuse it where it cannot. | [ADR 0031](../adr/0031-rails-compatibility-is-measured.md), `tests/aggregate_parity.rs`'s `TYPE_DIVERGENCES` |
| `avg` over an `int8` column is `0A000` | The other half of the same rule. PostgreSQL's `avg(bigint)` is `numeric` with sixteen fractional digits — `8.3333333333333333` — and the nearest `float8` is `8.333333333333334`: a different value in the last digit and a different type at the client, where `pg` maps `numeric` to `BigDecimal` and `float8` to `Float`. `avg(float8)` **is** implemented and is exact. First entry on the numeric backlog. | ADR 0031, `tests/slt/aggregate.slt` |
| Groups come back in `pg_cmp` order of their key | PostgreSQL promises **no order at all** without an `ORDER BY`, and returns its hash order — measured, it put the NULL group first. Ours is deterministic, which is a superset of what PostgreSQL guarantees, is what a byte-comparing harness needs, and — because `pg_cmp` puts NULL last — is the order `ORDER BY <key>` would have given anyway. The same choice `esker-columnar`'s evaluator made. | `crate::exec::aggregate`, `tests/aggregate_parity.rs`'s `DIVERGENCES` |
| A non-integer constant in `GROUP BY` groups rather than failing | PostgreSQL answers `42601 non-integer constant in GROUP BY`; here `GROUP BY 'x'` is an ordinary one-group key. Refusing it would mean a rule about literals that nothing else in this crate has, for a statement nobody writes on purpose. Divergence in the permissive direction, and recorded rather than fixed. | `tests/aggregate_parity.rs`'s `DIVERGENCES` |
| `EXPLAIN` prints `Aggregate` / `Group Aggregate` / `Unique` and no costs | The same divergence the access-path plans already carry: PostgreSQL chooses between `HashAggregate` and `GroupAggregate` and prints an estimate; there is one strategy here and no cost model, so the name says what it is rather than implying a choice that was not made. | `tests/slt/aggregate.slt`, `tests/slt/access_paths.slt` |

## 7. What unit 1 changed, and the two bugs it turned up

The aggregate itself is one plan node and one executor kind. Two things it touched were **already
wrong**, and both were found by writing the statement down rather than by a failure:

* **`ORDER BY <integer>` sorted by the constant.** A position in the target list is PostgreSQL's
  rule and this crate did not have it: `ORDER BY 1` lowered to the literal `1`, compared equal for
  every row, and left the input order — which *looks* right whenever the input order happens to
  agree, and silently ignores `DESC`. It is now a position, and out of range is `42P10` naming the
  clause and the number, as measured.
* **`EXPLAIN` printed the wrong column names above a node that reshapes the row.** Positions were
  always rendered against the *table's* columns, so the first output column of an aggregate printed
  as the table's first column — a name that is not merely unhelpful but wrong, and wrong in the
  direction a reader would believe. Each node now works out the row space it reads, and the one
  expression that is written against a node's *output* rather than its input — an `Aggregate`'s
  `HAVING` — says so where it is rendered.

## 8. Progress

| Unit | State | Commit |
|---|---|---|
| 0 — ADR, plan, aggregate capture | **done** | `de3c465`, `32fb58f` |
| 1 — aggregates | **done** | `1d78a96` |
| 2 — sequences and `RETURNING` | **done** | `e1b1bd2`, `7a7d4f3`, this commit |
| 3 — savepoints | in progress | |
| 3 — savepoints | not started | |
| 4 — joins | not started | |
| 5 — `pg_catalog` | not started | |
| 6 — the scoreboard | not started | |
