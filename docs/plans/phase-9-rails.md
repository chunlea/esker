# Phase 9 plan — a PostgreSQL that Rails can talk to, scored by Rails' own tests

Status: **units 0–4 landed** (unit 4 bar a second join); **unit 5 captured and re-ordered** — §2 says what the measurement changed. §9 records progress per unit, §6 the divergences, §7 what unit 1 changed and §8 what is watched.

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

Rails wraps each test in a transaction and rolls it back, and nests with savepoints. **Captured
first**, in one session, as `tests/corpus/pg19_savepoint.txt`: 48 statements, and five facts the
file exists for.

**The one that decides the shape.** `ROLLBACK TO SAVEPOINT` **un-aborts the block**. After an error
every statement is `25P02` until the end of the transaction — except that one, which recovers it
and lets the block go on and commit. Measured: a `23505`, then a `SELECT` that is `25P02`, then
`ROLLBACK TO s`, then an `INSERT` that works and a `COMMIT` that keeps both the pre-savepoint row
and the post-recovery one. That single fact is the whole of why Rails can run a test per
transaction — a failing assertion does not poison the rest of the block — and any design that
cannot recover an aborted block has not implemented savepoints at all.

Four more, each of which a plausible implementation gets wrong:

* a `ROLLBACK TO` **keeps** the savepoint, so the same one can be rolled back to twice;
* a `RELEASE` does not, and rolling back to a released savepoint is `3B001` — which itself aborts
  the block, so it is one of the ways *into* `25P02`;
* **names stack.** Two `SAVEPOINT dup` are two marks: `ROLLBACK TO dup` finds the most recent,
  `RELEASE dup` releases the most recent, and after that `ROLLBACK TO dup` finds the older one. A
  map from name to mark gets this wrong in a way no single-savepoint test can see;
* outside a block all three are `25P01` naming **their own verb** — `SAVEPOINT`,
  `RELEASE SAVEPOINT`, `ROLLBACK TO SAVEPOINT` — not the one the user typed.

#### The undo log, designed against the buffer rather than against the idea

Percolator has no nested transaction and `esker-client`'s `Transaction` does not expose its write
buffer — and `esker-client` is another lane's. So the honest design is not "truncate the buffer",
which this crate cannot do, but **a compensating undo log this crate keeps itself**:

* a **savepoint** pushes `(name, undo.len())` onto a stack;
* every `put` and `delete` made while the stack is non-empty first reads the key's **pre-image**
  through the same transaction — `txn.get(&key)` — and appends `(key, before)` to the undo log.
  The pre-image is what *this transaction* sees, which is exactly what restoring it has to put
  back;
* a **`ROLLBACK TO`** finds the topmost mark of that name, replays the undo log backwards to it
  (`put` the old value, or `delete` where there was none), truncates the log and the stack **above**
  the mark, and leaves the mark itself;
* a **`RELEASE`** pops the mark and everything above it and touches no data.

The cost is one read per write while a savepoint is open, and an undo log the size of what the
block wrote. Both are paid only inside a savepoint, which is the shape Rails uses and not the shape
a bulk load does. It needs **nothing from `esker-client`**, which is what makes it buildable in this
lane, and it is exact rather than approximate: the failure mode to avoid is a `ROLLBACK TO` that
leaves a write behind, and replaying pre-images cannot leave one.

Two things it must get right that the capture names: the aborted-block flag is **session state**
and `ROLLBACK TO` clears it, and the undo log has to be bounded the way the sort and the group
table are — a `53400` naming it beats an allocation on a client's behalf.

#### What building it turned up

**`ROLLBACK TO s` ended the whole block.** `sqlparser` puts `ROLLBACK` and `ROLLBACK TO` in one
variant with an `Option<Ident>`, and the classifier read the variant: so a statement PostgreSQL
accepts was answered with a `ROLLBACK` tag and the user's other work went with it, no error to say
so. It is now its own [`StatementClass`], and `tests/savepoint.rs` holds the regression — where the
assertion is the **keeping**, because a plain `ROLLBACK` would have discarded the earlier row too.

The corpus itself had to be re-taken. The first draft was two captures against two table states,
so its two halves contradicted each other and the replay failed on a line where *both* servers were
right about what they had been asked. It is now one session, and it **builds its own table**: a
fixture outside the file is a second thing that has to agree with it.

### Unit 4 — `INNER` and `LEFT JOIN`, `ON` and `USING` ✅

Nested loop only; correctness over speed, and a bench afterwards rather than a plan built for one.
The materialised inner side is bounded the way `Sort` is and answers `53400` naming it, which is
where the memory of a join actually is — the output streams.

**The trap the capture exists for**: the same predicate means different things in an `ON` and in a
`WHERE`. `LEFT JOIN r ON l.id = r.id AND r.flag` keeps every left row, NULL-extending the ones that
fail the condition; `... ON l.id = r.id WHERE r.flag` removes them. Three rows against one,
measured side by side. It falls out of *where* the extension happens: after the `ON` has been
applied to every pair, and before any `WHERE` above the node runs. An implementation that folded
the two together would answer the second for both and lose rows with no error.

**A left join may not swap which side drives the loop.** An inner join is commutative and the
planner is free to pick the side it can probe; a left join is not, because which side keeps its
unmatched rows is the whole of what it means. Driving the right side and NULL-extending answers a
`RIGHT JOIN` — the same rows, in the wrong places, with nothing to say so — so the swap is disabled
for it, and `tests/join.rs` pins that through `EXPLAIN` on a query where the other order *would*
have probed.

**`USING` is two things**, and carrying it only as the equality it implies would have got the
second wrong: it is `l.a = r.a` **and** a merge. `SELECT *` returns the column once and first,
ahead of either table's own; a bare reference to it is unambiguous where `ON l.a = r.a` makes it
`42702`. Its value is the **left** side's, with no `COALESCE` needed and none available — for an
inner join the two are equal by the condition and for a left join the right is either equal or
NULL, and there is no `RIGHT` or `FULL` join here to make a third case.

Two smaller things the capture decided. A `USING` column one side lacks names **which** side,
because a typo and a join between the wrong two tables look identical without it. And `ORDER BY id`
where more than one *output* column is called `id` is `42702 ORDER BY "id" is ambiguous` — a
different ambiguity from a column reference's, and the narrow half of PostgreSQL's rule: it also
prefers an output column to an input one, which is not implemented because nothing measured needs
it and an unmeasured preference would be invented.

**Not done: a second join.** `Select::join` is one `Option`, and a third table means a nested
`NestedLoop`, a three-table scope, and a probe boundary that is no longer "the last table". It is
its own unit's worth and is refused by name (`more than one JOIN`) until it is.

### Unit 5 — `pg_catalog` and `information_schema`, read-only

**Captured, and the capture says this unit is not next.** That is the finding, and it is the point
of having run the measurement before writing the code.

`tests/corpus/activerecord_8_1_statements.txt` is the 36 distinct statements ActiveRecord 8.1.3.1
sent a real PostgreSQL 19beta1 — read out of the **server's** log (`log_statement = 'all'` plus
`docker logs`), so what is recorded is what PostgreSQL received rather than what a client library
says it sent. `tests/activerecord_surface.rs` replays them and asserts the two contracts that apply
whatever the answer — C1, every one parses; C2, every refusal names its construct — and counts how
many run.

**Three of thirty-six.** The number was guessed at ten before it was run, which is the whole
argument for measuring: the guess was wrong by more than a factor of three, in the flattering
direction.

#### What actually blocks ActiveRecord, counted

| Blocker | Statements needing it | Is it the catalog? |
|---|---|---|
| **a table alias** (`FROM pg_type AS t`, `pg_class c`) | **22** | no — the query surface |
| a catalog function (`format_type`, `pg_get_expr`, `pg_get_indexdef`, `obj_description`, `current_schemas`, …) | 15 | half |
| an **array** and `= ANY` (`array_agg`, `ARRAY(…)`, `generate_subscripts`, `array_position`) | 13 | no — a type |
| **two or more joins** in one query | 9 | no — the query surface |
| a **subquery** (scalar, correlated, `ARRAY(SELECT …)`, derived table) | 4 | no — the query surface |
| a **cast** (`'…'::regclass`, `::regtype::oid`) | 4 | half |
| `SHOW search_path`, `SHOW max_identifier_length` | 2 | no — session |
| `character varying`, `integer`, `timestamp(6)` | 2 | no — the type surface |

The catalog's *content* — translating our `'m'` key space into `pg_class` and friends — is the last
thing on that list, not the first. A `pg_class` this node cannot alias, join twice, or take an
`array_agg` over is a table no ActiveRecord query can read.

#### The two that come before it, measured

**The migration does not run.** `t.string`, `t.integer` and `t.timestamps` compile to
`character varying`, `integer` and `timestamp(6)`, and this node has none of the three:

```sql
CREATE TABLE "harness_widgets" ("id" bigserial primary key,   -- runs (unit 2)
  "name" character varying NOT NULL,                          -- 0A000 the type CHARACTER VARYING
  "count" integer DEFAULT 0,                                  -- 0A000 the type INT
  "live" boolean,                                             -- runs
  "created_at" timestamp(6) NOT NULL, …)                      -- 0A000
```

So rung 2 of the ladder — establish a connection and run one migration — is blocked on the **type
surface** and not on the catalog at all. `bigserial` and `boolean` were the two that worked, which
is unit 2 and phase 6a paying off.

**Every catalog query is blocked on a table alias** before anything else in it matters. `FROM
pg_type AS t` is `0A000 a table alias is not supported` today; 22 of the 36 statements open that
way, including the very first one ActiveRecord sends.

#### What this re-orders

The unit as written — "translate our catalog into views" — is a phase, and it is the *last* phase of
several. What stands between here and a booting ActiveRecord, in the order the boot hits it:

1. **table aliases**, and with them the qualified-name resolution they change;
2. **`SHOW`** for the two GUCs a client reads at connect;
3. the **type surface**: `integer`, `character varying`, `timestamp` — which also decides whether
   `numeric` arrives at the same time (ADR 0031's backlog);
4. **multi-table joins** — the thing unit 4 deliberately left, now with a number on it: 9
   statements;
5. **subqueries** and **arrays**;
6. **casts** and the catalog **functions**;
7. and only then the catalog's content.

Each of those is a unit. The count in `activerecord_surface.rs` is what says whether one worked.

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

## 8. The watched list

Failures seen but not chased, instrumented so that a recurrence leaves an artifact rather than a
line in a scrollback. A third sighting makes it a chase.

| Seen | What | State |
|---|---|---|
| 2026-09-01, **twice** | `joint_gate`'s two transport calls have each missed their 30-second deadline once under a fully parallel `cargo test`: first `a_lock_the_ttl_kills_resolves_the_same_way_on_both_engines` on the `TxnKv` call, then `the_learner_answers_fragments_that_agree_with_a_row_scan` on the fragment call — `Timeout { no answer from 127.0.0.1:60224 in 30s }`. Both pass standalone. **Two different tests, one failure mode**, which narrows it: this is not the lock-expiry clock `1f22077` hardened, it is a 30-second RPC deadline against however many test binaries this machine is running at once. | **watched, one sighting from a chase.** Both calls now dump instead of unwrapping: `target/joint-gate-transport-<ts>.txt`, naming who led the region, every store's address, leadership and applied index, and the deadline. |

## 9. Progress

| Unit | State | Commit |
|---|---|---|
| 0 — ADR, plan, aggregate capture | **done** | `de3c465`, `32fb58f` |
| 1 — aggregates | **done** | `1d78a96` |
| 2 — sequences and `RETURNING` | **done** | `e1b1bd2`, `7a7d4f3`, `bf28e0d` |
| 3 — savepoints | **done** | `779ae2e` (capture), `a74b724` |
| 4 — joins | **`LEFT`, `ON`, `USING` done**; a second join is not | `56d23e2` |
| 5 — `pg_catalog` | **captured, and re-ordered by what it found**: 3 of ActiveRecord's 36 statements run, and the catalog is the *last* blocker rather than the first | this commit |
| 3 — savepoints | not started | |
| 4 — joins | not started | |
| 5 — `pg_catalog` | not started | |
| 6 — the scoreboard | not started | |
