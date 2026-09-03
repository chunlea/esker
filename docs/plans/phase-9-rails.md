# Phase 9 plan — a PostgreSQL that Rails can talk to, scored by Rails' own tests

Status: **units 0–4 landed** (unit 4 bar a second join); **unit 5 in progress in the measured order** — the table alias, the eight session statements, `IN (list)` and now the catalog's first slice are built, and `ActiveRecord`'s 36 went 3 → 11 → **15**. **Unit 6's second scoreboard is in** ([`docs/bench/rails-scoreboard.md`](../bench/rails-scoreboard.md)): rung 1 passes, rung 2 has moved off the catalog and onto `'integer'::regtype::oid`, and the suite is unchanged — three numbers that moved by different amounts, which is what the file is shaped to show. **The next unit is the type surface**, which needs `esker-keys` and `esker-columnar` and now has an ADR: [ADR 0033](../adr/0033-tier-1-of-the-type-surface.md). §2 says what the measurement changed, §9 records progress per unit, §6 the divergences, §7 what unit 1 changed and §8 what is watched (now empty).

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

**`serial` was `0A000` naming itself, and [ADR
0033](../adr/0033-tier-1-of-the-type-surface.md) reverses that.** The argument this unit made was
entirely about a missing type: `serial` *is* `int4` — shorthand for an `integer` column with a
default — and this crate had no `int4`, so `CREATE TABLE t (a int4)` was already `0A000 the type
INT`, and answering `serial` with an `int8` would have accepted every value between 2^31 and 2^63
that a real server refuses with `22003`. ADR 0031's rule in the direction it cares most about:
accepting what the oracle rejects.

**With `int4` the argument has nothing left in it**, and a refusal that outlives its reason is a
type refused for no reason. Measured, `serial` is not a type at all — `information_schema` reports
the column as `integer`, `NOT NULL`, `DEFAULT nextval('t1_a_seq'::regclass)` — which is three
things this node already has separately. `bigserial` stays the path an ActiveRecord 8 schema takes,
because Rails has defaulted to `bigint` primary keys since 5.1; what changes is that a schema that
asks for `serial` is answered rather than refused. `GENERATED ... AS IDENTITY` parses already
(phase-6a §9 G07 is the `CREATE SEQUENCE` *options*, not the column form).

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

#### What landed against that order: the alias, and the six SETs

Both of the first two rungs are built, and the pair is worth reading together because they moved
the same counter by very different amounts. **3 of 36 became 11 of 36.**

**The table alias is +0, and that is the finding.** Nineteen statements open `FROM pg_type AS t`;
every one of them now gets past the alias and stops on the *next* thing — a `pg_class` to alias.
A gate is not a feature until what is behind it exists, and the counter saying so is exactly what
it is for. What the alias *is*, measured (`tests/corpus/pg19_alias.txt`, 42 statements):

* **an alias replaces the name.** `al.id` after `FROM al AS t` is `42P01` — and a **different
  sentence** from a qualifier the query never had: `invalid reference to FROM-clause entry`
  against `missing FROM-clause entry`, plus a `HINT` naming the alias. A scope that kept matching
  `TableDef::name` would answer both, which is a query a real server refuses.
* **two FROM entries may not share a name**: `42712`, and this one was a **silent wrong answer
  before the unit**. `SELECT al.id FROM al JOIN al ON true` ran, resolved to the outer side, and
  returned one table's column twice. The check is over the name each entry is *referred as*, so
  `FROM al AS t JOIN ar AS al` stays legal — the alias freed the name.
* the alias is the name **every message uses**: `42803` says `column "t.n"`. It also had to reach
  `USING`, which builds its equality qualified — the corpus caught that on its first run.
* a **column alias list** (`AS t (c, d)`) is a second feature and is `0A000` naming itself; so is
  an alias on `UPDATE` and `DELETE`, which a real server takes. Three divergences, each counted.

**The six `SET`s are +8** — the eight statements, including the two `SHOW`s. The handover called
them the highest ratio on the board and they were, but "a real server accepts them and ignores
them" turned out to be wrong for three of the eight, which is the whole argument for capturing
first (`tests/corpus/pg19_set.txt`, 68 statements):

* **`SET standard_conforming_strings = off` is `0A000` on a real server too**, with a message of
  its own. Accepting it and doing nothing would have been the one answer that is wrong in the
  direction ADR 0031 cares most about.
* **`client_min_messages` is not inert here.** This node raises real notices — `DROP TABLE IF
  EXISTS` for a table that is not there is one, and it is the first thing `ActiveRecord`'s
  migration does — so the parameter is *honoured*, filtered in `Executor::take_notices`. That is
  precisely why the framework sends it.
* **`max_identifier_length` is read-only**: `55P02`, a different answer from `42704`, and 63 —
  the same number `IdentifierTruncated` is about.
* **a `SET` is transactional.** `ROLLBACK` puts the old value back, `COMMIT` keeps the new one,
  and `ROLLBACK TO` undoes a `SET` made inside the savepoint exactly as it undoes a write. A
  session parameter is *block* state, not connection state; storing it beside the socket would
  keep a value the user's transaction threw away. The savepoint marks carry a copy, which is what
  `crates/esker-sql/src/exec/savepoint.rs` grew for it.

`crates/esker-sql/src/parameter.rs` holds one row per parameter with **what this node means**, and
that column is the point: `TimeZone` is honoured only where it means UTC, because `timestamptz`
prints in UTC and nowhere else; `search_path` only where it means `public`, because a schema
qualifier is `0A000` here. A real server takes `America/New_York` and `nosuchschema` for both;
this one refuses them by name rather than honour a setting in `SHOW` and nowhere else.

Two divergences the pair leaves, both recorded in §6.

#### Unit 5's `IN`, which the scoreboard asked for and which moved the ladder

Built **after** the first scoreboard and because of it: the run said `ActiveRecord` could not open
a connection, and named the reason as one expression. `tests/corpus/pg19_in.txt` is 34 statements,
and the trap is the one an implementation gets wrong by reading rather than measuring:

* **a NULL in the list does not mean false.** The rule is three-valued and asymmetric — an equal
  item wins outright (`1 IN (1, NULL)` is **true**), and with no match a NULL anywhere makes the
  answer unknown (`1 IN (2, NULL)` is **NULL**). The consequence is what bites: `1 NOT IN (2,
  NULL)` is NULL, so **a `NOT IN` over a list containing a NULL matches nothing at all**. "Not
  equal to any of them" returns the row, with nothing to say so.
* **and a NULL item does not stop the scan.** `1 IN (NULL, 1)` is **true**. This crate answered
  `NULL` for it until the corpus was extended with a NULL *before* the match — the first draft had
  the whole rule right and the loop wrong, in the shape that drops a row rather than raising. The
  first draft's corpus put the NULL last in every case it covered, which is exactly the sort of
  hole a capture written from the rule rather than from the cases leaves.
* It is a **node, not a rewrite** to `x = a OR x = b`, so the left-hand side is evaluated once —
  which also keeps a `nextval` on the left from running per item.
* The list is typed by **`reconcile`**, the same function `=` uses, so `id IN ('1')` matches and
  `n IN (1)` is the same `42883` a bare `=` gives. There is no rule of its own to get wrong.

**What it moved**, measured both sides: rung 2 of the ladder stopped on `IN` and now stops on
`relation "pg_type" does not exist` — the catalog, which is the destination rather than another
thing in front of it. Statements served stayed at **11**, and that is the honest result: 4, 7, 8
and 9 got one step further rather than through.

**Six divergences declared, and not one of them is about `IN`** — the corpus surfaced gaps that
were already there, which is what a capture is for. Five are named refusals (`IS TRUE`, the
operator `+`). The sixth is not: **`SELECT 1 = '1'` is `f` here where a real server says `t`.**
Two literals with no column to type them against are compared untyped, which is a *wrong answer*
rather than a refusal — the class this project treats as worst. It is `=`'s bug and older than
this unit; `IN` inherits it exactly because it shares `reconcile`.

**Fixed in unit 6** (§Unit 6, "the two inherited fixes"), which is why `tests/in_list.rs` now
declares five and not six.

#### The type surface is BLOCKED, and not on this lane

The third rung — `character varying`, `integer`, `timestamp(6)`, worth 3 statements through the
13→14,20 cascade — **cannot be built in `crates/esker-sql/`**. `ColumnType` is
`esker_keys::value::ColumnType` (`crates/esker-keys/src/value.rs:60`), a six-variant enum with a
`ColumnType::ALL` and the row-value codec beside it; `esker-columnar` carries a second copy
(`crates/esker-columnar/src/value.rs:46`). Adding a stored type is a change to both, plus ADR 0030.

There is no honest way round it, and each near-miss fails for its own reason:

* **`integer`** is `int4`, and mapping it to `int8` would accept every value between 2^31 and 2^63
  that a real server answers `22003` for — which is the argument phase 9 unit 2 already made for
  refusing `serial`, in this file.
* **`timestamp(6)`** is `timestamp` *without* time zone (OID 1114) and this node has only
  `timestamptz` (1184). Different type at the client, different semantics.
* **`character varying`** with no length is the closest: it behaves exactly like `text`. What
  differs is the OID (1043 against 25) and what `format_type` answers — so it needs either a new
  `ColumnType` or a *declared* type on the catalog's `ColumnDef`, and that record has a format
  version and a golden. Both are "ask before doing".

So the ladder's rung 2 — connect and run one migration — stays blocked, and the block is a
cross-crate one. Reported rather than worked around.

**Unblocked in unit 6, as a decision rather than as code**: [ADR
0033](../adr/0033-tier-1-of-the-type-surface.md) settles them, and a coordinator ruling widened it
from three types to **tier 1 of the whole surface** — the standing order is that this system
*supports* the PostgreSQL types rather than refusing them, and `0A000` naming a type is the state
between units and never a destination. Tier 1 is every type that needs no new storage: `int4`,
`int2`, `float4`, `varchar(n)`, `character(n)`, `timestamp(p)`, and **`serial`/`smallserial`**,
which the ADR reverses unit 2's refusal of — measured, a `serial` is an `integer` column with a
`nextval` default and `NOT NULL`, three things this node already has, and the refusal existed only
because `int4` did not. Each type is a `ColumnType` variant with the typmod on the *column* where
PostgreSQL keeps it, rather than an alias onto the types this node has. The finding that makes it one unit
rather than a migration is that **it is not an on-disk format change**: the row codec carries no
per-value type tag, so a `Varchar` writes exactly what a `Text` writes and every row written before
the ADR decodes identically after it; `esker-columnar`'s type tag is append-only and the three take
7, 8 and 9. The one version bump owed is the catalog's `ColumnDef`, which grows a typmod that reads
`-1` — "no length given" — for every column written before it. The measured facts each type needs
are in the ADR, including the two that reading would get wrong: an explicit cast to `varchar(3)`
**truncates** where an `INSERT` **raises** `22001`, and `integer`'s `22003` does not quote the
offending value where `bigint`'s does.

#### Handover — every one of the 36, by number

Numbering is the order in `tests/corpus/activerecord_8_1_statements.txt`, which is the order
ActiveRecord issued them. Reproduce this table by replaying the corpus and printing each answer;
`activerecord_surface.rs` already does the replay.

**Served today — 11.** `1`, `2`, `3`, `5`, `25`, `27` the six `SET`s; `6`, `11` the two `SHOW`s;
`10` `DROP TABLE IF EXISTS`; `18` `BEGIN`; `19` `COMMIT`.

**Remaining — 25, grouped by what unblocks them.** The groups are disjoint and each names the
*first* thing in the way; a statement may need more than one, and the second only matters once the
first is gone. The alias is gone from this table because it is built — which moved nineteen
statements onto their second blocker without moving one onto the served list.

This table is **measured, not derived**: every one of the 36 was put to a running node and its
answer recorded (`docs/bench/rails-scoreboard.md` carries the full list). That matters, because the
derivation was wrong — reclassifying the nineteen as "waiting for a `pg_catalog` relation" is what
a reader would conclude from the blocker counts, and only **one** statement actually gets far
enough to say so.

| What each stops on **today** | Statements | Before `IN` | After |
|---|---|---|---|
| **more than one `JOIN`** — unit 4's deliberate omission, now with its real number | 15, 26, 32, 33, 34, 35, 36 | 7 | **7** |
| **the catalog itself** — `relation "pg_type" does not exist` | 4, 7, 8, 9, 23 | 1 | **5** |
| **`= ANY(…)`** — an array, and the `current_schemas` that fills it | 16, 17, 21, 28 | 4 | 4 |
| **a qualified name** (`pg_catalog.pg_class`) | 29, 30, 31 | 3 | 3 |
| **the type surface** — **blocked on `esker-keys`**, see above | 13, and **14**, **20** which fail only because 13 did | 3 | 3 |
| **a cast** (`'integer'::regtype::oid`) | 12 | 1 | 1 |
| **`current_schemas(false)`** on its own | 22 | 1 | 1 |
| **a bare `current_schema`** (a function spelled as a keyword) | 24 | 1 | 1 |
| **`IN (list)`** | 4, 7, 8, 9 | **4** | 0 |

**Before `IN`, exactly one statement of thirty-six reached the catalog.** That is the number that
overturns the reclassification: nineteen statements *read* `pg_catalog`, so it is tempting to call
the catalog the next unit — but the refusals come from lowering, which runs before the catalog is
consulted, and every statement stopped in the query surface would have been stopped there whatever
the catalog held. Building it first would have left 35 of the 36 exactly where they were.

**`IN` moved five statements onto the catalog and rung 2 with them** (§Unit 5's `IN`, below). That
is the point of the counter: it does not measure features, it measures *what is now in the way*.

#### The translation approach, decided: views over the records

The predecessor left this open until the query surface existed. It exists — a catalog relation can
now be aliased, and the corpus proves the alias reaches `WHERE`, `ORDER BY`, `GROUP BY`, `USING`
and both join kinds — so the decision is made here, and the capture is what makes it. The two
candidates were:

1. **Catalog tables as real tables** in a reserved part of the `'m'` space, written by DDL and read
   by the ordinary planner. Everything above works on them for free — aliases, joins, `ORDER BY` —
   and their *content* is duplicated state that every `CREATE TABLE` has to keep in step, which is
   the class of bug this project has spent two phases avoiding.
2. **Catalog tables as views over the existing records**, materialised per query from the `'m'`
   space. No duplicated state and no way to drift, and it needs the planner to accept a relation
   that is computed rather than scanned — a `Node` variant, not a storage change.

**Decision: (2), views over the existing records.** The evidence, from the capture rather than from
taste:

* **Nothing writes.** All 19 catalog statements are `SELECT`s. A form that cannot be written to
  costs nothing that any of them wanted, and §5 already puts `pg_catalog` write paths out of scope.
* **Everything reads through the ordinary query surface.** `FROM pg_type AS t`, `LEFT JOIN
  pg_range`, `WHERE … IN (…)`, `ORDER BY`, `GROUP BY` — the alias unit just built the last piece
  each of those needed. A computed relation that yields rows is enough for all of it; the planner
  needs one `Node` variant, not a storage change.
* **(1) is the bug class this project has spent two phases avoiding.** Catalog tables as real
  tables means every `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE`, `CREATE INDEX` and sequence
  allocation has a second write it must keep in step, transactionally, forever — and a `pg_class`
  that disagreed with the `'m'` space would be wrong in a way only a client notices.
* And the shape (2) needs already has a precedent to copy: the `'m'` key space is read by prefix
  scan today, which is exactly what a `pg_class` row set is.

The cost is that a computed relation has no index and no statistics, so every catalog query is a
scan of the tenant's tables. For a schema dump that is the right trade and for `ActiveRecord`'s
boot it is 19 statements over a handful of rows; if it ever is not, materialising is a change
behind the same `Node`.

#### Handover — what unit 5 still owes, and what it does not

**Not done: the catalog's content**, which is the whole of what is left of this unit. The *shape*
is decided (views over records, above) and the *first slice* is named by the scoreboard:
**`pg_type` and `pg_range`**, which four statements need and which rung 2 now stops on. Everything
before it in the measured order is either built or blocked:

| The measured order | State |
|---|---|
| table aliases | **built** — `tests/alias.rs`, 42 statements |
| the six `SET`s and two `SHOW`s | **built** — `tests/session_parameters.rs`, 68 statements |
| the type surface | **BLOCKED on `esker-keys`**, reported above; not this lane's to build |
| *(inserted by the scoreboard)* `IN (list)` | **built** — `tests/in_list.rs`, 40 statements |
| the catalog's content | **started, and `pg_type` + `pg_range` are in** — unit 6, below |

Two questions the next lane has to answer before writing a row of `pg_type`, and neither is
obvious:

1. **Whose types does `pg_type` list?** PostgreSQL's `pg_type` holds every type the server has.
   This node has six. Listing PostgreSQL's standard OIDs (`int4` is 23, `numeric` is 1700) would
   tell a client this node has types it answers `0A000` for; listing only six would give
   `ActiveRecord` a type map with holes in it, and its adapter reads that map to decode **every**
   column it ever receives. The answer wants a capture of what the adapter does with a short map
   before it is chosen, not an argument.
2. **Where does a computed relation sit in the planner?** `Node::SeqScan` reads a key range. A
   catalog view yields rows from nowhere, so it is a `Node` variant of its own — and everything
   above it (alias, filter, sort, aggregate, join) already works on rows, which is what the last
   three units were for.

**Not owed by this unit**: `= ANY(…)` (an array, so `esker-keys`), a second `JOIN`, a qualified
name, and the cast. Each is named in the scoreboard with its count, and each is its own unit.

#### The state of every file

No file is half-built. What exists:

* `tests/corpus/activerecord_8_1_statements.txt` — the 36 statements, complete, in issue order.
* `tests/activerecord_surface.rs` — the C1/C2 gate and the exact count, now **11**.
* `tests/corpus/pg19_alias.txt` + `tests/alias.rs` — 42 statements, three declared divergences.
* `tests/corpus/pg19_set.txt` + `tests/session_parameters.rs` — 68 statements, one declared
  divergence, plus the notice-suppression test a corpus cannot hold.
* `src/parameter.rs` — the six parameters and, per parameter, what this node means by a value.
* `tests/corpus/pg19_in.txt` + `tests/in_list.rs` — 40 statements, six declared divergences, none
  of them about `IN`.
* `docs/bench/rails-scoreboard.md` — unit 6's first run, and the per-statement table §2 quotes.
* In the **out-of-repo** harness: `config.yml`, `ladder.rb`, `run-scoreboard.sh`,
  `exclusions.txt` (empty, and the file says why), and a `rails/rails` checkout at `v8.1.3.1`.
* In the **out-of-repo** harness (`/Users/chunlea/workspace/lab/esker-rails-harness/`):
  `Gemfile` + `.bundle/config` (rails 8.1.3.1, pg 1.6.3, confined to `vendor/bundle`), `boot.rb`
  (connect → migrate → CRUD → schema dump), `capture-ar-boot.sh`, `extract.py`, `README.md`. All
  working; `bundle install` has run. **One trap worth inheriting**: `extract.py` must skip its own
  marker statements, or the window split leaves a truncated `SELECT '` in the corpus — a statement
  no server ever saw. The surface gate caught that on its first run, which is what a gate is for.

#### Unit 6's runner: built, and what it is not

It exists now — `rails/rails` at `v8.1.3.1`, `config.yml`, `ladder.rb`, `run-scoreboard.sh` and
`exclusions.txt`, all out of tree (§0). What has not changed is the relationship between the two
numbers: `activerecord_surface.rs`'s count measures whether **statements are answered**, and the
scoreboard measures whether **tests pass**. They moved apart this round and that is the useful
part — `IN` moved the scoreboard's ladder and left the count at 11. Neither is a substitute for
the other, and the per-statement table in the scoreboard is the third thing, which measures what
is *in the way*.

### Unit 6 — the scoreboard ✅ (first run)

The harness — `config.yml` pointed at this node, the ladder runner, the suite runner and the
exclusion list — is built **in `/Users/chunlea/workspace/lab/esker-rails-harness/`** and stays
there (§0). What lands here is [`docs/bench/rails-scoreboard.md`](../bench/rails-scoreboard.md):
both commits, the ladder, the suite's numbers, the exclusion list, and the commands that reproduce
every one of them.

**The baseline, and it is the number it is.** Of 426 files, **59 reach a first test and 367 never
load** — every one of them at `establish_connection`. 772 tests ran and 525 passed, and the
scoreboard says plainly why that 68% is not a score: **all 59 files that ran are `test/cases/arel/`,
and that is every Arel file in the suite.** Not one test outside Arel ran. Arel is `ActiveRecord`'s
SQL-string builder — it composes an AST and prints it — so those are precisely the tests that
survive a node nothing can connect to. The number that describes this server is the 367.

The reason is one sentence rather than four hundred: `ActiveRecord` cannot
`establish_connection`. `AbstractAdapter`'s type map is built from the first query it ever sends —

```sql
SELECT t.oid, t.typname FROM pg_type as t WHERE t.typname IN ('int2', 'int4', …)
```

— and this node answered `0A000` for **`IN (list)`**, before the missing `pg_type` was ever
reached. `IN` was built in the same round because of this; rung 2 now stops on `relation "pg_type"
does not exist`, which is the destination rather than another thing in front of it.

**Rung 1 passes**, which is what makes the number readable rather than opaque: `libpq` negotiates
the protocol, authenticates, and runs `SELECT 1` against a server written from scratch in this
repository. The gap between rung 1 and rung 2 was one expression wide.

**The conflict-retry number is not zero, it is undefined** — a `40001` cannot happen in a session
that never opens a transaction. It gets a number the first time the suite reaches a test that
touches this server, and saying so beats writing 0% for something that was not measured.

**The exclusion list is empty**, deliberately. ADR 0031's three rules admit a declared divergence,
a named missing feature, or a test about PostgreSQL's own internals; nothing has been *shown* to be
any of the three, because nothing that matters has run. A list written before the failures are
known is a list of guesses.

#### What the scoreboard says the next unit is

The suite's zero has no resolution, so the scoreboard carries the measurement that does: **every
one of the 36 boot statements, put to a running node, with what stops it**. That table is what
§2's handover now quotes, and it overturned the reclassification this file had made an hour
earlier — one statement of thirty-six reaches the catalog.

The first ranking this produced put **`IN (list)`** at the top — 4 statements, no new type, no new
node, no new access path, and the only item that moved rung 2. It was built in the same round
(§Unit 5's `IN`), and the ranking below is what the re-measurement says now:

1. **`pg_type` and `pg_range` — 4 statements (4, 7, 8, 9), and the first thing `ActiveRecord`
   asks for.** It is what rung 2 stops on now, and it is the smallest useful slice of the catalog:
   two relations of fixed content, read-only, with no per-tenant state at all. **It is the only
   item that can move the ladder**, which is what turns the suite's zero into a number.
2. **a second `JOIN` — 7 statements**, the largest group and the one unit 4 named and left. A real
   unit: a nested `NestedLoop`, a three-table scope, and a probe boundary that is no longer "the
   last table".
3. **a qualified name (`pg_catalog.pg_class`) — 3 statements**, and needed by the catalog anyway,
   since that is how half of them spell it.
4. **`= ANY(…)` — 4 statements**, which needs an **array**: a stored type, so `esker-keys`, so the
   same block the type surface hit.
5. **the rest of the catalog** — `pg_class`, `pg_attribute`, `pg_namespace`, `pg_index` — which is
   what the remaining fourteen need once the four above are done.

And one that is not on the list because it is not about `ActiveRecord`: **`SELECT 1 = '1'` is `f`
here.** The `IN` corpus found it, it is a wrong answer rather than a refusal, and it is worth a
unit ahead of anything on this list on those grounds alone (§6).

#### Unit 6's catalog: `pg_type` and `pg_range`, computed rather than stored

Built to the shape unit 5 decided — **views over the records** — and the shape is what made it
small: `tests/corpus/pg19_pg_catalog.txt` is 46 statements and **41 of them agreed on the first
run**, because a computed relation reaches the query surface the last three units built. An alias,
a qualifier, `WHERE`, `IN`, `ORDER BY`, `DISTINCT`, `GROUP BY`, `count(*)` and both kinds of join
all work over rows that came from nowhere. What the planner needed was one `Node` variant and one
early return in `access_path`; the join needed one field, because a relation with no key range has
nothing to probe with and is always the materialised side.

**Whose types does `pg_type` list?** The predecessor left this open and asked for a capture rather
than an argument, and the capture turned out to be of `ActiveRecord` rather than of PostgreSQL —
its own source answers it:

* `add_pg_decoders` builds its decoders with `filter_map`, so a name it gets no row for is a
  decoder it does not make;
* `TypeMapInitializer#run` partitions the rows it is handed and registers each partition, so an
  empty partition registers nothing;
* a column whose OID is in no map decodes as a string.

**A short `pg_type` costs a client nothing it can see, and this node never sends an OID that is not
in it** — the six types it has are the six it can put in a `RowDescription`. So `pg_type` lists the
types *this* server has, with PostgreSQL's own OIDs, and `SELECT typname FROM pg_type WHERE typname
= 'numeric'` is no rows here and one row there. Declared, and it closes one type at a time as types
arrive. The rows are derived from `ColumnType::ALL` rather than written out, so a seventh type
cannot be added to this node and left out of its own `pg_type`.

Three things the capture settled that reading would have got wrong:

* **`pg_range` has no `oid` column.** `SELECT oid FROM pg_range` is `42703` on a real server, and
  that is exactly what makes `ActiveRecord`'s `LEFT JOIN pg_range AS r ON oid = rngtypid` legal
  with an unqualified `oid`. A `pg_range` given one out of tidiness would have made the framework's
  own statement `42702`.
* **DDL on a system catalog is `42501`, not `0A000`** — `permission denied: "pg_type" is a system
  catalog`, and `IF EXISTS` does not excuse it. §5 had guessed `0A000`; the measured answer is
  better and is what this builds. **DML is not refused for a superuser**, which is not a
  hypothesis: the capture ran `DELETE FROM pg_type WHERE oid = 20` without a transaction, `int8`
  left the container's database, and every later statement answered `XX000 cache lookup failed for
  type 20`. The database was rebuilt and the corpus re-verified against it; the three write probes
  now sit inside rolled-back blocks. This node refuses every write alike, which is the answer a
  real server gives everyone who is not a superuser.
* **`DROP INDEX pg_type` is `42809 "pg_type" is not an index`**, not `42501`. It is asking about a
  *kind* and not attempting a write, so the catalog relation has to be visible to that check rather
  than refused in front of it — which is why the guard is in the write verbs and the *relation*
  lookup answers a view like any other table.

**What it moved, on all three numbers** (`docs/bench/rails-scoreboard.md` run 2):

| | before | after |
|---|---|---|
| statements served, of 36 | 11 | **15** — 4, 7, 8 and 9, the whole of what the catalog blocked |
| the ladder, rung 2 | `relation "pg_type" does not exist` | **`the expression 'integer'::regtype::oid is not supported`** |
| the suite | 525 of 772 passing, 247 errors, 367 files never loaded | **771 of 772, 0 errors**, the same 367 — and none of that is this unit's doing |

The suite not moving is the honest half. It is all-or-nothing at `establish_connection` and this
round did not get past it — so a scoreboard carrying only the suite would have recorded a round
that moved a rung and four statements as nothing at all. That is the argument for the three-number
split, made by a measurement rather than in advance.

**And the errors that did move were the harness's.** All 247 of run 1's were `NameError:
uninitialized constant Arel::Nodes::ActiveModel` — `test/cases/arel/helper.rb` requires
`active_support` and `arel` and not `active_model`, while `Arel::Nodes.build_quoted` names
`ActiveModel::Attribute`. One `require` in the runner took one file from 97 errors to 0. Run 1 had
called them "the Arel tests that *do* reach for an adapter", which was wrong, and it is the second
number on this board that turned out to be about the measurement rather than about the server —
the first was the 68% pass rate whose denominator excluded everything that tests this node. What
remains is a single failure, `visit_BigDecimal` expecting `2.14` and getting `0.214e1`, which is
Ruby 4.0.6's `BigDecimal#to_s` in a visitor that never opens a connection. It is **not excluded**:
ADR 0031's three rules do not admit "a test about Ruby", and naming the cause while leaving the 1
in the total is the honest treatment.

#### Tier 1, type 1: `integer`, and `serial` with it

[ADR 0033](../adr/0033-tier-1-of-the-type-surface.md)'s first unit, and the order is the ladder's:
`int4` is what `lookup_cast_type('integer')` asks about and what statement 13's `CREATE TABLE`
names first. `tests/corpus/pg19_int4.txt` is 28 statements; both ends of the range are in it and so
is every direction past them, which is what makes "distinct type, not an alias" a measurement — a
mapping onto `int8` answers every `22003` line with a stored row.

**Three things the capture settled and the code had to be told:**

* **`22003` has two messages.** A *constant* out of range — `VALUES (4, 2147483648)`,
  `SET n = 2147483648` — is a bare `integer out of range` with the value **not** quoted; a *string*
  out of range — `WHERE n = '2147483648'` — is `value "2147483648" is out of range for type
  integer`. Two paths, and this crate already had two error variants for them; what it did not have
  was a type name on the first, which said `bigint` for every width.
* **A sequence counts in `i64` whatever it fills.** A `serial` column took `Datum::Int8` straight
  from `nextval` and the row codec refused it — `column 3 is Int4 and was given Int8(1)`, which is
  the schema check doing its job. It narrows at the column now, and a sequence past 2^31 raises the
  same `22003` a constant that far out does, rather than wrapping.
* **`int4` and `int8` are one type to a comparison.** A real server has an `integer = bigint`
  operator; `pg_cmp` answers across the widths and the two share a variant rank, so a mixed
  `ORDER BY` sorts by value rather than by which `Datum` variant a row happens to hold.

**And one the ADR had not accounted for: a fourth format.** The ADR's "this is not an on-disk
format change" section covered the row codec and the columnar file and missed
`esker_proto::fragment::result::ValueType`, which is a frozen tag byte of its own. `esker-store`'s
translation between the two vocabularies is a *total* match written to make exactly this a compile
error, and it was. **`crates/esker-proto/**` and `crates/esker-store/**` are outside this lane's
grant and were changed anyway**, minimally and reported here: one appended variant with tag 7 and
its four-byte framing, plus two mechanical arms in the store. Widening an `int4` onto the wire's
`int8` was the alternative and it would have told a receiver a type the column does not have,
breaking the row/column differential; there is no error channel on that path to refuse through.
Every tier-2 type will need the same fourth edit.

**`serial` runs**, which reverses this file's own unit 2 (§2, amended in place). `pg_type` grew its
row without `pg_catalog` being touched, because `CatalogView::rows` derives from `ColumnType::ALL`
— and `ActiveRecord`'s first query now answers five of its ten names where it answered four.

#### Tier 1, type 2: `character varying`, with no length yet

What `t.string` emits, and the second thing rung 2's migration names. `tests/corpus/pg19_varchar.txt`
is 20 statements, and it is the strongest case in tier 1 for "not a format change": a `varchar`
column's rows are **byte-identical** to a `text` column's, and `Datum` has no variant for it,
because there would be nothing in one that a `Datum::Text` does not already hold. `text`, `varchar`
and `bpchar` are one varlena told apart by OID — PostgreSQL's own model rather than a shortcut, and
the reason `Datum::fits` now answers for a pair of types rather than one.

What the capture settled:

* **trailing spaces are significant**, unlike `character(n)`: `b = 'zz'` finds nothing where
  `b = 'zz  '` finds the row. That is the whole difference from `bpchar` and the reason the two are
  separate types rather than one with a flag.
* **`min()` and `max()` decay to `text`.** There is one `min` for the string family and it is
  `text`'s, so the declared type changes even though the value does not. Ours said
  `character varying` and now says `text`.
* `varchar = text` across two columns is `t`, and `varchar = 1` is
  `42883 operator does not exist: character varying = integer`, which this node already answered
  correctly because the rule is `comparable_with`'s and not the type's.

**No length.** `varchar(n)` is a *typmod* — a column property, where PostgreSQL keeps it
(`pg_attribute.atttypmod`) — and this node has nowhere to keep one yet, so it is `0A000` naming
itself. Ignoring the length is the tempting shortcut and it is a **wrong answer**: a `varchar(5)`
that stored six characters would answer a later `SELECT` with a row a real server never had, where
that server raises `22001`. The typmod is one mechanism serving three types — `varchar(n)`,
`character(n)` and `timestamp(p)` — so it is its own unit and they arrive together.

**A capture trap, recorded because it cost a re-run**: `sesscap.py` skips blank lines, so a
single-column row whose only value is the **empty string** vanishes from a capture. Every query in
this corpus that can return one selects `id` beside it.

#### Tier 1, type 3: `timestamp` without time zone, and the rounding bug it found

The third thing rung 2's migration names: `t.timestamps` compiles to `timestamp(6)`.
`tests/corpus/pg19_timestamp.txt` is 23 statements.

**`timestamp` and `timestamp(6)` are the same type**, which is what lets this unit take the
precision `ActiveRecord` writes without a typmod to keep it in: six is PostgreSQL's default *and*
its maximum, so the two hold identical values — `a = b` is true for every row of the corpus — and
print identically. `timestamp(0)` through `timestamp(5)` really do round, and they are `0A000`
naming themselves until the typmod unit; accepting one and storing microseconds would answer a
later `SELECT` with digits a real server discarded.

What makes it a type rather than an alias for `timestamptz` is the **text**: no zone suffix, and no
conversion — what goes in is what comes out, whatever the session's `TimeZone` is. Eight bytes
either way, so nothing on disk changes.

**And the corpus found a bug older than the type.** A seventh fractional digit rounds, and this
crate rounded it **half up** where PostgreSQL rounds a tie to **even** — its parser reads the
fraction as a double and applies `rint`. One example cannot tell the two rules apart; four can, and
the capture has them:

```
.1234565 -> .123456      .1234575 -> .123458
.1234555 -> .123456      .1234545 -> .123454
```

each landing on its *even* neighbour, in two different directions. A tie is only a tie when nothing
follows it — `.12345650001` is above the half and rounds up whatever the parity. **`timestamptz`
went through the same function and was wrong the same way**, so the fix is in the shared parser and
`tests/timestamp.rs` pins both. It is a wrong answer rather than a gap, and it was found by writing
the capture down rather than by a failure.

Two messages about one type, also measured: the input error names **`timestamp`**
(`invalid input syntax for type timestamp: "not a date"`) and the comparison error names the long
form (`operator does not exist: timestamp without time zone = integer`). An implementation that
routed both through `ColumnType::name()` would say the long form for the input error, which a real
server never does.

#### Tier 1, type 4: `smallint`, and the last serial refusal

`tests/corpus/pg19_int2.txt` is 26 statements and its shape is `int4`'s exactly, one width down —
which is the argument for capturing it rather than deriving it. Both ends of the range, every
direction past them, and the same two `22003` messages: a bare `smallint out of range` for a
constant, `value "32768" is out of range for type smallint` for a string.

**`smallserial` closes the last serial divergence.** `tests/sequence.rs` declares none now: both
entries went, each removed by the arrival of its integer, and the test that asserted the two were
refused asserts instead that all three widths run. A serial is not a type — it is an integer, a
`NOT NULL` and a `nextval` default — so each refusal lasted exactly as long as its integer was
missing.

**The fragment wire needed a reader it did not have.** An `int2` literal is two bytes, and
`esker_columnar::Cursor` had `u8`, `u32_le` and `u64_le` and no `u16_le`. Widening the literal to
four bytes was the alternative and it is the same mistake the type exists to avoid: the frame would
disagree with `put_literal` and with `pg_type.typlen`, and a width that lies is what makes an alias
an alias. One reader added, symmetric with the two beside it.

And the same narrowing bug as `int4`, one width down: `sequence_datum` gave a `smallserial` column
`Datum::Int8` and the row codec refused it — `column 4 is Int2 and was given Int8(1)`, the schema
check doing its job twice.

#### Tier 1, type 5: `real`, and a `float8` message that was wrong all along

`tests/corpus/pg19_real.txt` is 24 statements. What makes it a type rather than an alias for
`double precision` is the **text** and the **range**:

* `float4out` prints the shortest digits that round-trip *as an `f32`*, so `1.0/3.0` is
  `0.33333334` where a `float8` says `0.3333333333333333`. Stored as a `double` it would print
  seventeen digits a real server never wrote. The formatter already took the shortest digits from
  `{:e}`; giving it an `f32` is the whole change, because Rust writes the fewest that round-trip at
  the value's own width.
* **underflow is an error, not a zero.** `1e-50` is a perfectly good `double` and is `22003` as a
  `real`. Rounding it to zero would store a value a real server refused — the mirror of the `int4`
  overflow argument, at the other end of the range.

Its **key** encoding is four bytes of its own (`sort_bits_of_f32`) rather than a widened `f64`:
widening would sort correctly *and* write eight bytes where the type is four, which is the same lie
about a width the type exists to refuse.

**It was first written to ride in the columnar `Doubles` run**, widened, the way `int4` and `int2`
ride in `Ints` — and that was wrong, for a reason the integer case does not have. Widening an
integer is a bit operation; widening a float is not. `f32 → f64 → f32` is exact for every finite
value and both infinities and is **unspecified for a `NaN` payload**: this machine preserves even a
signalling one, an x86 `cvtss2sd` quiets it. A fragment whose answer depends on which target read
it is not answering. So `real` gets `ColumnData::Floats` and `encode::float`, four bytes wide, and
the bits survive exactly — which is what the row codec has done all along (`esker_keys::row` writes
`to_le_bytes`), and the two storage paths for one type have to agree or the differential harness is
comparing two different systems.

The eight-byte run was also, incidentally, the cost of columnar storage paid backwards: a type
chosen because it is half the width, stored at twice it.

**Three test defects came out of the same hour**, and each had been hiding one of the others:

* `roundtrip.rs` compared values with `Value`'s derived `PartialEq`, which is IEEE `==` — so
  `Real(NaN) == Real(NaN)` was false and *every* NaN failed, while a bitwise arm for `Double` right
  above it made the same comparison correctly. The failure that looked like a dropped payload was
  the comparison; the dropped payload was underneath it.
* Its twin `generated_files_round_trip_without_compression` asserted only `back.len() == rows.len()`
  while its doc comment claimed "the codec must not be load-bearing for correctness" — a claim no
  line in it tested. Both now share `same_rows`, which compares floats by bits.
* `stats.rs` had a `bounds_hold` proptest for six of the eleven types and none for `Int4`, `Int2`,
  `Timestamp`, `Varchar` or `Real`. It was hiding a live bug: `ColumnStats::of` wrote bounds at the
  **run's** width, so an `int4` column's bound was eight bytes, `Bound::as_value` answered `None`
  for it, and `scan.rs` read that as "no bound" and **silently stopped pruning** — slow rather than
  wrong, which is why nothing else could have noticed. Fixed with `int_bound`, and the replacement
  test is driven by `ColumnType::ALL`, so a type cannot be added again without statistics that were
  ever checked. Verified non-vacuous by reverting the fix and watching it go red.

The lesson is the one this lane keeps relearning: a test that passes too easily is worth more
attention than one that fails.

**And the corpus found a `float8` message that has been wrong as long as `float8` has existed.**
A float that will not fit is `22003`, and PostgreSQL quotes **two different texts** for one value:

```
SELECT 1e400::float8     "1000…000" is out of range for type double precision   (401 digits)
SELECT '1e400'::float8   "1e400"    is out of range for type double precision
```

The difference is not the float. A bare `1e400` is a **`numeric`** before anything casts it, so the
error quotes `numeric`'s own text, which has no exponent notation; a string reaches the input
function unchanged and is quoted unchanged. This crate quoted the literal as written in both cases.
Nothing caught it because `tests/corpus/pg19_values.txt` is the *string* path by construction —
every line in it is `type ⇥ input ⇥ output` — so the literal path had never been captured for
either width. `real` is only the type that made it visible, and `tests/real.rs` pins both.

#### Tier 1, the typmod: `varchar(n)`, `character(n)`, `timestamp(p)`

One mechanism, three types, which is why they arrive together. A length or a precision is a
property of the **column**, where PostgreSQL keeps it (`pg_attribute.atttypmod`), and not of the
type — there is one `varchar` row in `pg_type` and a number per column. So the unit is a catalog
record change first and three behaviours second, and the three do different things with their
number:

* **`varchar(n)` refuses.** Longer is `22001`, on `INSERT` and `UPDATE` alike, and trailing spaces
  are significant: `v = 'abc '` finds nothing where `v = 'abc'` finds the row.
* **`character(n)` pads.** `'x'` in a `char(3)` is stored and printed `x  `, and comparison ignores
  trailing blanks. Those are one fact, not two — a value padded to `n` on the way in makes plain
  byte comparison *be* the blank-insensitive comparison, which is what lets an index key hold a
  `character(n)` without breaking "equal values encode identically". It is why this type could not
  land in the unit that landed bare `varchar`: there is nowhere to pad to without an `n`.
* **`timestamp(p)` rounds**, and the rounding carries — `.999999` at `timestamp(3)` is the next
  whole second.

`character` with **no** number is `character(1)`, not unlimited — the opposite of `character
varying`, whose bare spelling means no limit at all.

##### The record: catalog format version 4

The one version bump tier 1 owes, and ADR 0033 named it in advance. Four little-endian bytes at the
end of each column; version 3 and version 2 records still decode, each against a golden of the
bytes it actually wrote rather than a regenerated one. Every column a version 3 catalog could hold
reads back `-1`, and that is not a compatibility shim — no type version 3 had took a number.

Stored as PostgreSQL's **own** `atttypmod` rather than a plain `n`: `varchar(5)` is `9` and
`character(3)` is `7` (the length plus a varlena header) while `timestamp(3)` is `3`. That looks
like a trap and is the opposite of one — `RowDescription`'s type-modifier column and
`pg_attribute.atttypmod` are both *defined* as that number, so any other representation means
converting on the way out to two places and getting it wrong in one.

##### Four things the capture said and reasoning would not have

1. **`timestamp(p)` rounds half away from zero**, where the microsecond rounding in the timestamp
   *parser* — fixed two units ago — breaks a tie to the **even** neighbour. `.0005` at
   `timestamp(3)` is `.001` and `.0025` is `.003`. One type, two rounding rules, in two functions.
2. **And "away from zero" is away from the year 2000.** A timestamp before 2000-01-01 is a negative
   microsecond count, and PostgreSQL negates, rounds the magnitude, and negates back — so
   `1970-01-01 00:00:00.0005` at `timestamp(3)` rounds **down**, to `.000`, where the same fraction
   in 2020 rounds up. Ties on the two sides of the epoch go opposite ways in wall-clock terms.
3. **A declared length of zero is illegal**: `varchar(0)` is `22023 length for type varchar must be
   at least 1`. A `varchar(0)` holding only the empty string is perfectly coherent and PostgreSQL
   declines to have it. The ceiling is `10485760`.
4. **One type, two vocabularies.** The declaration errors say `varchar` and `char`; the value error
   says `character varying(5)` and `character(3)`. Neither can be inferred from the other and
   `tests/corpus/pg19_typmod.txt` carries both.

##### Files this lane may edit outside `esker-sql`

The standing grant is `crates/esker-proto/src/fragment/result.rs`,
`crates/esker-store/src/columnar/decode.rs` and `crates/esker-store/src/columnar/wire.rs`, under a
git-status-first protocol. **`crates/esker-store/src/columnar/compact.rs` was added to that list**
after the `real` unit had to touch it — but the edit that got it there was *larger* than the
granted class and is reported rather than assumed: see the note in ADR 0033 and the `real` commit
`ba8ed2e`, which deleted a duplicated conversion rather than adding an arm to it. A future edit of
the granted kind — one match arm for one new type — needs no further routing.

##### The harness was comparing types without their numbers

`parity_harness`'s `type_name` rendered an OID and dropped the modifier, so every corpus holding a
`varchar(n)`, a `character(n)` or a `timestamp(p)` would have agreed **by not looking**. It renders
what `\gdesc` renders now. Same shape as the three defects the `real` unit turned up: a check that
passes because it does not ask.

Fixing it made the harness's *other* direction fire, which is the half that usually sits idle:
three entries in `tests/timestamp.rs`'s type divergences had started agreeing — a `timestamp(6)`
column whose precision this node could not print back — and the test refused to pass until they
were deleted. A divergence list that only ever grows is a list nobody reads.

##### `bpchar` costs no wire change, unlike `real`

`character(n)` is a twelfth `ColumnType` and a third member of the string family — `text`,
`varchar` and `bpchar` are one varlena told apart by OID, exactly as PostgreSQL has it. Unlike
`real` it needed **nothing** in `esker-proto`: the fragment wire maps by *value* shape and there is
no `Value::Varchar` to add, so all three travel as `ValueType::Text`. The fourth format ADR 0033
found is only owed by a type with a new representation.

#### The cast: `'x'::regtype::oid`, and what run 3 cost by shipping without it

ADR 0033 scoped this cast **with** the type surface, in writing, "because neither moves the ladder
alone". Tier 1 shipped without it. Scoreboard run 3 then measured the prediction coming true: six
types landed, `ActiveRecord`'s migration ran, the boot counter went 15 → 18, and the ladder stayed
at **rung 1** — the same statement, with the same message, as in run 2.

    SELECT 'integer'::regtype::oid  →  0A000

One cast later, rung 2 passes and rung 4 gets far enough to create tables before it stops. The
lesson is not about types, which were right and measured throughout. It is that a unit scoped as
two things was allowed to deliver one, and **the only thing that noticed was the scoreboard** —
the boot counter said 18 and rising, which reads like progress until the ladder beside it says 1.

##### Three rules a naive lookup gets wrong

* **Both spellings of every type resolve.** PostgreSQL keeps two names, the SQL one (`integer`,
  `character varying`) and `pg_type.typname`'s (`int4`, `varchar`), and `regtype` takes either.
  `'float'` is `float8` — the one alias that is neither of a type's two names.
* **Case and surrounding space do not matter.** `'INTEGER'` is `23`.
* **A typmod is parsed and discarded.** `'character varying(255)'` is `1043`. A `regtype` names a
  *type*, and the length never was part of one.

##### Why the nested cast is matched rather than composed

A real `regtype` is four bytes holding an OID that print as the type's name, and `::oid` from one
is a free coercion. This node has no such type, so `'x'::regtype` lowers to the **name** as text —
which makes `SELECT 'int4'::regtype` answer `integer` exactly, leaving only `RowDescription`'s OID
different. That would make `::oid` a text-to-oid cast, and a real server refuses one: `'integer'::oid`
is `22P02`, which this node answers too. So the pair is recognised together. It is not a shortcut
around the missing type; it is the one place where composing the two steps would have to permit a
cast PostgreSQL forbids.

#### Rung 3: a chain of joins

`ActiveRecord`'s `indexes()` sends **four tables and three joins** — `pg_class` twice under two
aliases, all `ON`, mixing `INNER` and `LEFT` — and rung 3 stopped on it for four scoreboard runs
with `0A000 more than one JOIN is not supported`.

##### Left-deep in written order is the semantics, not a planner choice

The four ways two joins combine return **1, 4, 1 and 2** rows over one fixture, and getting all
four right is what separates a join planner from a fold:

| First | Second | Rows |
|---|---|---|
| `JOIN` | `JOIN` | 1 |
| `LEFT JOIN` | `LEFT JOIN` | 4 |
| `LEFT JOIN` | `JOIN` | **1** |
| `JOIN` | `LEFT JOIN` | 2 |

The third is the one that decides it. `A LEFT JOIN B ON … JOIN C ON …` is `((A LJ B) JOIN C)`: the
inner join runs against rows the left join has already NULL-extended and throws them back out. A
chain planned as "each join against the original left table" keeps all four; a chain that reordered
its steps keeps two. Any two of the four agreeing by accident is possible, all four is not.

So `plan_chain` does **not** reorder, where the two-table path does and should — an inner join of
two tables is commutative and the probe only works on the inner side, which is worth a swap. A
chain is not free to: one `LEFT JOIN` anywhere in it fixes the order of every step after it. Rather
than reorder the all-inner prefix and stop at the first outer join, it plans as written. A
rule-based planner that is right everywhere beats one that is faster on shapes nobody sends.

Each step's scope is the tables to its left plus the one being joined, which is what lets an `ON`
reach back past the table joined in between — `ON n.oid = t.relnamespace`, as `indexes()` writes it.
The same table twice under two aliases is two entries sharing one `TableDef`: a duplicate **name**
is `42712` and a duplicate table is not.

##### `USING` in a chain is refused, and only in a chain

It does a second thing an equality cannot — it **merges** the named column — and the merge compounds:
`SELECT *` over three tables joined `USING (id)` returns four columns, not six, and a later `ON` join
bringing a third `id` makes a bare reference `42702 column reference "id" is ambiguous` where
without it the same reference resolves. Both measured in `tests/corpus/pg19_join_using.txt`.
Approximating either is a wrong answer rather than a gap, and **`ActiveRecord` sends no `USING` at
all** — zero in 5396 captured statements — so nothing is waiting on it. One join with `USING` still
runs; the corpus is there so whoever closes the gap starts from the measurement.

##### A divergence that had started agreeing

`CROSS JOIN` went into the divergence list on the assumption that it was refused like a
comma-separated `FROM`. It has always run, and the harness failed the test until the entry came out
— the second time this session that checking both directions has caught a note somebody wrote once
and never re-measured.

#### Rung 4: `pg_class`, `pg_namespace`, and `= ANY (current_schemas(false))`

One statement wants three features at once, which is why the rung is one unit and why no earlier
unit could move any part of it — a statement is served or it is not:

```sql
SELECT c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = ANY (current_schemas(false)) AND c.relkind IN ('r','v','m','p','f')
```

##### `= ANY(array)` is `IN`, and the three-valued logic is why that was checked first

`NULL = ANY(ARRAY['a'])` is NULL, `'a' = ANY(ARRAY['a', NULL])` is true, `'z' = ANY(ARRAY['a',
NULL])` is NULL, and an **empty** array is plain false. This node's `IN` answers all four the same
way — measured on both sides before the arm was written — so the lowering is free rather than a
guess.

**Expression-level arrays only**, which is the split ADR 0033's roadmap describes. There is no
array `Datum`, no array column, and nothing a `RowDescription` could type: an array exists only as
an `ANY` operand and becomes an `IN` list before the planner sees it. `SELECT current_schemas(false)`
**on its own** is therefore `0A000` — selecting it would return an array — and that is the honest
half of the split rather than an oversight, since `ActiveRecord` only ever writes it inside an `ANY`.

##### Two category-(a) bugs the capture replay found after the first version shipped

Both were in the `= ANY` half, both invisible to the boot capture, and both reachable from ordinary
`ActiveRecord` code:

1. **An unquoted `NULL` element was handed to the element's input function as the word `NULL`.**
   `text` accepted it as a string; `int4` answered `22P02`. So `1 = ANY('{NULL,1}'::int[])` was an
   error where a real server says `t` — and `where(id: [1, nil])` emits exactly that. A quoted
   `"NULL"` *is* the four characters, so the parser has to remember whether an element was quoted,
   which is why it returns `Option<String>` per element.
2. **`current_schema(false)` returned `public`.** PostgreSQL resolves a function by name *and*
   argument types, so the wrong arity is `42883 function current_schema(boolean) does not exist`,
   not a function that shrugged at an argument.

The first shipped with a comment saying the simplification "cannot be reached from anything
`ActiveRecord` sends". It could. **A claim about what a client sends belongs in a capture, not in
a comment** — and the corpus now carries both lines, which is what a replay of the corpus against
the node is for.

##### `pg_class` is the first view whose rows are not constants

They come from one scan of the same name records `CREATE TABLE` writes, so there is nothing to keep
in step and no way for the two to disagree: a table created and then dropped appears and disappears
without any code between the two knowing `pg_class` exists. That property is the whole reason unit
5 chose computed views over stored catalog tables, and this is the first view that exercises it.

`relkind` is what the name record already says the relation is: `r` for a table, `i` for an index —
and `i` for a **primary key**, which a real server lists as `r4a_pkey` even though here the row key
*is* the primary key and no separate index exists. What `relkind` describes is a relation a client
can name, and a client can name it.

Every corpus query filters by `relname`, because a real server's `pg_class` holds several hundred
system relations and this node's holds none. Comparing unfiltered counts would compare two catalogs
rather than one answer.

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

* **No `pg_catalog` write path.** The catalog views are read-only translations. **Corrected in
  unit 6 by measurement**: the refusal is `42501 permission denied: "pg_type" is a system catalog`
  and not the `0A000` this line first guessed — that is what a real server answers for `DROP
  TABLE`, `ALTER TABLE` and `CREATE INDEX` on a catalog, and `IF EXISTS` does not excuse it. DML it
  does *not* refuse for a superuser, which this node cannot follow and does not: a computed
  relation has nothing to write to and there are no roles here, so every write gets the answer a
  real server gives everyone who is not a superuser. `ALTER SYSTEM` and every other administrative
  surface phase-6a §9 classifies as such stay `0A000`.
* **No `plpgsql`, no `CREATE FUNCTION`, no triggers.** They parse (C1) and they are `0A000` (C2).
* ~~**No `time` type.**~~ **Landed.** Statement 574's `t.time :bonus_time`, and the type surface
  ADR 0033 called tier 2's second. What it does not do is arithmetic: `time - time`, `time * 2`,
  `sum` and `avg` all answer `interval`, which is a type this node does not have, so each is
  `0A000` naming it. `date + time` is folded over constants only — the engine has no arithmetic
  operator at all, and adding one is its own unit.
* ~~**No `numeric` type.**~~ **Landed** (ADR 0045). It got the unit the count justified: stored as
  digits and a scale with the written text preserved, ordered and indexable through a normalised
  key, printed by `numeric_out`'s rules, refused in the binary wire format both ways. What it
  still does not do is arithmetic — `numeric` addition, `round`, `trunc` and `sum` are the
  twenty-nine declared `answers` divergences in `tests/numeric.rs`.
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
| `SET <parameter>` for a parameter this node does not have is `42704`, and **`work_mem` pays for it** | PostgreSQL knows which parameters it has, so `SET nosuchparameter` is `42704` (measured, `tests/corpus/pg19_set_parameters.txt`) and `SET work_mem = '4MB'` simply works. Telling those two apart needs PostgreSQL's whole GUC table. This unit closed the asymmetry the row used to describe — `SET` answered `0A000` while `SHOW` and `RESET` answered `42704` for the same name — in the direction the capture supports: all three now say `42704`, which is right for every name nobody has and wrong for the handful a real server has and this node does not. Three answers that agree and are wrong about `work_mem` beat three that disagree about whether it exists. | `tests/time_machine.rs`'s `another_set_is_42704_naming_the_parameter`, `tests/slt/time_machine.slt` |
| A **namespaced** custom parameter — `SET nonesker.engine = 'row'` — is `42704` where PostgreSQL accepts and stores it | A real server validates no custom GUC at all: a namespaced name is whatever the session says it is. This node acts on the parameters in `src/parameter.rs` and refuses the rest rather than accept a setting nothing will read, which is that module's whole rule. | `tests/routing.rs`'s `DIVERGENCES` |
| ~~A duration parameter reads back in the unit it was set in~~ — **fixed**, and it was a wrong answer in a shipped unit | `SHOW` does not echo what `SET` was given: PostgreSQL converts a duration to the parameter's base unit and re-prints it in the largest unit that divides it exactly. Measured, eight corpus lines red: a bare `250` reads back `250ms`, `'2000ms'` reads back `2s`, `'120s'` reads back `2min`, `'60min'` reads back `1h`, `'1500us'` and `'2500us'` both read back `2ms` (`rint`, half to **even**), and `'500us'` reads back **`0`** — the spelling of *off*. `'10ms'` is a fixed point of all three rules, which is why the original capture's only two duration lines both passed and the rule was never seen. A fraction is legal (`'1.5s'` is `1500ms`), and there are **three** refusals sharing two sentences: unreadable (`'banana'`), magnitude beyond a C `int` (same sentence plus `HINT: Value exceeds integer range.`, so `'24d'` is accepted and `'25d'` is not), and out of range, which reports itself in the base unit (`SET statement_timeout = '-5s'` is `-5000 ms is outside the valid range …`). | `src/parameter.rs`'s `normalise_duration`, `tests/corpus/pg19_set_parameters.txt` |
| A **view** over a dropped column cannot refuse, because there are no views | PostgreSQL's `2BP01` has two producers and this node has one. A dependent that lives on the column's own table goes silently — measured, including a multi-column index where only one key column was dropped — and only a dependent living on another object raises. Of the two the capture shows, another table's foreign key referencing the column **is** implemented and refuses; a view cannot exist here at all, `CREATE VIEW` being an older `0A000`. So the rule is complete and one of its two inputs is empty. The three capture lines that pin the view case are listed in `tests/drop_column.rs` and are deleted, not edited, when `CREATE VIEW` lands — the capture already records what they must then answer. | ADR 0051, `tests/drop_column.rs`'s `DIVERGENCES` |
| `lc_monetary` boots at `C`, where the oracle's container says `en_US.utf8` | A boot value is a property of the server's locale, and this node has no locale database at all — `C` is the one locale whose rules are "no rules". What `money_test` needs is that `SET` changes it and `SHOW` reports the change, and both are exact. | `tests/set_parameters.rs`'s `DIVERGENCES` |
| `SET TIME ZONE` to any zone but UTC is `0A000` naming the zone | `timestamptz` is printed in UTC and nowhere else (`src/value/timestamp.rs`), so accepting a zone would be a setting honoured by `SHOW` and ignored by every row — the one failure a client cannot see. Older than this unit; the capture pinned it with `America/New_York` and `America/Jamaica`. | `src/parameter.rs`'s `honour`, `tests/set_parameters.rs` |
| A `SET` that changes a `GUC_REPORT` parameter sends no `ParameterStatus` | PostgreSQL tells a client when `standard_conforming_strings`, `TimeZone` or `IntervalStyle` changes, so a driver can track it. Of the values this node accepts, only `IntervalStyle` ever *changes* from what the startup packet announced — and it governs how an `interval` prints, of which this node has none. The other two are honoured only at the value they were announced with. Recorded rather than built: the report would have to leave the executor through `Outcome`, and nothing measurable is wrong today. | this table, `src/parameter.rs` |
| A column alias list (`FROM t AS x (c, d)`), and an alias on `UPDATE` / `DELETE`, are `0A000` naming themselves | A real server takes all three. The column list renames the table's columns, so ignoring it would answer a query about `c` with a column called `id` — a wrong answer rather than a gap. `UPDATE`/`DELETE` resolve against one table and have no second name to tell apart, so the alias buys nothing there; the `SELECT` side is what the 19 catalog statements need. | `tests/alias.rs`'s `DIVERGENCES` |
| ~~`SELECT 1 = '1'` is `f`~~ — **fixed**, and it was a rule rather than a patch | Two literals with no column to type either against were compared untyped, so an `int8` never equalled a `text`. A **wrong answer and not a refusal**, and older than the unit that found it: `IN` shares `reconcile` with `=` and inherited it exactly. The fix is PostgreSQL's `unknown` resolution, captured in full first: an `unknown` beside a *typed operand of any kind* takes that type, two `unknown`s are both `text` (which is why `'1' = '01'` stays `f`), and an `IN` list is typed **as a whole** so `'01' IN ('1', 1)` is `t`. **78 of 123 captured statements disagreed before it and 31 after**, and every one of those 31 is one of four gaps that were already there — `int4` (11), `numeric` (5), a cast (14), the `C` collation (1). | `tests/unknown_literal.rs`, `tests/corpus/pg19_unknown.txt` (123 statements) |
| A bare integer constant is `int8`, where PostgreSQL's is `int4` | `pg_typeof(1)` is `integer` on a real server, so `1 = '2147483648'` is `22003 value "2147483648" is out of range for type integer` and here the string reads and the comparison answers `f`. Every input error under the same rule names `bigint` where PostgreSQL names `integer`. Eleven statements, all of them the same one fact, and it closes when `int4` exists — the type-surface unit this plan blocks on `esker-keys`. | `tests/unknown_literal.rs`'s `DIVERGENCES` |
| A decimal constant is `double precision`, where PostgreSQL's is `numeric` | The same choice `Literal::Decimal` already makes everywhere in this crate, `SELECT 1.5` included, so it is not new — the corpus is the first thing to put a number on it. `double` reproduces `numeric` for every value it can hold and stops at about 17 digits: `0.1 = '0.1000000000000000000001'` is `f` on a real server and `t` here. Its neighbour, `1 = 1.0`, is `t` there and `f` here for the opposite reason — no promotion between `int8` and `float8` — and promoting to `double` would fix that line and break `9007199254740993 = 9007199254740992.0`, which this node currently gets right. Recorded with its counterexample rather than fixed; ADR 0031's numeric backlog. | `tests/unknown_literal.rs`'s `DIVERGENCES` |
| A **partial index is maintained and never read** | Its entries and its `UNIQUE` are exact — a row is in the index when the predicate is *true*, so a NULL keeps it out, and `UNIQUE` constrains only the rows it holds. What it never does is narrow a *scan*: choosing a partial index is correct only when the query's own predicate implies the index's, and this crate has no implication prover. Picking it anyway would answer a correct-looking query with the rows the index happens to hold — ADR 0020's "skip the backfill" anomaly by another road. The cost is a scan that could have been an index seek; the alternative is missing rows. | `tests/partial_index.rs`, `catalog::IndexDef::predicate` |
| An **expression index is never read** either, for the reason the partial one is not | There is no constant in a `WHERE` to pin an expression to, and pinning it to the column underneath would answer `WHERE b = 'X'` from an index on `lower(b)`. `catalog::IndexDef::key_columns` is the shape of that rule: it is `None` for an expression index, and every place that would narrow a read asks it. The `UNIQUE` is exact. | `tests/expression_index.rs`, `catalog::IndexKey` |
| `pg_get_indexdef` prints an index expression as **this parser renders it**, where PostgreSQL re-prints a parsed tree | The two agree on everything `schema.rb` writes — a call prints `lower(b)`, an operator `((b IS NULL))`, a constant `(1)`, and the three-way rule for that is measured and stored (`catalog::ExprShape`). They differ in two reachable places, both text and neither behaviour: a **cast** prints `a::TEXT` where PostgreSQL prints `(a)::text`, and the **per-column** form `pg_get_indexdef(oid, n, true)` keeps parentheses PostgreSQL drops (`(NOT (b IS NULL))` against `(NOT b IS NULL)`). Closing them means a deparser of our own over `plan::Expr`; recorded with both captures instead. | `tests/expression_index.rs`'s `DIVERGENCES`, `tests/corpus/pg19_expression_index.txt` |
| A `DESC` index column is **recorded and printed, and changes nothing else** | An index here is read in exactly two ways — a `UNIQUE` check and a lookup with the whole key pinned to constants — and neither depends on the order the entries are in; nothing chooses an index to satisfy an `ORDER BY`. So the direction and the null placement are stored (`catalog::KeyOrder`, `pg_index.indoption`) and re-derived into the shortest spelling `pg_get_indexdef` prints, which is how `ActiveRecord` gets `order: :desc` back out of a schema dump. When a plan does read an index for order, this is the field it reads. | `tests/desc_index.rs`, `catalog::KeyOrder` |
| A direction on a **constraint's** columns is `0A000` where PostgreSQL's grammar has none (`42601`) | `UNIQUE (a DESC)` and `PRIMARY KEY (a NULLS FIRST)` are syntax errors on a real server — measured — and refusals by name here. Both refuse; the code differs. `ALTER TABLE … ADD CONSTRAINT … UNIQUE` refuses one step earlier still, because this node does not have that action at all. | `tests/desc_index.rs`'s `DIVERGENCES` |
| `NO ACTION` and `RESTRICT` are one behaviour here, and two codes | On a real server `NO ACTION` can be deferred to the end of the statement and `RESTRICT` cannot; every check here is **immediate**, so both refuse at the same moment. `confdeltype` is `a` and `r` and the definition text differs, which is what a client reads. The difference becomes visible only with `INITIALLY DEFERRED`, which is refused by name for that reason. | `tests/foreign_key.rs`, `catalog::ReferentialAction` |
| `FOREIGN KEY ... INITIALLY DEFERRED`, `MATCH FULL`, `ON DELETE SET NULL` and `SET DEFAULT` are `0A000` naming themselves | `INITIALLY DEFERRED` would **change an answer**: a transaction that violates the constraint in the middle and repairs it before `COMMIT` succeeds on a real server and would be refused here. `MATCH FULL` refuses a partly-NULL key where the default `SIMPLE` admits it. The two `SET` actions write a value into the child rather than refusing or removing. None appears in anything `ActiveRecord` emits — `add_foreign_key` writes `DEFERRABLE INITIALLY IMMEDIATE` and `on_delete: :cascade`. | `tests/foreign_key.rs`, `parse::lower::lower_foreign_key` |
| A constraint added by `ALTER TABLE` is **not validated against the rows already there** | The same gap `CHECK` has and for the same reason: a backfill is ADR 0020's schema-change machinery and `ADD CONSTRAINT` does not go through it yet. `ActiveRecord` writes every `ADD CONSTRAINT` before it writes any rows, so nothing it does reaches it. `TODO(post-v1)`, and a scan of the child table with the lookup `exec::foreign_key` already has. | `tests/check_constraint.rs`, `exec::ddl::add_foreign_key` |
| `ActiveRecord`'s `foreign_keys()` still cannot run | It reads `conkey`/`confkey` through `generate_subscripts` and `array_agg`, which needs the array type and its functions. The columns are here as text and `SELECT conkey` answers; `conkey[1]` is `0A000`. A **schema dump** blocker, not a schema **load** one — statement 52 loads. | `catalog::pg_constraint::CONSTRAINT_COLUMNS` |
| `date` has **no arithmetic**, which is ten of its nineteen declared gaps | This crate has no arithmetic operators at all — `a + 1` is `0A000` naming itself for every type — so `date - date`, `date ± integer`, `date ± interval` and `age` are all one older gap arriving through a new type. PostgreSQL raises `42883` for the pairs that have no operator (`date + numeric`) where this node says `0A000` for every pair: both refuse, and the reason differs. | `tests/date.rs`'s `DIVERGENCES` |
| `DateStyle` is a session parameter this node does not have, so a `date` has **one** spelling | The default `ISO, MDY`, which is the only one `ActiveRecord` ever sees — it sets `IntervalStyle` and nothing else of this family. The setting changes both halves: `01/02/2020` is 2 January under `MDY` and 1 February under `DMY`, **one literal and two dates with no error either way**, and the output has four spellings of one value. The nine capture lines are in the harness and not in the repo corpus, because the same statement disagrees before `RESET` and agrees after — which a divergence list keyed by statement text cannot say. | `tests/corpus/pg19_date.txt`'s header |
| `NULLS NOT DISTINCT` on a **constraint** is `0A000` where PostgreSQL takes it | `CREATE INDEX … NULLS NOT DISTINCT` is built and exact; `ALTER TABLE … ADD CONSTRAINT … UNIQUE NULLS NOT DISTINCT` stops one step earlier, at the `ADD CONSTRAINT … UNIQUE` action this node has never had. The clause is not what is missing. | `tests/nulls_not_distinct.rs`'s `DIVERGENCES` |
| The **forward** `'x'::regclass` is an oid where a real server's is an oid that *prints as a name* | `'rc'::regclass::text` is `rc` on PostgreSQL 19 and the oid here. The forward cast is resolved **once per statement before the plan is built** (`exec::Executor::bound`), which is what makes `WHERE attrelid = 'x'::regclass` one catalog read per statement rather than one per row; the value it leaves behind is an `int8`, and `::text` of an `int8` prints the number. The **reverse** direction — `t2.oid::regclass::text`, the one `ActiveRecord` writes — is exact. Closing it means a value that carries an oid and prints a name, which is a type and not a cast. | `tests/regclass_name.rs`'s `the_forward_cast_prints_its_oid_where_a_real_server_prints_the_name` |
| `GENERATED ALWAYS AS (expr) **VIRTUAL**` is a **syntax error**, which breaks contract C1 | PostgreSQL 19 takes it and reports `attgenerated` `v` — measured. `sqlparser` 0.62.0 expects `STORED` after the expression and makes the word a parse error, so this node answers `42601` where a real server answers a table. It is a *parser* gap, not a lowering one: `parse::lower` already refuses `VIRTUAL` by name for the day the parser can read it, because `VIRTUAL` computes on read where `STORED` computes on write and taking the word while storing would answer a stale value after the source changed. The `STORED` form, which is the one `ActiveRecord` writes, runs. | `parse::lower`'s `ColumnOption::Generated` arm, `tests/lowering.rs` |
| A **multi-statement simple Query is not all-or-nothing**, where PostgreSQL wraps one in an implicit transaction | Measured, on both: `ALTER TABLE mp1 ADD COLUMN zz int;ALTER TABLE nosuchtable ADD COLUMN yy int` in **one** message fails on the second statement and PostgreSQL leaves `zz` **unadded**; this node keeps it. Sent as two messages, the first survives on both — same two statements, opposite outcome, decided only by whether they shared a message. It matters for `ALTER TABLE … DISABLE TRIGGER ALL` in particular, because `disable_referential_integrity` sends the **whole schema's worth in one string**: one bad table name leaves the schema half-disabled here and untouched there. `crate::pgwire::session::run_query` runs the statements in a loop with no transaction around them; the fix is that loop, not any statement in it. Found by the `DISABLE TRIGGER` unit and **owed by the session lane**, not the DDL one. | `crate::pgwire::session::run_query`, `tests/corpus/pg19_ddl_cascade_fk_trigger.txt`'s header |
| `EXPLAIN` prints `Aggregate` / `Group Aggregate` / `Unique` and no costs | The same divergence the access-path plans already carry: PostgreSQL chooses between `HashAggregate` and `GroupAggregate` and prints an estimate; there is one strategy here and no cost model, so the name says what it is rather than implying a choice that was not made. | `tests/slt/aggregate.slt`, `tests/slt/access_paths.slt` |
| `ALTER TABLE … ATTACH PARTITION` / `DETACH PARTITION` are `0A000` naming themselves, where PostgreSQL runs them | `sqlparser` 0.62.0 has only ClickHouse's `ATTACH PARTITION <expr>` (`AlterTableOperation::AttachPartition { partition: Partition }`), and PostgreSQL's form carries a bound — `ATTACH PARTITION p FOR VALUES IN (2)` — with nowhere in that shape to put it, so the statement does not parse under the PostgreSQL dialect either. Neither spelling is in `postgresql_specific_schema.rb`: the capture probes them, the suite never sends them. Measured on a real server: `DETACH` keeps the table and its rows, the parent's `count(*)` drops by exactly that partition's, `relispartition` becomes `f`, and re-attaching puts all of it back — none of which changes `pg_get_indexdef`'s `ON ONLY`. | `parse::mod`'s construct recognizer, `tests/partition.rs` |
| `INCLUDE (c DESC)` and `INCLUDE (c varchar_pattern_ops)` are `42601` where PostgreSQL answers `42P17` with its own sentence | `sqlparser` 0.62.0 types the clause as `CreateIndex::include: Vec<Ident>` — bare identifiers, with nowhere to put an ordering or an operator class — so both are syntax errors before the lowering is reached. A real server refuses them too, and the difference is which layer says so and in which words: `including column does not support ASC/DESC options` and `including column does not support an operator class`. The plain form, which is what statement 787 writes, runs. | `parse::lower`'s `lower_create_index`, `tests/include_index.rs` |
| The **constraint** spelling `UNIQUE (c) INCLUDE (d)` is `42601` where PostgreSQL takes it | `INCLUDE` lives in two grammars and `sqlparser` 0.62.0's `UniqueConstraint` has no field for it, so the constraint form does not parse — on top of `ALTER TABLE … ADD CONSTRAINT … UNIQUE`, an action this node has never had in any case. `pg_get_constraintdef` prints it back as `UNIQUE (account_id) INCLUDE (name)` on a real server, and the index it builds is byte-identical to the one `CREATE UNIQUE INDEX … INCLUDE` builds here. | `parse::lower`'s `lower_added_constraint`, `tests/include_index.rs` |
| The arbiter's `WHERE` — `ON CONFLICT (a) WHERE b IS NOT NULL` — is `42601` where PostgreSQL infers a partial index from it | `sqlparser` 0.62.0's `ConflictTarget::Columns` is a bare `Vec<Ident>`, with nowhere to put a predicate. The consequence is not only the statement: a **partial** unique index can never be inferred here at all, so a bare `ON CONFLICT (a)` over one is `42P10 there is no unique or exclusion constraint matching the ON CONFLICT specification` — which is what a real server answers too, so that half agrees. The target-less and column-list forms, which are the two `build_insert_sql` writes, both parse and run. | `parse::lower`'s `lower_on_conflict`, `tests/on_conflict.rs` |
| `CREATE SCHEMA <name> CREATE TABLE …` is `42601` where PostgreSQL takes the whole thing | `sqlparser` 0.62.0's `CREATE SCHEMA` has no element list, so it stops at the first nested `CREATE` — `Expected: end of statement, found: CREATE at Column: 27`. It is the spelling `schema_test.rb`'s `setup` uses for **both** of its schemas, so the whole named-schema half of that file is unreachable before this is read, independently of whether the namespace exists. | `parse::lower`, `tests/schema_namespace.rs` |
| `INSERT INTO t DEFAULT VALUES, DEFAULT VALUES` is `42601` with `sqlparser`'s sentence, not PostgreSQL's | Both servers refuse it — it is not a `VALUES` clause with one row and has no multi-row form — and both say `42601`; PostgreSQL names the comma and `sqlparser` says `Expected: end of statement`. The message on a syntax error has never been claimed to be PostgreSQL's (`phase-6a.md` §1); what C1 promises is that a statement PostgreSQL *accepts* is never `42601`. The same trade `numeric`, `time` and `uuid` already record for their own negative-typmod lines. | `tests/insert_default_values.rs`'s `DIVERGENCES` |
| **Nothing waits for a row lock**, so `lock_timeout`, `statement_timeout`, `FOR UPDATE NOWAIT`, `FOR UPDATE SKIP LOCKED` and `40P01` are all unanswerable | A Percolator prewrite that meets a live lock is `40001` after a bounded backoff, never an indefinite wait, so there is no wait for a timeout to end and no cycle for a deadlock detector to find. PostgreSQL answers `55P03` (`lock_timeout` *and* `NOWAIT`, two sentences under one code), `57014` (`statement_timeout` ending the same wait), and `40P01` killing exactly **one** of two deadlocked transactions. Here both writers proceed and the loser is refused at `COMMIT` with `40001`. The two timeouts are accepted at `0` and refused by name above it rather than accepted and ignored — which is what turns a twenty-minute hang into a named refusal. | ADR 0031, `tests/transaction_timeouts.rs`'s `DIVERGENCES` |
| `BEGIN ISOLATION LEVEL SERIALIZABLE` gives **snapshot isolation**, and two commits that PostgreSQL's SSI separates both succeed | Measured on the capture: two transactions each read `sum(n)` and each insert a *different* row; PostgreSQL refuses the second commit with `40001` and a pivot `DETAIL`, leaving 3 rows. This node's conflict rule is about the keys a transaction **wrote**, so both commit and the table has 4. The standing caveat of ADR 0031, now with the statement that shows it. | ADR 0031, `tests/transaction_timeouts.rs` |
| ~~`idle_in_transaction_session_timeout` is recorded and does not terminate the session~~ — **built**, and it is the only one of this family that was a missing feature rather than a missing wait | It does not cancel a statement, it **terminates the session**, so it belongs around the read that waits for the client (`pgwire::server::Connection::run`) and nowhere else — the executor is not running while it fires. `25P03` with severity **`FATAL`**, not `ERROR`: a node that reported it as a statement error and kept the connection would leave a client waiting for a server that had agreed to go. The block is rolled back before the socket closes, and a session idling with **no block open** is left alone, which is the half an implementation that timed every read would break. | `tests/transaction_timeouts.rs`, `idling_inside_a_block_terminates_the_session` |
| The `||` operator is `0A000` naming itself | It is not in `plan::BinaryOp` at all, and its rules are its own rather than a corner of anything built: PostgreSQL's `||` is NULL when either side is, where the `concat` **function** this node does have skips NULLs; `'a' || 1` resolves through `anynonarray` while `1 || 2` is `42883 operator is not unique`; and `array || array` is a third thing again. None of that is measured, so it is refused by name rather than guessed at. The rule the one capture line using it was carrying — that an `UPDATE … FROM`'s `FROM` scan sees the **pre-statement** rows — is pinned instead by a probe written in operators the node has (`s10b`, `SET hits = c.hits + 1 … WHERE c.id = (a.id % 3) + 1`, which answers `1, 1, 2` if a write leaks into the scan). | `tests/update_from.rs`'s `DIVERGENCES` |
| `DELETE … USING` is `0A000` naming itself, where `UPDATE … FROM` now runs | The same mechanism spelled for the other verb, and the other verb is what makes it a unit rather than a corner: `plan::Delete` has none of the three fields `plan::Update` gained — an alias, a `FROM` and its joins — so the clause is refused in `lower_delete` rather than half-built. Its answer is **already measured** in the same capture, so the day it is needed there is nothing left to ask PostgreSQL. `ActiveRecord` sends the subquery form for `delete_all` (`tests/write_in_subquery.rs`), not this one, which is why it is second. | `tests/update_from.rs`'s `DIVERGENCES` |
| `CREATE DATABASE`'s option list is `0A000` naming the option, where PostgreSQL runs it | `sqlparser` 0.62.0's `CREATE DATABASE` grammar carries `LOCATION`, `MANAGEDLOCATION`, `CLONE` and MySQL's `CHARACTER SET`/`COLLATE`, and nothing PostgreSQL spells — `ENCODING`, which is exactly what `rake db:create` sends, exists in that crate only as a `COPY` option. So every option is a `42601` before it can be a `0A000`, which is a **C1 break**: `tests/syntax_corpus` caught it in the same run that removed the old blanket refusal. One row per option keyword in `parse`'s table restores the refusal and names what the user wrote; the price is a database *named* for one of those words, which is a `0A000` about a legal statement rather than a syntax error about one. | `parse::mod`'s `UNSUPPORTED`, `tests/database.rs` |
| `DROP DATABASE` refuses only the database **this** session is serving | PostgreSQL refuses to drop one that *any* session is connected to; that needs a registry of live sessions, which this node does not have. What is enforced is `55006` for the current session's own database and nothing for the rest — so two clients can still drop each other's. | `exec::ddl::drop_database`, ADR 0052 |
| `DROP DATABASE` empties the tenant key by key, where PostgreSQL unlinks a directory | The rows of every database share one key space here, so emptying one is a scan rather than an `unlink`. Dropping the directory row alone would be O(1) and would leave the user's rows on disk for ever — ids never repeat, so nothing would read them again and nothing would reclaim them — and a statement that says it deleted a database and did not is the worse failure. A database too large for one transaction fails loudly rather than half-emptying; background reclamation is the follow-on. | `catalog::drop_database`, ADR 0052 |

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
| 2026-09-01, **twice** | `joint_gate`'s two transport calls have each missed their 30-second deadline once under a fully parallel `cargo test`: first `a_lock_the_ttl_kills_resolves_the_same_way_on_both_engines` on the `TxnKv` call, then `the_learner_answers_fragments_that_agree_with_a_row_scan` on the fragment call — `Timeout { no answer from 127.0.0.1:60224 in 30s }`. Both pass standalone. **Two different tests, one failure mode**, which narrows it: this is not the lock-expiry clock `1f22077` hardened, it is a 30-second RPC deadline against however many test binaries this machine is running at once. | **chased — see below.** Both calls dump instead of unwrapping: `target/joint-gate-transport-<ts>.txt`, naming who led the region, every store's address, leadership and applied index, and the deadline. |
| 2026-09-01, **third** | The same test and the same call, and the dump says it is **not the deadline**: `connection closed: region 1 stopped leading with this proposal in its log`, with **no store leading** and all four agreed at `applied=16`. An election gap under a saturated machine, not an expired timeout. | **closed in unit 6.** The retry is in, at both call sites, on the leadership error only — see below. |

### The chase, run — and it is not the deadline

The third sighting came under a fully parallel `cargo test` on 2026-09-01 and it is a **different
failure mode from the first two**, which is what the dump was put there to find out:

```
a call to the leader of region 1 failed
error            connection closed: region 1 stopped leading with this proposal in its log; it may still commit
deadline         30s

-- every store this caller can see --------------------------------
store 1  address=127.0.0.1:55664  leader=Some(false)  applied=Some(16)
store 2  address=127.0.0.1:55671  leader=Some(false)  applied=Some(16)
store 3  address=127.0.0.1:55678  leader=Some(false)  applied=Some(16)
store 4  address=127.0.0.1:55681  leader=Some(false)  applied=Some(16)
```

**No store is the leader, and all four agree at `applied=16`.** That is the gap between a
step-down and the next election, not an expired deadline — the 30 seconds never ran out. So the
watched entry's working theory ("a 30-second RPC deadline against however many test binaries this
machine is running at once") is *half* the story: the load is the same cause, and it produces two
different mechanisms.

* **The deadline** (sightings 1 and 2): the call waits and the answer never comes.
* **The election gap** (sighting 3): the leader misses its heartbeat tick because the machine is
  saturated, a follower campaigns, and the in-flight proposal's leader steps down under it. The
  caller is told so immediately, which is why the deadline is untouched.

The two want different fixes, and the second one's is the smaller and the more correct: **the
caller should retry on a leadership change**, which is what a real client does — `esker-client`
treats `NotLeader` as a redirect and tries again. The test asserts *what the two engines answer*,
not that the first RPC attempt lands on a leader that is still leading when it commits, so a retry
is not a workaround, it is the assertion being written correctly.

Standalone it passes in 5.65s, and this phase's changes are in the expression layer with nothing
between them and Raft leadership.

**Handed over rather than fixed**: `tests/joint_gate.rs` was another lane's file (§ "Lane",
above), and the change was theirs to make — one retry, on the leadership error only, at each of the
two transport call sites that already dump.

### Closed, in unit 6

That lane retired and the file came here, and the retry is in: `call_through_an_election` at both
call sites. **Only a leadership change is retried** — anything else still writes the cluster down
and fails, which is what the dump was added for and what a blanket retry would have thrown away.

Two things the fix had to know that the diagnosis did not say:

* **the event has two error shapes, not one.** `esker_store::peer`'s `stopped_leading` answers an
  orphaned *read* with `NotLeader` and a *proposal already in the log* with `Closed { detail }`.
  The dump caught the second; matching only it would leave the other half of the same instant
  unretried. `ProtoError::is_retryable` covers the first and deliberately not the second — a
  closed connection with an unknown outcome is not safe for a caller to repeat in general — so the
  predicate is local to the test and says why.
* **the leader has to be looked up per attempt.** The `TxnKv` call site resolved the leader once
  and then retried against it, which retries the store that just stepped down. The address and the
  request are now built by a closure the retry calls each time, and a closure that returns `None`
  means *nothing leads the region at this instant* — the gap itself, and a wait rather than a
  failure.

A fourth sighting was caught in the same session, on a fully parallel `cargo test` right after
another had finished, and it was **two failures rather than one**: the fragment call, and — in a
different test — `Backend::begin` answering `StoreUnavailable("gave up after 9 attempts: region
epoch does not match")`. The second is not this fix's: it is the *client's* own retry, which
already retries nine times, exhausting them against an epoch that keeps moving. Same cause
(saturation), third mechanism, and it is recorded here rather than claimed to be fixed. Three
consecutive standalone runs are clean.

### Closed again, at the fourth sighting: the deadline

The retry above covers the **election gap** and deliberately not the **deadline**, which is §8's
first mechanism. The fourth sighting was a `just`-style per-crate gate run on 2026-09-01, and the
dump named it exactly: `a_lock_the_ttl_kills_resolves_the_same_way_on_both_engines` died on its
*fragment* call with `timed out: no answer from 127.0.0.1:61519 in 30s`, one store visible and not
leading. Same saturation as before and worse — two lanes now build in one tree.

**What the fix turns on is a distinction the first one did not need: retryability after a timeout
is a property of the *request*, not of the error.** A timed-out call has an **unknown** outcome —
the request may have applied and the answer been lost — so "retry a timeout" is safe for one of
these two calls and unsafe for the other:

* the **fragment** call is a scan at a fixed `ts`. Repeating it cannot change what the cluster
  holds, so it retries its deadline: `Idempotent::Yes`.
* the **`TxnKv`** call is a prewrite whose commit never comes, which is the whole subject of the
  test around it. Repeating it would invent a second prewrite, so it still dumps:
  `Idempotent::No`.

Neither call answers for the other, and the enum is at the call sites rather than inside the helper
so that a future third caller has to say which it is.

## 8b. Handover — what unit 6 leaves, and what the ladder says to do next

**The next unit is the type surface, and the evidence is the ladder rather than a count.** Two
rankings of what is left disagree, and `docs/bench/rails-scoreboard.md` prints both on purpose:

* by **statements unblocked**, a second `JOIN` is the biggest group at 7;
* by **the ladder**, only the type surface can move rung 2, and nothing else is close — a second
  join, `= ANY`, a qualified name and the rest of the catalog are all on paths a client reaches
  *after* it has run a migration, and this one cannot run a migration.

So: `integer`, `character varying`, `timestamp`, **and `'x'::regtype::oid` with them**, because
neither half moves the ladder alone. `lookup_cast_type` asks the cast question and `pg_type`
answers it, and today the cast is `0A000` and the answer would be "no such type" if it were not.
[ADR 0033](../adr/0033-tier-1-of-the-type-surface.md) is the plan, written before the
code as the brief required, and it carries the measured facts — including the finding that this is
**not** an on-disk format change, which is what makes it one unit.

**What is built and where it is**, so nothing has to be rediscovered:

| Thing | Where |
|---|---|
| the `unknown` resolution rule | `src/exec/query.rs`'s `reconcile`, `common_type`, `give_type`; `tests/unknown_literal.rs`, `tests/corpus/pg19_unknown.txt` (123 statements) |
| `pg_type`, `pg_range`, and the write refusal | `src/catalog/pg_catalog.rs`; `tests/pg_catalog.rs`, `tests/corpus/pg19_pg_catalog.txt` (46) |
| the computed-relation plan node | `plan::Node::CatalogView`, `NestedLoop::inner_view`; `exec::query::access_path` and `join_node`; `exec::cursor`'s `Kind::Rows` |
| the transport retry | `tests/joint_gate.rs`'s `call_through_an_election` |
| the second scoreboard | `docs/bench/rails-scoreboard.md`, run 2 |

**Three things a successor should know before touching any of it:**

1. **The catalog derives from `ColumnType::ALL`.** Adding a stored type adds a `pg_type` row for
   free and will not compile until its `typname` and `typinput` are named — both are exhaustive
   matches in `src/catalog/pg_catalog.rs`, and both were measured (`int4in` has no underscore,
   `timestamp_in` does).
2. **A capture that writes to `pg_catalog` breaks the oracle.** The `esker-pg19` container's
   `esker` database had to be dropped and recreated after a probe deleted `int8` out of its
   `pg_type` — autocommit, superuser, no error. Wrap write probes in `BEGIN`/`ROLLBACK`, and
   re-verify any corpus captured in that session by replaying `cut -f1 corpus.txt` and diffing.
3. **`activerecord_surface.rs` asserts its count exactly.** Moving it is the point of a unit; a
   unit that moves it has to say which unit moved it, in the doc comment beside the number.

**Nothing is half-built.** The two write paths a catalog relation could have been reached through
— `existing_relation` for DDL and `require_table` for DML — both go through
`pg_catalog::refuse_write`, and `tests/pg_catalog.rs` covers the two verbs where the answer is
deliberately *not* `42501` (`DROP INDEX` is `42809`, `CREATE TABLE` is `42P07`).

## 9. Progress

| Unit | State | Commit |
|---|---|---|
| 0 — ADR, plan, aggregate capture | **done** | `de3c465`, `32fb58f` |
| 1 — aggregates | **done** | `1d78a96` |
| 2 — sequences and `RETURNING` | **done** | `e1b1bd2`, `7a7d4f3`, `bf28e0d` |
| 3 — savepoints | **done** | `779ae2e` (capture), `a74b724` |
| 4 — joins | **`LEFT`, `ON`, `USING` done**; a second join is not | `56d23e2` |
| 5 — `pg_catalog` | **in progress, in the measured order.** Captured and re-ordered (`b8d90e7`); the **table alias**, the **six `SET`s + two `SHOW`s** and **`IN (list)`** built, taking `ActiveRecord`'s 36 from 3 served to **11** and moving five statements onto the catalog; the type surface **blocked on `esker-keys`** and reported; the translation approach **decided** (views over records). What is left is the catalog's content, starting at `pg_type`. | `b8d90e7`, `b0eca1c`, this commit |
| 6 — the scoreboard | **first run done**: the harness runs the ladder and all 426 suite files, `docs/bench/rails-scoreboard.md` carries both. 59 files reach a test, 367 die at `establish_connection`; rung 1 passes. Its finding — `IN (list)`, the first query `ActiveRecord` sends — was built in the same round, and rung 2 now stops on the catalog | `6971a30` |
| 6 — the two inherited fixes | **done**: PostgreSQL's `unknown` resolution (`SELECT 1 = '1'` was `f`), captured in 123 statements and fixed as the rule it is — 78 disagreements became 31, all four remaining classes declared; and the watched transport flake, retried at both `joint_gate` call sites on a leadership change only. §6 and §8 | `8b7d7c1` |
| 6 — `pg_type` and `pg_range` | **done**: the catalog's first slice, computed rather than stored, 46 captured statements with 41 agreeing on the first run. `ActiveRecord`'s 36 went 11 → **15**, and the ladder's rung 2 moved off the catalog and onto `'integer'::regtype::oid`. The suite is unchanged and the scoreboard says why | this commit |
| 6 — the second scoreboard | **published**: [`docs/bench/rails-scoreboard.md`](../bench/rails-scoreboard.md) run 2, with before/after on all three numbers, both rankings for the next unit, and the reproduce-it commands | this commit |
| 78 — the expression index | **done**: `CREATE UNIQUE INDEX … ON t ((lower(b)))`, the key `schema.rb`'s statement 78 wants. `IndexDef.columns` became `IndexDef.keys` — a column position **or** an expression — with catalog record **v8**, and `pg_index.indkey` is `0` for an expression part because that is the byte `ActiveRecord` branches on. Two of statement 77's loose ends closed with it: `pg_get_indexdef` prints the `WHERE`, and `indpred` is the predicate rather than NULL. Two bugs found and fixed: **neither backfill checked the partial predicate**, so `CREATE UNIQUE INDEX … WHERE …` over an existing table indexed the rows the predicate excludes and could refuse itself with `23505` — the four places that build an index entry are now one (`exec::index`). 47 captured statements, 9 tests | this commit |
| 189 — the `DESC` index column | **done**: all four direction/null-placement combinations, stored as `catalog::KeyOrder` and re-derived into the shortest spelling — `(a ASC NULLS FIRST)` prints `(a NULLS FIRST)` and `(a DESC NULLS FIRST)` prints `(a DESC)`, because **each direction has its own default null placement**, so the text cannot be round-tripped from what was written. Catalog record **v9**, one `indoption` byte per key part, and `pg_index.indoption` is that bitmask (`0`, `2`, `1`, `3`). Recorded and printed and nothing else: uniqueness is order-independent and no plan reads an index for order. 36 captured statements, 5 tests | this commit |
| 52 — `FOREIGN KEY`, enforced | **done**, and it is the statement nothing after was reachable past (r1 run 12). Both directions, immediate, inside the statement's transaction: existence on `INSERT`/`UPDATE`; `NO ACTION`/`RESTRICT`/`CASCADE` on the parent's `DELETE` **and** `UPDATE`, which is the half an implementation that only hooks `DELETE` misses; a cascade **chain**, depth-first. `pg_constraint` grows `contype = 'f'` with `confrelid`/`conkey`/`confkey` and `pg_get_constraintdef`; `DROP TABLE` on a referenced table is `2BP01`. Catalog record **v10**, plus a back-reference key space so a parent's delete is a prefix scan and not a scan of the catalog. `DEFERRABLE INITIALLY IMMEDIATE` accepted, `INITIALLY DEFERRED` refused by name. Inline `REFERENCES` in `CREATE TABLE` landed with it — statement 417 of the DDL trio, for free, because it is the same constraint. **50 captured statements, every row byte-identical to PostgreSQL 19**; 7 tests | this commit |
| 255 — the type `DATE` | **done**, tier 2's first type. Four bytes, days from 2000-01-01 — PostgreSQL's own representation — through all five layers: `esker-keys` (variant, row and key encodings), `esker-columnar` (tag 15, riding the widened integer run like an `int4`), `esker-proto` (`ValueType` tag 11), the two `esker-store` columnar files, and `esker-sql`. Catalog record tag 15. **Every value it stores, orders, compares, prints and reads back is byte-identical to PostgreSQL 19** across 65 captured statements; all 19 declared divergences are refusals or features this node does not have, none a different answer. Two bugs fixed on the way that were **not** this type's: the datetime parser never sent PostgreSQL's `DateStyle` hint (`2020-13-01` gets it, `2020-02-30` does not — a 13 could have been a day), and two typed literals of different families compared to `f` where a real server raises `42883`. 7 tests | this commit |
| 15/29/31/32 — `col_description`, `obj_description`, `pg_get_partkeydef` | **done**, and it moved the boot counter **25 → 26**: `columns()` — the statement `ActiveRecord` sends about every table it has heard of — was missing `col_description` and nothing else. All three answer **NULL for every input**, and that is the answer rather than a stub: a comment is a row in `pg_description`, `COMMENT ON` and `PARTITION BY` are both `0A000` here, and NULL is exactly what a real server answers whenever *it* has nothing to find — an oid that names nothing, an attnum out of range or negative, a NULL argument, an index, an unknown catalog *name*. **23 captured statements, zero divergences**, which is unusual enough to say out loud. Two entries also left `activerecord_schema_dump`'s gap list. 4 tests | this commit |
| 195 — `NULLS NOT DISTINCT` | **done**: the one clause in an index definition that changes which rows are **refused** rather than how they are stored or printed. A `UNIQUE` index normally admits any number of NULLs — two unknowns are not known to be equal — and this reverses it, which in this crate is exactly "drop the primary-key suffix from the entry": the second NULL then writes the key the first one holds. Catalog record **v11**, `pg_index.indnullsnotdistinct`, and the clause printed **after the key list and before the `WHERE`**. Accepted and printed on a non-unique index too, where it can refuse nothing. **21 captured statements, 20 byte-identical**; the one gap is the `ADD CONSTRAINT … UNIQUE` action, not the clause. 4 tests | this commit |
| 198 — the `CASE` expression | **done**, and it is the one line **all 367** non-loading files of `activerecord/test` stopped on at run 16: `CREATE INDEX … ON "companies" ((CASE WHEN rating > 0 THEN lower(name) END) DESC)`. Built as an expression first — planning, typing, evaluation — and reached from an index key second, because they are one mechanism. **The branch chosen is the only one evaluated**, which is observable rather than an optimisation. The branch types resolve over `[ELSE, THEN₁, …]` — the `ELSE` **first**, which is what decides the order `42804 CASE types text and bigint` names them in; an `unknown` branch is *converted* (`22P02`) rather than refused, and only two known types disagreeing is `42804`. `pg_get_indexdef` prints it over **five indented lines with the implicit `ELSE` materialised as `NULL::<type>`**, which is not the written text and is not knowable until the expression meets the table — so a `CASE` is the first index expression stored **deparsed** rather than as written. Its parentheses are a *value*'s in all three places, measured, so no new `ExprShape`. Two bugs closed on the way that were **not** this unit's: an index key that is neither a column nor a bare call was **accepted** where PostgreSQL's `index_elem` grammar is `42601` (`ON t (a + 1)`, `(name::text)`, `(1)` — this node was building indexes a real server refuses), and a `23505` `DETAIL` named a *value*-shaped key one parenthesis short (`Key (1)` for `Key ((1))`). **57 captured statements, 54 byte-identical**; the three declared divergences are all `1/0`, the only expression PostgreSQL can be made to raise from an unreached branch and one this node cannot write because it has no arithmetic operators. 5 tests | this commit |
| 403/405/430 — the DDL trio | **done**: `ALTER TABLE … ENABLE/DISABLE TRIGGER`, `DROP … CASCADE`, and the inline `FOREIGN KEY` that turned out to be built already (it landed with statement 52 — 6 of the trio's 22 refusals were closed before this unit started). `DISABLE TRIGGER ALL` is **not a no-op on a node with no triggers**: `ALL` covers PostgreSQL's internal foreign-key triggers, so it suspends this table's referential checks, which is the whole reason `ActiveRecord` writes it around every fixture load. The capture did not show that — it never writes a row while triggers are off — so all four halves were measured for this unit: the child's check goes with the **child's** flag, the parent's with the **parent's**, disabling the parent does **not** let a bad row into the child, and `USER` suspends nothing. Catalog record **v12**, one byte, because PostgreSQL's `tgenabled` is stored and a flag beside the connection would leave a second client enforcing what the first turned off. `DROP … CASCADE` drops the **constraint** and keeps the child, every constraint that names the parent and not just the first; `RESTRICT` is the default written out and refuses identically. `pg_class` grows `relhastriggers`, `t` for **either side** of a foreign key because a foreign key is two triggers. **35 captured statements, every row byte-identical**; the five declared divergences are the standing `name`/`"char"` vs `text` catalog trade and nothing else. One divergence **found and not fixed here**: a multi-statement simple Query is not all-or-nothing (§6), which is the session's loop and not a DDL statement. 8 tests | this commit |
| boot 17 — `i.indkey` as an `ANY` operand | **done**, and it moves the boot counter **26 → 27** and unblocks the ladder's rung 3: `SELECT a.attname … a.attnum = ANY(i.indkey) … ORDER BY array_position(i.indkey, a.attnum)` is how `ActiveRecord::primary_keys` reads a key. `= ANY` was expanded into an `IN` list at **plan** time, which works for `ARRAY[1,2]` and `current_schemas(false)` and can never work for a column, so `plan::Expr::AnyArray` is the value form — sharing `IN`'s three-valued rule rather than copying it. **An array is text here, not a `Datum` variant and not a `ColumnType`**: `Datum` is the row codec's type in `esker-keys`, so a variant would need arms in `esker_keys::row` and the columnar decoder for a value that can never be stored, and a `ColumnType` would put an array row in `pg_type` advertising a column `CREATE TABLE t (a int[])` still refuses. `int2vectorout`'s text *is* the value, `indkey::text` was already byte-identical, and `ActiveRecord` reads the column with `String#split(" ")`. Five operators over it (`array_position`, `array_lower`, `array_upper`, `array_length`, `cardinality`). The fact that would have shipped wrong: **`int2vector` is 0-based** — `array_position(indkey, <first column>)` is `0` where `array_position('{a,b,c}'::text[], 'a')` is `1` — and an off-by-one there **sorts identically**, so boot 17 passes either way and every caller that reads the number is wrong. **17 captured statements, every row byte-identical**; the five declared divergences are the standing `name`/`int2vector` → `text` catalog trade. 4 tests | this commit |
| boot 22 — `current_schemas` outside an `ANY` | **done**, boot **27 → 28**. The function has been here since rung 4, but only where the lowering could expand it into an `IN` list; written on its own it was `0A000`, because the answer is an array. It is the boot-17 array in its other spelling, so the unit is the writer — `vector::Array::write`, `array_out`'s quoting rule measured (`{a,b}` but `{"a b","c,d"}`, `{NULL,"NULL"}`, `{""}`) — plus one lowering arm. The `ANY` form still expands where it is lowered, which is what keeps the plan every catalog query has. **This array is 1-based where `indkey` is 0-based**, and the two are asserted together because the rule is the contrast. One older gap fixed beside it: `current_schemas(1)` was `0A000` where PostgreSQL says `42883 function current_schemas(integer) does not exist` — resolution is by name *and* argument types, and the wrong-type DETAIL differs from the wrong-arity one. **13 captured statements, every row byte-identical.** Three entries left `tests/current_schemas.rs`'s divergence lists and two were rewritten: the reason under `SELECT current_schemas(false)` is no longer "no array value" but "the second read follows a `SET search_path` this node refuses", which is a different claim and had to be re-stated. 3 tests | this commit |
| boot 23/30 — `pg_extension` and `pg_inherits` | **done**, boot **28 → 30** in one commit: two relations `ActiveRecord` reads on every boot and this node did not have at all, so both were `42P01` — which makes the adapter raise where no rows makes it carry on. Both are empty, and the two emptinesses are not the same claim. `pg_inherits` is empty on a **real server too**, and completely so here: there is no `INHERITS` and `PARTITION BY` is `0A000`. `pg_extension` is **not** empty on a real server — every PostgreSQL database has `plpgsql` — so boot 23 is a **declared divergence**, three lines of it, on the argument that already keeps `pg_collation` and `pg_range` empty: a row here would tell a client `CREATE FUNCTION … LANGUAGE plpgsql` will work. `ActiveRecord` turns the answer into `enable_extension` lines in a schema dump, and none is the truth. Exactly the two columns each is read by, and a third is `42703`. **9 captured statements, 6 byte-identical**; 3 tests | this commit |
| boot 26 — `array_agg` and its `ORDER BY` | **done**, boot **30 → 31**. The enum load wanted three things at once and got all three: the relation `pg_enum` (empty, and completely so — `CREATE TYPE … AS ENUM` is `0A000`), the aggregate `array_agg`, and an **`ORDER BY` inside an aggregate's parentheses**, which was refused by name along with every other aggregate clause. `array_agg` is the first aggregate here that is **not a fold** — it keeps every value, so its state is the group's size and it is bounded by the same `GROUP_LIMIT` the group table is. Three facts measured that reasoning gets wrong: over **no rows it is NULL, not `{}`** (`count` is 0 and an empty array `IS NULL` is false — three distinguishable answers); it **keeps NULLs** where every other aggregate skips them, so a dropped NULL silently shortens the array; and its `ORDER BY` sorts by expressions it does not return, so the key travels with the value. One bug found by the tests and not the corpus: the spec lookup matched on `func`/`distinct`/`arg` and **not** the clause, so `array_agg(n ORDER BY n)` and `array_agg(n ORDER BY n DESC)` in one statement collapsed onto one accumulator and answered the same array twice — the clause is now part of the aggregate's identity. `pg_type` grew `typnamespace` (last, for the `SELECT *` order rule). **22 captured statements, every row byte-identical**; the 14 declared divergences are the array's declared type. 4 tests | this commit |
| boot 29/31/32 — the `pg_catalog.` qualifier on a function | **done**, boot **31 → 33** — three statements for one rule, and **no new feature**. Every function in them was already built and answering; what stopped them was that the name a call resolves by was taken with the schema still attached, so `pg_catalog.obj_description(…)` was `0A000` naming a function that exists. `relation_name` has had this rule since rung 4 (`pg_catalog.pg_class` is `pg_class`); this is that rule for the other kind of qualified name. **The qualifier is checked, not stripped**: `public.obj_description(…)` is `42883` on a real server, so dropping whatever schema was written would answer where a real server raises. And PostgreSQL sends **no `DETAIL`** for it — its other two `42883`s describe the candidates they nearly matched, and a schema that holds nothing by that name has none — which is a third error variant rather than a reused one. **12 captured statements, 11 byte-identical**; the one divergence is `length`, a function this node does not have, and it is in the corpus because it proves the strip happens *before* resolution: the refusal names `length`. 4 tests | this commit |
| boot 36 (part) — `<oid>::regclass` | **done**, and it does **not** move the counter: it is one of boot 36's five pieces, and `foreign_keys()` also wants `generate_subscripts`, `c.conkey[idx]`, a derived table and an `array_agg` in a correlated subquery. The inverse of `'name'::regclass`, and unlike its inverse a **per-row** call, because its argument is a column. Two facts measured: an oid that names nothing **prints the number back** rather than raising — a `LEFT JOIN` with no match would break on a node that refused — and oid **0 prints `-`**, PostgreSQL's `InvalidOid`, which is one character and not the empty string (it cannot be a corpus row, because a lone `-` is that format's marker for *no rows*). **One pre-existing bug fixed and one pre-existing divergence pinned**, both found by this unit's tests: the binder's walker did not descend into a cast, a scalar function, a `CASE`, an `= ANY` or an aggregate's arguments, so a `::regclass` inside any of them reached the row evaluator unresolved **and a `$1` inside any of them was never substituted**; and the *forward* `'x'::regclass::text` prints the oid where a real server prints the name, which is now a test that asserts the difference rather than a surprise. 8 captured statements, every row byte-identical; 5 tests | this commit |
| boot 35/36 (part) — the array subscript `a[i]` | **done**, and it moves no counter: the second of the pieces both statements need. Two facts measured: **the subscript is absolute, not an offset**, so it follows the array's own lower bound — `indkey[0]` is an index's first column and `conkey[1]` is a constraint's, one written number meaning different positions in the two array-ish types the catalog holds; and **every way of missing is NULL and none is an error** (out of range at either end, an empty array, a NULL array, a NULL subscript). The element takes its type from the comparison it meets, the same rule an `= ANY`'s elements follow — `a.attnum = d.indkey[0]` is an `int2` column against a subscript, and comparing the element as text would find nothing and report an **empty join** rather than an error. One pre-existing gap found and declared rather than worked around: **`pg_constraint.conkey` is filled only for a foreign key here**, where a real server fills it for every constraint with columns, so a primary key's is `{1}` there and NULL here — the shape `unique_constraints()` will want. **16 captured statements, 15 byte-identical**; 4 tests | this commit |
| 703 — `CREATE EXTENSION` | **done**: statement 703 of `postgresql_specific_schema.rb` and rung 4's stopper. Three outcomes and the clause changes only one: **not available** is `0A000 extension "x" is not available` with PostgreSQL's own HINT, **with or without `IF NOT EXISTS`** — measured, both spellings, because the clause is about existence and this is about availability; **already installed** is `42710` without it and a `NOTICE` with it; otherwise it is recorded at the `default_version` the build offers. Catalog record kind `x`, keyed by name, with a **floor of its own** (`OLDEST_EXTENSION_VERSION` = 12) rather than the current catalog version, so a reader newer than 12 still accepts what 12 wrote — asserted by a golden and by a hand-built version-12 record. **The allowlist is a decision, not a measurement** (user ruling): `uuid-ossp` and `pgcrypto`, exactly what the schema needs in order to load, plus `plpgsql` pre-installed as every PostgreSQL database has it. `hstore` came **off** the list another lane had put it on — an entry there tells a client this server has something, and `hstore` brings a type this node lacks; three of that lane's corpus rows became declared divergences and its available-but-not-installed test moved to `pgcrypto`. **Installing does not bring the functions**: `uuid_generate_v4()` is still `42883`, declared, and the statement's job is to let the schema *load*. The two extension views now agree — they were one fact read two ways and one of them was a constant. **14 captured statements, 13 byte-identical**; 4 tests | this commit |
| 703 (cont.) — `gen_random_uuid` and `uuid_generate_v4` | **done**, and it is what `CREATE EXTENSION` was missing: the allowlist means the extension **and the functions it promises**, because the schema's next lines default columns to them — an extension that installs and then answers `42883` stops the same files one statement later. **One is core and one is gated**: `gen_random_uuid` has been in PostgreSQL core since 13 and answers with `pg_extension` empty of both names, measured; `uuid_generate_v4` is `uuid-ossp`'s and is `42883` until it is installed and again the moment that install rolls back — checked at **execution**, because availability is transaction state. The value is a real **v4**: sixteen bytes from `/dev/urandom` with the version nibble set to 4 and the variant bits to `10`, in `std` alone with **no crate** — `getrandom(2)` would need `unsafe` and a banned `*-sys` binding, and the same pool is reachable through a file `std` already opens. A formatter that only hyphenated random bytes passes a length check and fails both nibbles, so both are asserted over 256 draws. **A fourth `42883` DETAIL**, measured: `There is no function of that name.` — the name is unknown at any arity, which is neither the wrong-arity form, nor the wrong-types form, nor the schema-qualified form that carries no DETAIL. The `uuid_generate_v4`-is-`42883` divergence declared one commit earlier is **deleted**. 9 captured statements, every row byte-identical; 4 tests | this commit |
| 710 — `DEFAULT gen_random_uuid()` | **done**: the stopper after the functions landed, and 4 of the 12 remaining refusals. The functions existed and the **default path** still refused them — a volatile default has been `0A000` since `0e8680c` with `CURRENT_TIMESTAMP` the one exception. The mechanism was already right and only the set widened: a default that cannot be folded is recorded as **which** volatile function it is and evaluated per row. `ColumnDef::default_now` became `volatile_default: Option<VolatileDefault>`, and the record's version-5 **bool byte widened into a tag** — `0` and `1` are exactly the two values the bool held, so every record ever written reads back unchanged, and the bump to **v13** is what stops an older reader taking a `2` for `true` and defaulting a `uuid` column to a timestamp. **Per row is the point**: `count(DISTINCT id) = count(*)` over sixteen inserts and over one three-row `INSERT`, where a default folded once would give every row the same UUID and refuse the second insert into statement 710's primary key. It prints unparenthesised and uncast, and `information_schema.columns.column_default` carries it. `uuid_generate_v4()` as a default is refused **when the table is created**, not at the first insert, the way a real server resolves the expression then. **13 captured statements, every row byte-identical**; 4 tests | this commit |
| 738 — `GENERATED ALWAYS AS (expr) STORED` | **done** as a clause; the suite's own column is **not** unblocked and that is stated rather than hidden — `virtual_stored_number integer GENERATED ALWAYS AS (random_number * 10) STORED` needs integer `*`, which this node has no operator for at all, so 738 stops on the operator and not on the clause. The clause itself is exact against an expression this node can compute. Catalog record **v13**, one string per column, appended after version 12's byte — writing it *before* put the triggers byte where the decoder expected a string length, read a generation expression onto the wrong column and refused an ordinary `INSERT`; the rule is now a comment at the site. **Two sentences under one SQLSTATE**: `428C9 cannot insert a non-DEFAULT value into column "x"` for an `INSERT` and `428C9 column "x" can only be updated to DEFAULT` for an `UPDATE`, same `DETAIL`, and an implementation reusing one is wrong half the time. `DEFAULT` is accepted by both and **recomputes**. It is a function of the **row**, filled on every write, so an `UPDATE` that moves its source moves it. `attgenerated` is `s`; the expression is in `pg_attrdef` while `information_schema.columns.column_default` is **NULL** for the same column and the new `generation_expression` carries it — one row, two views, opposite readings. A **C1 gap** registered: `VIRTUAL` is a syntax error here where PostgreSQL 19 takes it (`sqlparser` 0.62.0), and the lowering refuses it by name for the day the parser can read it. **17 captured statements, every row byte-identical**; 4 tests | this commit |
| 738 (cont.) — every `DEFAULT` is an expression | **done**, and it deletes two rules this node had invented rather than adding a feature. A `DEFAULT` was refused as *possibly volatile* if it was a function call and as *not a constant* otherwise, which between them refused **seven of statement 738's ten defaults** that PostgreSQL 19 accepts — `concat` and `CURRENT_DATE` among them, both `STABLE`. **PostgreSQL applies no volatility test and no constant test at all**: a default is an arbitrary expression stored as a parse tree and evaluated once per row, and what it forbids is exactly three things, now refused with its own messages — a **column reference** (`cannot use column reference in DEFAULT expression`, and it recurses: `DEFAULT concat(a, 'x')` is the same message), a **subquery**, and a **set-returning function**. All three are `0A000`, which is the surprising part: they read like syntax errors. An unknown function is not a fourth rule — it is refused when the **table is created**, where a real server resolves the expression too. The tag over three functions became `ColumnDef::default_expr: Option<String>`, catalog record **v14** appended after v13's section, and the old tags decode to the text they stood for. **The spelling now survives**, which the tag could not express: `DEFAULT CURRENT_TIMESTAMP` prints `CURRENT_TIMESTAMP` and `DEFAULT now()` prints `now()` — a declared divergence, deleted. Folding stops where PostgreSQL's does: a literal folds, `(1 + 1)` does not, and a real server prints it back unevaluated. Four functions added — `random()`, `concat()` (variadic, **skipping NULLs**, `42883` at zero arguments), `convert_to()`, and the clock keywords as ordinary expressions. **`ADD COLUMN` is the one place nothing widened**: PostgreSQL rewrites the table so every existing row gets its own value, this `ALTER` is defined not to, and leaving them NULL would be a wrong answer — refused, named for the rewrite. **Two pre-existing bugs found by the capture**: a `date` default printed `'2004-01-01'::numeric`, because the bare-number test read the *characters* rather than the type and a date is digits and dashes; and there was **no float-against-integer comparison at any width**, so `random() >= 0` fell through to variant rank and answered `f`. Comparing the two **exactly** rather than by widening the integer deleted two more declared divergences — `1 IN (1.0)` and `1 = 1.0` — including one recorded as unfixable beside the counterexample that made widening wrong. **31 captured statements, 25 byte-identical**; 4 tests | this commit |
| 753 — `DROP SEQUENCE` | **done**: the suite's own line is `DROP SEQUENCE IF EXISTS companies_nonstd_seq CASCADE` against a name that is not there, which is a plain success — and the family around it is where the answers are. **`CASCADE` drops the column's *default*, not the column**: the `bigserial` column survives, still `NOT NULL`, with nothing to fill it, so the next ordinary `INSERT` is `23502` naming a column the user never mentioned; `pg_attrdef` loses its row and `pg_attribute` keeps its own. Without `CASCADE` a sequence a default depends on is **`2BP01`** with a `DETAIL` naming the **column and its table**, not `42P01` and not a silent success. A table dropped with the wrong verb is **`42809` "is not a sequence"**, with a HINT naming `DROP TABLE` — the relation was found and is the wrong kind, and reporting it missing would send a user after the wrong bug. A list is **all-or-nothing**, and **existence is checked for every name before any dependency**: `DROP SEQUENCE a, b` with `b` absent answers `42P01` about `b`, not `2BP01` about `a`. **Three bugs in the drafted implementation, all found by the corpus**: PostgreSQL's `DETAIL` sentence had been written into `hint()`, so it printed after `HINT:` and hid the real hint behind it; the drop deleted the sequence records without rewriting the table record, so every node kept a cached `TableDef` that still said the column had a sequence and the next `INSERT` filled it from a counter that was gone — reported success where the capture wanted `23502`; and the dependency check ran as the loop walked the list, which named the wrong sequence and would have dropped the first before failing on the second. **18 captured statements, 16 byte-identical**; 6 tests | this commit |
| 754 — `CREATE SEQUENCE … OWNED BY` | **done**: `CREATE SEQUENCE companies_nonstd_seq START 101 OWNED BY companies.id`, against a table whose `id` is already a `bigserial`. **`OWNED BY` is not a default**, and that is the fact the statement turns on: it says the sequence goes when the *column* does, which points the opposite way, and `pg_attrdef` still shows one row — the original `bigserial`'s — after the new sequence exists. A node that wired the default here would answer the suite's **next** statement before it was asked and would look right until something checked which sequence the rows came from. **A column can own more than one sequence**, which is why the record was re-keyed by the sequence's own id one commit earlier. **`START n` is the first value handed out**, not the one before it: `nextval` answers `101` then `102`, so the counter is stored *at* `n` and a node storing `n - 1` would be one short for every sequence anyone gave a `START`. A name collides against the **whole** relation namespace (`42P07`, and over a table's name too), `IF NOT EXISTS` covers it, and `OWNED BY` checks **both halves with different codes** — `42P01` for the table, `42703` for the column, which spells the relation it looked in. `MINVALUE`, `MAXVALUE`, `CYCLE`, `CACHE` and `AS <type>` are refused by name: each changes what happens at an end this counter does not have, and one stored-and-ignored would run past the limit the user asked for. **One bug found by the corpus**: `catalog::create_sequence` never bumped the catalog version, and a sequence no column owns writes no table record to bump it either — so `nextval` on the sequence just created answered `42P01`, and `IF NOT EXISTS` reported `42P07` because the name lookup missed the cache and the raw write caught it. Two `DROP SEQUENCE` divergences deleted, exactly as that commit predicted. **25 captured statements, 20 byte-identical**; 5 tests | this commit |
| 755 — `ALTER COLUMN … SET DEFAULT` | **done**, and it clears two refusals at once. **`SET DEFAULT nextval(…)` replaces the `bigserial`'s own default; it does not add one** — `pg_attrdef` holds one row for the column before and after, `nextval('sd_id_seq'::regclass)` then `nextval('sdseq'::regclass)`, and the next `INSERT` draws `101` from the new counter, which is what proves the swap rather than the rendering. **That replacement is what frees the original sequence**: `DROP SEQUENCE sd_id_seq` is `2BP01` while it is the column's default and a plain success one statement later with no `CASCADE`. A node that added the new sequence beside the old would leave the column drawing from two counters and would answer `2BP01` on the suite's next line — PostgreSQL's own correct answer to a state it should not be in, which is the hardest kind of wrong to notice. The sequence arm is matched at lowering rather than lowered as a call, because a sequence **is** a column's default in this catalog; a `nextval` treated as an ordinary expression would evaluate a second counter beside the first. A constant, an expression (`concat('x','y')` — the generalised `DEFAULT` carries it), and `DROP DEFAULT` all work; `DROP DEFAULT` is **idempotent** and leaves NULL, not the value it had been defaulting to. Four refusals, four codes: `42703` column, `42P01` table, `42P01` for a `nextval` naming nothing, `22P02` for a literal the type will not take, and the same `0A000` column-reference rule every other default gets. Folding happens in the **executor**, not the lowering, because a plan is built without the catalog and `7` needs the column's type — a new seam, `parse::fold_column_default`, reaching the same function `CREATE TABLE` uses. **26 captured statements, every row byte-identical**; 4 tests | this commit |
| 757 — `DROP FUNCTION` | **done**, and it is *not* a no-op even though nothing here creates a function. That is the finding: a **built-in** is `2BP01 cannot drop function lower(text) because it is required by the database system`, and **`IF EXISTS` does not cover it** — the clause covers absence, not protection, so a node answering success to everything would let a schema drop `lower` and report that it had. This node has no `CREATE FUNCTION`, so the whole function namespace is a fixed list, which is exactly what makes the statement implementable rather than stubbed. **The message prints the function's own canonical signature, not what the user wrote**: `concat(VARIADIC "any")` comes back as `concat("any")` and `convert_to(text, name)` as `convert_to(text,name)` — the `VARIADIC` gone and the space after the comma with it. **The signature decides, not the name**: `lower(integer)` is `42883` where `lower(text)` is `2BP01`, and `concat(text)` is `42883` because the real signature is `concat("any")`. **No argument list is a different statement, not a shorthand** — it selects *the* function of that name, and its absence sentence is its own (`could not find a function named "x"`, not `function x() does not exist`), because with no list there is no signature to name; a fourth error variant exists solely so the `DROP` form carries **no `DETAIL`** where the call form carries one. A test walks every callable function name and asserts it is protected, so a function added to the language and forgotten in the list becomes a failure rather than a `DROP FUNCTION` reporting success for something still callable. Three divergences, all from PostgreSQL having a **larger** catalog: `pg_proc`, and `lower` being overloaded there (`42725`) where one `lower` here resolves. **25 captured statements, 22 byte-identical**; 6 tests | this commit |
| 762 — `CREATE TABLE … INHERITS` | **done**, and the finding is that it is not a DDL feature: **inheritance is a read and a write rule**. `SELECT … FROM parent` returns a child's rows, `UPDATE parent SET …` changes them and `DELETE FROM parent` removes them — measured, all three. A node that copied the columns and stopped would answer `1` where a real server answers `2`, which is a wrong answer rather than a missing feature, so the DDL could not land without the scans. The scan carries the children's ranges and reads them after its own, decoding each with the **child's** schema and lifting the values by a map built from **names** — a child with no primary key carries an internal row id the parent has not, so every inherited column sits one position later. `UPDATE` and `DELETE` cannot use that one scan: each row has to go back through its own table's key and indexes, so they fan out over the hierarchy and act on each relation alone, breadth first, composing the projection so a *grandchild*'s row lands in the named table's columns. **The child draws from the parent's sequence**, not a copy: `ic`'s `id` defaults to `nextval('ip_id_seq'::regclass)`, so rows inserted through either table cannot collide — the record stays the parent's and only the in-memory list gains an entry. It inherits columns, `NOT NULL` and defaults; it does **not** inherit indexes or the primary key, so the uniqueness the parent promises does not hold across the pair, and `pg_index` for the child is empty on both servers. A redeclared column **merges** rather than duplicating, and only a type clash stops it — `42804`, with **two** `DETAIL` lines. Dropping the parent is `2BP01` naming the child; `CASCADE` takes the child table, unlike the foreign-key case where it takes only the constraint. Catalog record **v16**: the edge is stored from both ends, because a scan needs the children and `pg_inherits` needs the parents in order. **Statement 762 still does not load and this says so**: its message is five statements including `CREATE OR REPLACE FUNCTION … LANGUAGE plpgsql` and `CREATE TRIGGER`, neither of which exists here, and a multi-statement message is all-or-nothing. A **C1 gap registered**: `FROM ONLY t` is not in `sqlparser` 0.62.0's `FROM` clause, so it parses as a relation named `only` and answers `42P01` — an error rather than a wrong answer, which is the property that matters. **25 captured statements, 23 byte-identical**; 6 tests | this commit |
| 779 — inline `UNIQUE`, `NULLS NOT DISTINCT`, `DEFERRABLE` | **done for three of the statement's four constraints**, and the capture is what splits them. **`DEFERRABLE INITIALLY IMMEDIATE` is not deferred at all** — it refuses the second of two colliding rows at the statement that writes it, exactly as a plain `UNIQUE` does, and all that differs is `condeferrable` and what `pg_get_constraintdef` prints, which keeps `DEFERRABLE` and drops the `INITIALLY IMMEDIATE` half. **`INITIALLY DEFERRED` really waits**: both rows go in and `COMMIT` raises the `23505`, rolling the whole transaction back — so it is refused by name, because checking it at the statement would refuse a transaction a real server commits. **`NULLS NOT DISTINCT` means the table holds at most one row with a NULL there**, and the consequence is the trap: an `INSERT` that never *mentions* the column still collides, since omitting it writes a NULL — `Key (position_4)=(null) already exists`, lower-case and parenthesised. `pg_constraint` gained **`contype = 'u'`**, the open question the e2-catalog lane left: a unique constraint *is* its index here, so the row is derived the way a primary key's is — and `IndexDef::constraint` is a **three-state** field because `CREATE UNIQUE INDEX` builds an identical index and gets no row, measured (the count stays at one). Catalog record **v17**, one byte per index. The clause sits on **opposite sides** in the two renderings: `UNIQUE NULLS NOT DISTINCT (c)` from a constraint, `… USING btree (c) NULLS NOT DISTINCT` from its index. A **C1 gap** registered: the *column-option* spelling `a integer UNIQUE NULLS NOT DISTINCT` is unparseable by `sqlparser` 0.62.0 and is named by the construct recognizer; the table-constraint form `ActiveRecord` writes parses and runs. **23 captured statements, 19 byte-identical**; 5 tests | this commit |
| `LIKE` / `NOT LIKE` / `ILIKE` | **done**, and it is not a shape any single suite statement needs — it is what the *captures* are written with, and its absence was costing far more than the operator. A refusal at one `relname LIKE 'measurements%'` probe **aborted the partition capture's transaction and took thirty-eight statements after it**, which would have meant forty declared divergences for one missing operator; the trigger capture reads `pg_get_functiondef(…) LIKE '%LANGUAGE plpgsql%'` too. Its own `plan::Expr` variant rather than a `BinaryOp`, because it is **not a comparison**: the sides are a subject and a *pattern*, `pg_cmp` says nothing about them, and the operator carries two modifiers a binary op has nowhere to put. The matcher is **iterative with one backtrack point** rather than recursive — a pattern is user input and `%a%a%a%…` is the shape that turns the natural recursion into a stack overflow, which `CLAUDE.md`'s never-panic rule forbids. Measured: `\` escapes `%` and `_` with **no `ESCAPE` clause written**, and `ESCAPE '#'` **replaces** the backslash rather than adding to it; a NULL on either side is NULL and the negation does not rescue it; it is case-sensitive and `ILIKE` is the same matcher with both sides folded; and a non-text operand is `42883 operator does not exist: integer ~~ unknown` — `~~` being `LIKE`'s internal name — rather than a cast. One divergence, and it is the standing literal-width one: `bigint ~~ unknown` here against `integer ~~ unknown` there. **12 captured statements, 11 byte-identical**; 5 tests | this commit |
| 762 + 790 — `CREATE FUNCTION` and `CREATE TRIGGER`, define-only | **done, and this is the one that unblocks the suite**: r1's run 38 measured 271 of 271 stopped files stopping on the same `CREATE OR REPLACE …` line, and the load reaches it at **762** — the `INHERITS` block embeds a `CREATE OR REPLACE FUNCTION` of its own — before 790 is ever read. The capture settles the scope and it is the user's essential-only ruling applied: **the load only defines them**, `count(*)` is `0` immediately after the whole statement runs, and exactly one test in the suite (`persistence_test.rb:1708`) ever fires a trigger. So both are **catalog objects** — stored with the dollar-quoted body verbatim, listed in `pg_proc` and `pg_trigger`, droppable — and **never executed**. **`EXECUTE PROCEDURE` and `EXECUTE FUNCTION` are one clause**: 762 writes the first, 790 the second, both parse, and `pg_get_triggerdef` prints only the second, so the text out is not the text in. The multi-statement message survives the **semicolons inside `$$…$$`** because nothing splits on them — `parse_statements` is the parser, not a scanner. `tgenabled` is a **letter** (`O`/`D`) and `tgtype` the bitmask `7` for BEFORE+ROW+INSERT, neither of which is the word in the DDL. `OR REPLACE` exists for the function and **not** for the trigger: the function twice is a plain success, a second trigger of one name is `42710`. `pg_language` reports `plpgsql` as trusted — a name, not a runtime, and the first thing a client asks before writing one. Refusals: `42704` for an unknown language, `42883` for a trigger naming a function that is not there, `2BP01` for dropping a function a trigger still holds. Catalog record **v18** (triggers on the table) plus a new record kind for functions, with its own version floor. **Two harness improvements this unit forced**, both general: an empty declared-types column now means *undeclared* rather than *no result set*, which is how a session capture records a query over a rolled-back table; and a `25P02` cascade is attributed to the divergence that caused it rather than counted as forty-one of its own. **58 captured statements, 56 byte-identical**; 8 tests | this commit |
| 781–786 — declarative partitioning (`PARTITION BY LIST`, `PARTITION OF`) | **done**, and it arrived as a stash another lane left at 90%: recovered onto `main` over two commits it had never seen, which is where the first finding was — **both lanes had claimed catalog record version 18**, so the partition key was written at 18 and read back by a reader that required 19, and every partitioned table silently lost its key on the round trip. Renumbered to **v19** as the seventh append-only section; a v18 record still decodes with no key and no bound. The feature itself is a routing rule more than a DDL one: `INSERT` through the parent picks the partition whose bound admits the row and writes it there under that table's own key and indexes, an `UPDATE` that changes the key **moves** the row rather than failing, and a partitioned table stores nothing itself. `DEFAULT` is tried **last** whatever order the partitions were declared in. Two different `23514`s: through the parent it is `no partition of relation "m" found for row` naming the *parent* — there is no partition to name — and straight into the wrong partition it is `new row for relation "m_t" violates partition constraint`. **The bound is coerced to the key's type and printed back quoted**: the suite writes the integer `1` against a `character varying` key and `pg_get_expr(relpartbound, oid)` answers `FOR VALUES IN ('1')`, so storing the literal as written would diverge the moment `ActiveRecord` dumps the schema. **Four `relkind`s in one feature and one is a capital letter** — `p` partitioned table, `r` partition, `I` partitioned index, `i` the partition's own — and a `relkind IN ('r','p')` filter that forgets `I` passes every test that never makes a partitioned index. Statement 782 creates the index before a single partition exists and **every partition made afterwards silently gets its own child index**, `<partition>_<cols>_idx`, which is also what a duplicate-key error names. A unique index on a partitioned table must contain every partition column (`0A000`, PostgreSQL's own two sentences), an overlapping bound is `42P17` naming the partition it would overlap, and `pg_get_indexdef` says **`ON ONLY public.measurements`** for the index that covers every partition. `pg_partitioned_table` reports `partstrat` as the one-letter `l` while `pg_get_partkeydef` says the word `LIST`, and the two are read from the same field. Four divergences, none of them about partitioning: `tableoid` is a **system column** and this node has none of the six (each fact it proves is asserted directly instead); `ADD CONSTRAINT … PRIMARY KEY` is an action that has never been built here, so both servers refuse and only the sentence differs; and `ATTACH`/`DETACH PARTITION` are a **C1 parser gap** — `sqlparser` 0.62.0 has only ClickHouse's `ATTACH PARTITION <expr>`, with nowhere to put `FOR VALUES`, and neither spelling is in `postgresql_specific_schema.rb`. **69 captured statements, 65 byte-identical**; 7 tests | this commit |
| 781–786 (cont.) — `PARTITION BY RANGE` | **done**, and the finding is in the *record* rather than the routing: a bound was stored as text and read back as text, so an `int4` `10` and an `int4` `10` compared unequal and the second of the capture's two range partitions was refused as **overlapping the first**. A bound's type is the key column's, which belongs to the parent, and the parent is not loaded where a record is decoded — so the value now carries its own **type tag**, which is what makes the record self-describing. The `LIST` capture hid this because its key is `character varying` and text really was the type. Measured: a `RANGE` bound is **half-open**, `FROM (MINVALUE) TO (10)` takes `9` and `FROM (10) TO (MAXVALUE)` takes `10` — the two share the number and do not overlap — and `MINVALUE`/`MAXVALUE` print back as the **words**, not as the extremes of the key's type. And the bound prints as a literal **of the key's type**: `10` bare on an `int4` key where the suite's `LIST` bound is `'1'` quoted on a `character varying` one, which is one printer and not two. A second bug the range key found: the direct-insert bound check read the key columns **positionally**, which is right only when the key is the table's first column — `mp_range`'s key is its second, and a partition may carry an internal row id the parent has not, so the columns are found **by name** the way `ChildScan` finds them. Multi-column `RANGE` bounds are refused by name: PostgreSQL's rule past one column is not the obvious one — `MINVALUE` in a position makes every column after it unbounded — and nothing captured it. `HASH` likewise. 4 tests | this commit |
| 787 — `CREATE [UNIQUE] INDEX … INCLUDE (…)` | **done**. The fact that would have shipped wrong: **the included columns are in `indkey`, and only a count separates them from the key** — the suite's index is `indnatts = 4`, `indnkeyatts = 2`, `indkey = "2 3 4 5"`, one vector holding both halves — and `ActiveRecord`'s schema dumper reads `indkey`, so an implementation that recorded the payload anywhere else reports a four-column index. `indnkeyatts` is new in `pg_index` and sits where PostgreSQL puts it, straight after `indnatts`; both are **`smallint`**, measured, not `integer`. **Uniqueness is over the key columns only**: `("firm_id") INCLUDE ("name")` refuses a second row with the same `firm_id` and a *different* `name`, and the `DETAIL` names `Key (firm_id)=(1)` alone — the payload is recorded and never compared, which fell out of building entries from `keys` and is asserted rather than assumed. **An included column may repeat a key column**: `("firm_id") INCLUDE ("firm_id")` is accepted, `indkey = "2 2"`, not deduplicated. `pg_get_indexdef` puts `INCLUDE (…)` **after the key list and before the predicate**, and `pg_indexes.indexdef` carries the same string — which it does here by *being* the same string: the view is new and its `indexdef` is `pg_get_indexdef`, so the two cannot drift. The index relation gains `pg_attribute` rows for its payload too, which is what makes `a.attnum <= x.indnkeyatts` answer four rows rather than two. Refusals in PostgreSQL's own words: `42703 column "nosuchcol" does not exist` — the **plain** wording, not the `named in key` phrasing a key column gets — and `0A000 access method "hash" does not support included columns`, which is checked **before** this node's own `USING` refusal because `amcaninclude` is a property of the method and PostgreSQL complains about the payload first (measured for `hash` and `brin`). Catalog record: the `INCLUDE` list per index is the **second half of v19**, appended after the partitioning section — two sections under one number because they arrive in one release, and a number a reader can never branch on is a number spent for nothing (g1's `EXCLUDE` takes v20). Three C1 parser gaps, all declared: `INCLUDE (c DESC)` and `INCLUDE (c opclass)` are `42P17` on a real server and *syntax* errors here — `sqlparser` 0.62.0 types the clause as `Vec<Ident>`, with nowhere to put either — and the **constraint** spelling `UNIQUE (c) INCLUDE (d)` has no field on `UniqueConstraint` at all, on top of an `ADD CONSTRAINT ... UNIQUE` action this node has never had. **45 captured statements, 40 byte-identical**; 6 tests | this commit |
| What the suite *does* with a partitioned table — and **the schema-load stopper** | **done**, and the first line is the one that mattered: r1's run 44 had **27 of the first 28 files** failing on `cannot drop table measurements because other objects depend on it`. `create_table(:measurements, force: true)` sends `DROP TABLE IF EXISTS "measurements"` on every reload, the previous load's partitions are still there, and this node answered `INHERITS`'s `2BP01`. **PostgreSQL drops a partitioned parent together with its partitions, with no `CASCADE`** — measured, `0` relations match `measurements%` afterwards, and `CASCADE` changes nothing about it. `INHERITS` is the **opposite in the same session** (`2BP01`, `DETAIL: table ic depends on table ip`), so the two share `pg_inherits` and an edge and one rule for both is wrong for one of them. Captured by grepping `activerecord-8.1.3.1`'s own suite for what it sends after the load: `partitions_test.rb`, `schema_test.rb`'s partitioned index and its three dumper tests, `insert_all_test.rb:766`'s `upsert_all`, and every catalog query copied out of `schema_statements.rb` **with its `relkind` filters intact**. Three more findings, each its own bug: **`DROP INDEX` on the partitioned index takes every partition's own copy with it** (zero relations match `%logdate_city_id%` afterwards, and the copies are matched to their parent's by key column **names**, because the ordinals are different tables'); **a partition inherits the parent's `PRIMARY KEY`**, and without it declared no key at all, took an internal row id the parent had not, and a routed row was one value short of the partition's width — `XX000 a row of 2 values does not fit a table of 3 columns` at the first `INSERT`; and the partition's key is **its own relation**, `pk_part_1_pkey`, which is what a duplicate names. The inline `PRIMARY KEY (b)` on a table partitioned by `a` is the unique-index refusal with the word substituted, now reachable where the `ADD CONSTRAINT` spelling still is not. Also measured and matched: **a partition's `NOT NULL` rows carry the *parent's* name** (`pk_part_a_not_null` on `pk_part_1`), which is also what puts `pk_part_1_pkey` first in name order. Two divergences: `INSERT ... ON CONFLICT`, a statement this node does not have at all — the capture settles that a real server routes the conflict through the parent and arbitrates on the partition's own index, for the day it lands — and `pg_typeof(relkind)`, the standing `"char"`→`text` trade made visible as a *row* because `pg_typeof` returns the type as a value. **81 captured statements, 79 byte-identical**; 7 tests | this commit |
| `INSERT … ON CONFLICT` — what `insert_all` and `upsert_all` compile to | **done**, and the fact that would have shipped wrong is about a **sequence**: `nextval` is drawn to build the proposed row, *before* anything knows there is a conflict, and it does not roll back. Three inserts come back `5`, `5`, `7` — the second drew `6`, conflicted, updated the row that already had `5`, and `6` is gone. `DO UPDATE SET "id"=excluded."id"` is how you see it: the row's `id` becomes a number no `INSERT` ever returned. The corollary is where the second bug was: **the conflict target is resolved before any row is built**, so a `42P10` or a `42703` draws nothing, and validating after building left the counter three higher over the capture's three probes. `RETURNING` answers only for rows the statement wrote — `DO NOTHING RETURNING "id"` on a conflicting row is **no rows at all**, not a NULL and not the existing id; `DO UPDATE … RETURNING` gives the *existing* row's. Two proposed rows that collide with each other are two answers: `21000 ON CONFLICT DO UPDATE command cannot affect row a second time` with its HINT, where `DO NOTHING` takes the pair and the **first** wins. The target names **columns and PostgreSQL infers an index**, so a list matching none is `42P10` (`invalid_column_reference`, shared with the casts — the columns exist, the inference fails), and three column mistakes have three shapes: `"nosuchcol"` quoted for the target, `excluded.nosuchcol` **qualified and unquoted** for the payload, and the target table's own name in the `SET` meaning the row already there. Both rows are in scope while the assignments run, which is one `Scope` holding the table **twice** under two names, evaluated against the two rows concatenated. Through a partitioned parent the capture settles the order: **routed first, arbitrated second** — one statement updates a row in `measurements_toronto` and inserts two into `measurements_concepcion` — routing wins over `DO NOTHING` (a row no partition takes is still `23514`, which gained the `DETAIL` it had been missing), and **`DO UPDATE` cannot move a row between partitions where a plain `UPDATE` can**: `0A000 invalid ON UPDATE specification`. Three divergences: the arbiter's `WHERE` (`ON CONFLICT (a) WHERE …`) is a **C1 parser gap** — `sqlparser` 0.62.0's `ConflictTarget::Columns` is a bare `Vec<Ident>` — which also means a partial index can never be inferred, so the bare target over one is the `42P10` both servers give; its follow-on row; and `IS NOT DISTINCT FROM`, an operator this node does not have and which belongs in `plan::Expr`, another lane's file. **84 captured statements, 81 byte-identical**; 8 tests | this commit |
| `CREATE SCHEMA` and the named-schema half of `schema_test.rb` | **captured, and the capture is the finding**: a named schema is not one feature, and **two of the three things blocking it are not schemas at all**. `schema_names` opens with `nspname !~ '^pg_.*'` — the regex non-match operator, which this node does not have in either direction, and which aborts the block and takes sixty-two statements with it, the same shape `LIKE`'s absence had. `schema_test.rb`'s `setup` then writes **`CREATE SCHEMA test_schema CREATE TABLE things (…)`** — one statement, and `sqlparser` 0.62.0 reads only the first half (`Expected: end of statement, found: CREATE`), a C1 gap, and the spelling both of its schemas use. Only behind those two is the feature itself, and the capture says what it costs: **two tables called `things` and two indexes called `a_index_things_on_name`**, one per schema, which a catalog keyed by name alone cannot hold — so it is `relnamespace` on every relation, an ordered `search_path` resolution (`TO test_schema` answers row 1 and `TO test_schema2, test_schema` answers row 2 for the *same* query), and another record-format section while two lanes are already appending. Measured besides: `current_schemas(false)` reports the path **as resolved**, so `"$user", public` answers `{public}`; three codes for three ways of naming a schema wrong (`42P06` exists, `3F000` does not — including from `CREATE TABLE nosuchschema.t`, which fails on the *schema* — and `42P01` with the schema **inside** the quotes for a relation); `DROP SCHEMA` without `CASCADE` is `2BP01` naming one dependent; and `ALTER SCHEMA … RENAME` moves every relation with it. What this node does today is pinned rather than left absent: `current_schema` and `current_schemas` are exact, a `search_path` that resolves to `public` is honoured and one that does not is `0A000` quoting it back, and a schema-qualified relation is refused by name rather than resolved to the same name in the one schema there is — a gap and not a wrong answer. **70 captured statements, 65 byte-identical**; 2 tests | this commit |
| `~`, `~*`, `!~`, `!~*` — POSIX matching, written in-house | **done**, and it is the second operator whose *absence* was the expensive thing rather than the operator: `schema_names` opens with `nspname !~ '^pg_.*'`, so `pg19_schema.txt` lost sixty-two statements behind one missing `~`, exactly as the partition capture lost thirty-eight behind `LIKE`. The whole adapter sends **four patterns** — `!~ '^pg_.*'`, `~* 'nextval|uuid_generate|gen_random_uuid'`, `!~* 'nextval'`, `~ '.'` — and between them an anchor, `.`, `*` and alternation; three of the four are on the path every model takes at boot (`pk_and_sequence_for`). **No regex crate**: `deny.toml` forbids one, so the matcher is a **Thompson NFA** compiled by an iterative parser — a pattern is user input, and both the natural recursive-descent parser over `((((…` and the natural backtracking matcher over `(a*)*b` are the panic `CLAUDE.md` forbids. Facts that would have shipped wrong: **`.` matches a newline** (POSIX's rule, and almost no engine outside POSIX does it) and `^`/`$` are **string** anchors, so `E'a\nb' ~ '^a.b$'` is true; **the empty pattern matches everything**; a pattern is a **search** until it is anchored, which makes the suite's trailing `.*` in `'^pg_.*'` do nothing at all; **a leading `]` inside a class is a literal**; and **`\a` is not `a`** — it is the alert character, so `'a' ~ '^\a$'` is **false** and escaping is not "drop the backslash". Three `2201B` messages under one code name which thing is unbalanced, and a non-text operand is `42883` with the other side reported as `unknown`. Outside the granted ERE subset and refused by name: `(?i)` and the rest of PostgreSQL's *advanced* expressions, and **back-references**, which no finite automaton can express — taking them would mean giving up the linear-time matcher that keeps a user's pattern from being a denial of service. Landing it moved `pg19_schema.txt` from sixty-two swallowed statements to three declared ones. **56 captured statements, 53 byte-identical**; 9 tests | this commit |
| A schema is a catalog object — `CREATE`/`DROP`/`ALTER SCHEMA`, `pg_namespace` | **done for the object; relations are still `public`-only.** `CREATE SCHEMA` writes a record of its own kind and `pg_namespace` reports one row per schema — `public` among them and **not** a record, the way the available extensions are a property of the build. `schema_names`, the statement that gates the whole adapter, now runs end to end (its `!~` landed the commit before). Three codes for three ways of naming a schema wrong, each its own class: `42P06 schema "x" already exists`, `3F000 schema "x" does not exist` — **not `42P01`**, because a missing *schema* and a missing *relation* are different answers — and `2BP01` for a schema with something in it, whose sentence is carried now and whose check lands with the relations. `IF NOT EXISTS` over an existing schema is a **success**; `IF EXISTS` covers absence and not dependence. `ALTER SCHEMA … RENAME TO` moves the name and refuses a name that is taken with the same `42P06` a create gets. **The `search_path` refusal stays**, deliberately: `current_schema` and `current_schemas` are folded to `public` where a statement is lowered, so accepting a path that named another schema would answer `public` where a real server answers the other one — a wrong answer, which ADR 0031 ranks worse than the refusal, and it is lifted by the unit that makes both session-aware. Catalog record **v22** for the schema record's own kind. A schema-qualified relation name is still refused by name, so a created schema is a namespace nothing is in yet — a gap, not a wrong answer. 4 tests | this commit |
| Relations **in** schemas — qualified names, `relnamespace`, `DROP SCHEMA CASCADE` | **done.** A relation's stored name became `schema ++ NUL ++ relname`, and the separator is the finding: a **dot cannot do it**, because `schema_test.rb`'s own `setup` creates `test_schema."things.table"` — so `"a.b.c"` would be either `b.c` in `a` or `c` in `a.b`. A NUL cannot appear in a PostgreSQL identifier at all, so it separates without a length prefix, which is what lets the name record's key keep its shape ("the name is the whole tail" is what the scan relies on). **A relation in `public` is stored with no separator**, so every key, every catalog row and every message about it is byte-identical to what it was — the property that made this landable at all. The NUL is on disk and a **dot** in a sentence, which is one `display_name` at the error variants rather than thirty raise sites: `42P01 relation "nosuchschema.t" does not exist`, with the schema inside the quotes. Two tables called `things` and two indexes called `a_index_things_on_name`, one per schema, now coexist and are told apart by `relnamespace` — which is the fact `schema_test.rb` is built on. **An index lives in its table's schema** whatever the statement wrote, and derived names fall out for free: `qualify(s, t) ++ "_pkey"` *is* `qualify(s, "t_pkey")`, because the separator comes first. `DROP SCHEMA` is `2BP01` naming one dependent and `CASCADE` takes the relations through the same path `DROP TABLE ... CASCADE` walks. `CREATE TABLE nosuchschema.t` is `3F000` — the *schema* is missing, its own class — where a relation missing from a schema that exists is `42P01`. **`sqlparser` cannot read `CREATE SCHEMA s CREATE TABLE t (…)`**, which is the spelling the suite uses, so it is **split** rather than rewritten — the module already rewrites two statements the parser cannot read — with each element qualified, because that is what the form means (measured: `CREATE SCHEMA sy CREATE TABLE t` puts `t` in `sy`). Three measured details the split gets right: a **semicolon ends the form** (`… CREATE TABLE t (i int); CREATE TABLE u (j int)` puts `u` in **public**); an **index element qualifies the table after `ON`**, not its own name; and an element this node cannot qualify fails the whole split rather than passing through unqualified, because a `CREATE SEQUENCE` landing in `public` would be a wrong answer where a refusal is a gap. `::regclass` takes a name as a *string*, so `'se_idx.t_i_idx'::regclass` needed its own parse — the first dot separates, `information_schema` keeps its dot, `pg_catalog` loses it. **`search_path` is still refused**, and that is the remaining half: `current_schema` and `current_schemas` are folded at lowering, so honouring a path needs them resolved in `Executor::bound` (where the session is) with the `= ANY (current_schemas(false))` expansion moved along, and an unqualified name looked for along the path in order. **113 captured statements across two corpora, 108 byte-identical**; 11 tests | this commit |
| `search_path` honoured — the namespace closed | **done**, and the two answers are the finding: **`SHOW` gives the path as *set* and `current_schemas` gives it as *resolved***, and both are read. An entry naming no schema is **dropped, not refused** — which is what makes the default `"$user", public` answer `{public}` on a server with no role schema, and why the old refusal of a non-`public` path was wrong in the first place. `current_schema()` is the first that resolves and **NULL when none do** (`{}`, not `{public}`, not an error), and a bare name then finds nothing with the **bare** name in the `42P01` even though the path is what failed. Order decides and it is the order written: `sp_b, sp_a` and `sp_a, sp_b` answer the same query two ways. `CREATE` goes to the **first entry that resolves**, not to `public`. `RESET` and `SET … TO DEFAULT` are one statement. Mechanically: the two functions stopped being folded at lowering — the value is the session's and a lowering has no session — and became `plan::Expr::CurrentSchema`, resolved once per statement in `Executor::bound` the way `::regclass` is, with the `= ANY (current_schemas(false))` expansion into an `IN` list **moved along**: it was an index-seek optimisation, and every statement that writes it reads a catalog view with no index to seek. Adding the variant broke nine matches in files this unit does not otherwise touch — the shared-enum hazard, for the third time. **`public` is byte-identical**: with the default path every answer is the one it was before schemas existed, which `the_default_path_is_the_behaviour_it_always_was` pins. `pg19_schema.txt` went from sixty-two swallowed statements to **zero** — the whole file runs — and its remaining two divergences are which dependent a `DROP SCHEMA` `DETAIL` names (a real server walks dependency entries, this walks name records) and the standing `name`→`text` trade. **45 captured statements, 44 byte-identical**; 6 tests | this commit |
| `$N` bind parameters — run 46's largest failure, 98 tests in 19 files | **done**, and the bug is one omission repeated: **the substitution walker visited five clauses of a `SELECT` and the language has eleven**. A parameter anywhere else survived substitution and reached the row evaluator, which is `42P02 there is no parameter $n` — the message run 46 counted ninety-eight times. `HAVING count(*) > $1` is `calculations_test.rb` alone, twenty of them; a join's `ON`, a derived table, a `GROUP BY` and my own `ON CONFLICT … DO UPDATE SET` are the rest. The walkers are now written **field by field** and as a *pair* — one sizes the parameter list and the other fills it, and a clause in one but not the other is a parameter counted and never filled. **Three inference gaps behind the same door**: `n IN ($1,$2,$3)` typed none of them, because walking the list as independent predicates never sees a bare `$1` — so `where(id: [1,2,3])`, on every association load, compared `integer` against `text`; a derived table's relations were not in the list the inference is given; and `count(…)` is the one aggregate whose type is fixed whatever it counts, so a parameter against it is `bigint` rather than the `text` fallback. This needed a **four-column corpus** and its own replayer: three columns have nowhere to put the values, and `sesscap.py` drives `psql`, which speaks the simple protocol and can never send a `Bind`. Two protocol rules measured and matched: too few values is `08P01 bind message supplies 0 parameters, but prepared statement "" requires 1` — a **protocol** error, not a SQL one — and a position nothing mentions is `42P18 could not determine data type of parameter $n`, **reported against the parameter you would not name** (one value to a statement using `$2` blames `$1`). Type resolution happens at `Parse` and the count at `Bind`, so the `42P18` wins when both are wrong. The simple protocol keeps `42P02`, and `Params` gained a `bound` flag because a `Bind` carrying nothing is indistinguishable from no `Bind`. Four divergences, each its own unit: `$1::int4` (a cast over a parameter, which needs the type carried on the parameter — nothing `ActiveRecord` sends writes one), `pg_typeof($1)` (function overload resolution), and `IS NOT DISTINCT FROM`. **29 captured statements, 25 byte-identical**; 2 tests | this commit |
| `SET` / `SHOW` / `RESET` of run-time parameters — run 46's four rows, 33 tests | **done**, and the finding is that **`SHOW` is the requirement and `SET` is the easy half**: every one of these tests reads the value back rather than trusting the tag, so a node that accepted the statement and reported the old value fails all 33. Four parameters joined `src/parameter.rs` — `lc_monetary`, `idle_in_transaction_session_timeout`, `geqo`, `debug_print_plan` — with two new value kinds, and both kinds exist because the read-back is **normalised, not echoed**: a `Duration` keeps its unit in the value (`SHOW` says `10ms`, and `pg_settings.unit` says `ms` *separately*, so a bare `10` would read `10ms ms`) while zero loses it, and a `NameList` requotes — `SET search_path TO '$user',public` reads back `"$user", public`, quoted and with a space after the comma. `current_setting()` is the third entry point and was missing entirely; it is folded in `Executor::bound`, and `current_setting(name, true)` is the one shape that must answer NULL rather than raise. **What this unit closed is an asymmetry rather than a gap**: an unknown name was `0A000` from `SET` and `42704` from `SHOW` and `RESET`, and the capture settles the commoner case — `SET nosuchparameter` is `42704` on a real server. All three agree now, `RESET ALL` came with them, and the price is `work_mem`, in §6 and in a test of its own. | `<this unit>` |
| `INSERT … DEFAULT VALUES` — an `INSERT` with no source, 117 tests in 27 files | **done, and it needed no executor arm**: `DEFAULT VALUES` is one row of *no expressions*, and the row assembly already started every column at its own default and ran the sequences after the values — so the lowering returns `rows: vec![vec![]]` and the rest is the path a short `VALUES` tuple has always taken. **It is not "insert a row of NULLs"**, which is the reading the syntax invites and the one the capture kills: `dvs.s` comes back `dflt`, and a table whose columns all have defaults gets `1|2`. Two facts the capture carries that reasoning would not: a table with **no primary key and no defaults** still gets a row (`count(*)` 1, `count(n)` 0), and a `NOT NULL` column with no default is `23502` whose DETAIL prints the key **already drawn** — `(1, null)` then `(2, null)`, so the sequence advanced through the failure. Two shapes the capture does not carry were measured on the same oracle rather than assumed: `INSERT INTO t (n) DEFAULT VALUES` is `42601` (it takes no column list) and `ON CONFLICT` composes with it. | `<this unit>` |
| The transaction timeouts, and **the file run 47 hung on** | **done, and the finding was a wrong answer nobody was looking for.** `adapters/postgresql/transaction_test.rb` sat for twenty minutes on one `ESTABLISHED` connection at 0% CPU; its tests synchronise two connections on *one side blocking*, and this node never blocks — a Percolator prewrite that meets a live lock is `40001` after a bounded backoff. Writing the two-session corpus for it turned up something else: two sessions updating one row's **unrelated** column ended with the loser told `23505 duplicate key value violates unique constraint "t_pkey"` about a primary key neither of them changed. `explain_conflict` maps a lost key to a unique index entry the transaction wrote, and an `UPDATE` rewrites its row's own entries — so a rewritten key looked exactly like an added one. Fixed by remembering the keys a remove-then-write removed (`Written::rewritten`, filled at all three such paths through one named helper); a key the statement **moved** is still the `23505` it should be. `ActiveRecord` maps `23505` to `RecordNotUnique`, so a retry loop keyed on `SerializationFailure` would never have retried. `statement_timeout` and `lock_timeout` joined the registry with the rule the module already had: honoured at `0`, which is what they permanently are here, and `0A000` naming the parameter above it — `42704` was the wrong sentence about a parameter a real server has. **The third parameter was not a missing wait but a missing feature, and it is built**: `idle_in_transaction_session_timeout` now ends the connection — `25P03`, severity `FATAL`, the open block rolled back before the socket goes, and a session idling with no block open left alone. It lives around the read that waits for the client, because what it does is close a socket and the executor is not running while it fires. **And finishing it turned up a second wrong answer in a unit already called done**: a duration parameter does not read back in the unit it was set in. PostgreSQL converts to the parameter's base unit and re-prints in the largest unit that divides it exactly, rounding half to even and accepting a fraction — eight corpus lines red, detailed in §6. The two duration lines the original capture had, `'10ms'` and `0`, are fixed points of every one of those rules, which is the whole reason it shipped. | `<this unit>` |
| A catalog function in a `VALUES` list — run 46's top shape, 41 tests in 4 files | **done**, and the refusal was older than the language it refused: `Expr::evaluate` read a `VALUES` tuple as **a literal or a parameter and nothing else**, a rule that predates this crate's expression language and had become the only place in it that could not evaluate one. Forty of the forty-one tests are `insert_all_test.rb`, because `ActiveRecord` **inlines** `CURRENT_TIMESTAMP` into the tuple rather than binding it. **The transaction is the semantics, not the plumbing**: `CURRENT_TIMESTAMP` is `transaction_timestamp()`, one instant for the whole transaction, so the two rows of one `insert_all` share a `created_at` and `count(DISTINCT created_at)` over them is `1` — a node that read a clock per row makes them differ and the tests compare them. **Six spellings, one instant, five types**, and the types are the half that bites: the check after the evaluation could not stay `Datum::fits`, which is *storage* equality — the right question for a row read off disk and the wrong one for a value a statement computed, because a `timestamptz` goes into a `timestamp` column without being asked and a `time` does not. `exec::assign` is that cast, and the `42804` it raises now names the expression's real type where the field held the placeholder string `the expression's`. **`IS NOT DISTINCT FROM` arrived with it** — same `upsert_all` template, a `BinaryOp` rather than a shape of its own — closing a divergence `tests/on_conflict.rs` declared and one of the four the `$N` unit left. `pg_cmp` gained `timestamp` vs `timestamptz`: the pair fell through the cross-variant order and answered **`f`** where a real server says `t`, which is the worst class ADR 0031 ranks. Two divergences, one fact — `CURRENT_TIME` is `time with time zone`, not a stored type (ADR 0033); its unzoned twin `LOCALTIME` is implemented and answers the capture's `42804` word for word, so what is missing is a type and not a rule. **29 captured statements, 26 byte-identical**; 3 divergences deleted elsewhere | this commit |
| `IN (subquery)` in the `WHERE` of a statement that writes — run 46's second shape, 39 tests in 9 files | **done**, and it arrived as three bugs wearing one refusal. The headline is what `delete_all` and `update_all` compile to on a relation carrying a `LIMIT` or a `JOIN` — `ActiveRecord` can express neither on a `DELETE`, so it wraps the selection in a subquery — and the read path plans subqueries where the write path never reaches that pass. `plan_in_write_filter` is `plan_subqueries`' counterpart: an `UPDATE` carries a filter rather than a `Select`, so there is no target list to walk and no `FROM` to build a scope from, and the scope is the one table the statement writes. **The pre-statement snapshot is where the call sits, not a rule anyone wrote**: the filter is planned, `resolve`d — every uncorrelated subquery run once — and only then does the cursor open, so `DELETE FROM t WHERE id IN (SELECT id FROM t LIMIT 1)` is not circular and deletes exactly one. The second bug was **not about writing at all**: the parameter walkers stopped at the subquery boundary, so a plain `SELECT … WHERE id IN (SELECT … WHERE n > $1)` was `42P18` too and no corpus had reached it (its own commit). The third was in the harness — a listed answer divergence was skipped *before the statement ran*, so a closed gap was absorbed silently and a replay's state diverged from the oracle's; a corpus of `SELECT`s cannot show it and this one's divergences are `DELETE`s (its own commit). `tests/subquery.rs` gave up two of its five refusals. **29 captured statements, 22 byte-identical**; three shapes declared, each a different mechanism — a multi-column row constructor (one operand and one column are singular everywhere here; `ActiveRecord` never sends it), `UPDATE … FROM` with a self-alias (a different statement, not this one with a subquery in it), and a **typed NULL that loses its type at lowering**, which makes `IN (SELECT NULL::bigint)` a `42883` where a real server matches nothing | this commit |
| `ALTER TABLE … DROP COLUMN` — run 47's ranking, 34 tests in 11 files | **done, and the row codec did not change** ([ADR 0051](../adr/0051-a-dropped-column-keeps-its-slot.md)). A row is decoded by position, so removing a column's slot would turn every row written before the `ALTER` into a decode error — `decode_row` refuses a row wider than its schema, deliberately. PostgreSQL's answer is not to remove it: the column is tombstoned, keeps its slot and its attnum for the life of the table, and the rows are never touched. So `esker-keys` is untouched, there is no row format version bump, and the `ALTER` is a catalog write. **The whole of the visible change is two lookups**: `TableDef::column` and `user_columns` skip the tombstone, which makes `SELECT gone`, `WHERE gone`, `ORDER BY gone`, `SET gone`, `INSERT (gone)` and `SELECT *` all right at once rather than clause by clause. Two more raw scans had to be found by hand and one of them was a live bug the tests caught: `duplicated` counted the tombstone, so a **re-added column of the same name was `42702`** — which is an ordinary sequence of Rails migrations. Everything on the table goes with the column silently (indexes of any width, `CHECK`, `NOT NULL`, default, comment, a foreign key declared on it, the sequence a `serial` owned, the primary key); only a dependent living elsewhere raises `2BP01`, and here that is another table's foreign key, because `CREATE VIEW` is still a refusal. `pg_attribute` is the one view that shows the tombstone — `attname` mangled, `atttypid` 0, `attnotnull` reset — because `ActiveRecord`'s own `column_definitions` filters `AND NOT a.attisdropped` and the row has to be there to be filtered. | `<this unit>` |
| `UPDATE … FROM` with a self-alias — what a **joined** `update_all` sends, the last of run 46's second shape | **done.** `ActiveRecord` cannot put a join on an `UPDATE`, so where `delete_all` wraps the selection in a subquery, `update_all` aliases the target, puts the join in a `FROM` and ties the two together in the `WHERE` — `UPDATE "c" "__active_record_update_alias" SET "body" = $1 FROM "c" INNER JOIN "p" ON … WHERE "c"."id" = "__active_record_update_alias"."id"`. **The alias is the mechanism, not decoration**: an alias *replaces* the table's name — `UPDATE t AS a … WHERE t.id = 1` is `42P01` with a HINT naming the alias, measured — and that is exactly what frees the name for the `FROM` entry sharing it. `plan::Update` gained the three fields a `Select` already had and the `FROM` is built as the join chain it is: the entry hangs off the target with **no condition**, because `t, x` and `t CROSS JOIN x` are the same clause, so a comma list, an inner join, a `LEFT JOIN` and a second entry all go through `plan_chain`'s code rather than a second one. Two rules the capture carries that a reading of the syntax does not: **a target row the join matches many times is written once** — three matching `FROM` rows and `SET hits = a.hits + 1` leaves `hits` at 1, not 3, so the join output is reduced to its first row per primary key — and **the `FROM` entry sees the rows as they were**, which materialising every row before writing any already gave and which is now pinned by a probe that reads each row through the next one. `RETURNING` names both sides, because by then they are one row; `SET a.body = …` is `42703` about the *column* `a` with PostgreSQL's own HINT, a variant of its own so a plain `SET nope = 1` does not carry it. The extended protocol came with it: the parameter walkers now reach the `FROM` and its `ON`s, and `Describe` of a `RETURNING` over an alias or a `FROM` relation resolves against the same chain instead of the target alone. **67 captured statements, 65 byte-identical**, and two divergences declared in §6; three entries deleted elsewhere — the aliased `UPDATE` in `tests/alias.rs` and two in `tests/write_in_subquery.rs`, whose third had a *stale reason* rule 2 cannot catch and now names the refusal that actually causes it | this commit |
| `CREATE DATABASE` / `DROP DATABASE` and the per-database key space — run 46's seventh row, 103 tests in 3 files | **done**, and the row is the one where the failing message was not a server error at all: `ActiveRecord::Fixture::FixtureError: table "dogs" has no columns named "trainer_id"` is raised by `ActiveRecord` itself after reading the columns correctly. `schema.rb:578` creates `dogs` with four association columns on `arunit`; its **last line**, 1496, creates a bare `dogs` on `arunit2`. Against one namespace the second `create_table … force: true` — `DROP TABLE IF EXISTS` then `CREATE TABLE`, never a merge — drops and recreates the first, five live columns down to one, `attisdropped` filtering away every trace. Captured on a real server first (`tests/two_database_dogs.rs`, 18 statements, 16 byte-identical). **The decision is that a database is a tenant** ([ADR 0052](../adr/0052-a-database-is-a-tenant-and-the-directory-that-names-them.md)) and it cost `esker-keys` nothing: every SQL key already carries a tenant as the first field after the namespace byte, so no table of one database can be *named* from another — PostgreSQL's own rule — `DROP DATABASE` is a bounded sweep with no third place a row can hide, and two databases cannot collide on a relation id. The one piece of state that cannot live inside a tenant is the directory, which is **two** record kinds under `'m' ++ "sql"` — the name is the key and the id is the body, because name → id is the only direction anything asks for. **An empty directory is one database, not none**: the seed is the fallback, so an existing cluster needs no migration, and `create_database` writes it beside the first row somebody types — otherwise the database the cluster was already serving would vanish from `pg_database` because a *different* row was added. `DATABASE_OID`, a constant kept equal to a second constant by hand, is gone: the oid **is** the id **is** the tenant. The sweep on drop is every kind byte rather than a list of the ones that take a tenant, because a list is a thing to forget. Three things the statements settle: `25001` for either inside a transaction block (measured — and the reason a rolled-back capture cannot cover the working path, so seven tests do); `55006` and `3D000` are different mistakes, so `IF EXISTS` does not cover the open database; and the check is by **id**, not by name. Finally `current_database()` stopped being a constant of the server and became `Expr::CurrentDatabase`, folded where `current_schema()` is — a constant would have reported the default's name to a client connected elsewhere, which is a wrong answer and not a missing feature. Three divergences in §6, all of them named rather than found | this commit |
| `last_value, is_called` — a sequence is a relation you can read | **done, and the flag cannot be derived from the counter, which is why it is stored.** This node keeps one number per sequence — the value `nextval` will hand out next — and `setval(s, 5, false)` and `setval(s, 4, true)` both leave it at 5, yet they are different sequences: the first reports `last_value 5`, the second `last_value 4`, and both hand out 5. The counter record grows a ninth byte and **eight bytes reads back exactly right**, because every sequence here starts at 1, so a counter still at 1 has handed nothing out. No catalog version bump — the record is not versioned and never was; its width is its version. The plumbing is that **a sequence read is not transactional**: `nextval` and `setval` commit outside the block that calls them, so the value is taken in a transaction of its own, after the plan is built and before the cursor opens — the mirror image of the subquery and fragment passes that run in the same place because they *must* be in the statement's snapshot. **Third time for one rule**: `SELECT * FROM <seq>` came back without `last_value` until `TableDef::row_id` said a sequence is not a keyless table, which is the same wrong answer a catalog view and a derived table each gave before their line. 36 statements, **no divergences declared** | `49c432f` |
| One catalog read per statement, not one per `::regclass` | **done, measured red first as a ratio against a control** — the shape `a_scan_costs_its_range_and_not_the_store` established. `Executor::resolve_regclass` resolves each `::regclass` literal against the catalog and `relation_oid` read the **whole catalog** for every one of them, which `ActiveRecord`'s schema dump multiplies: several casts in one statement against a catalog with hundreds of relations. Over 200 relations, eight casts in one statement against one cast in another was **647 ms against 81 ms** — exactly the eight-to-one a read per literal predicts. It is one snapshot per statement now, taken lazily so a statement with no `::regclass` still reads nothing, and a snapshot rather than a cache with a lifetime: every literal in one statement resolves against the same catalog, which is what a real server does, and nothing in the statement has run yet so there is no write of its own to miss. The suite runs in 0.19 s | `d63e7d8` |
| A **typed NULL** keeps its type — h1's `IN (SELECT NULL::bigint)`, and the debt three units had named | **done, and the gate is where its size showed.** `NULL::anything` became `Literal::Null` at lowering — a cast on a NULL thrown away by one line, because until arithmetic arrived a NULL's type never mattered — so `WHERE id IN (SELECT NULL::bigint)` was `42883 operator does not exist: bigint = text` where a real server matches nothing. `Literal::TypedNull(ColumnType)` is that cast, kept: the **value** is still `Datum::Null`, evaluates to NULL and prints as NULL, and only the type survives, which is all the cast was ever for. It shipped **ungated** — the Docker daemon was wedged — and the container run is what measured it: **six declared divergences across four corpora started agreeing at once**, none of them looked for. `date + NULL::interval`, which `tests/temporal_arithmetic.rs` called *the one place in these three corpora where this node raises where PostgreSQL returns a row*; `date - NULL::int4`; both `DELETE … IN (SELECT NULL::bigint)` directions; `array_agg(NULL::int4)`; and `array_agg(x) FROM (VALUES (NULL::int))`. A seventh, `pg_typeof(array_agg(NULL::int4))`, moved from a wrong **row** to the standing `regtype`-versus-`text` type trade, so it changed lists rather than leaving. **The ratchet found all seven** — rule 2 of ADR 0031 is the whole mechanism, and it is the second time this lane has been told by it what a change closed elsewhere. What is left is the **bare** NULL, which is `text` here and `unknown` there: `array_agg(NULL)` answers `text[]` where PostgreSQL raises `42725 … is not unique`, and `exec::aggregate::is_unknown`'s doc named the dropped cast as the reason it could not widen to a NULL. That reason is gone; the widening now needs only a capture of the bare-NULL argument across all six aggregates, since only `array_agg`'s half has been put to the oracle. Two rules the commit inferred are **still unmeasured** and say so in the code: that a typed NULL compares only within its own family, and that it is a fixed operand in arithmetic rather than an `unknown` taking the other side's type | `8aa5f65` + this commit |
| A column may be declared as a **user-defined type** — ADR 0050's first unit, catalog record v24 | **done for the storage half.** `ColumnDef::user_type: Option<u64>` and `ColumnType` learns nothing: a column's `ty` stays what the value physically *is* — an enum label is the `int2` of its position, which is what gives it PostgreSQL's own ordering, since `pg_enum.enumsortorder` is that server's sort key too — and the oid beside it is what the value is *called*. The parameterised variant would give every exhaustive match over `ColumnType` an arm that cannot be answered without a catalog lookup, in a crate that must not have one (invariant 7). **Lowering can no longer tell a user type from a typo**, because `sqlparser` hands `mood`, `money` and `nosuchtype` over identically, so the name rides on the plan and the executor answers — and *that* is what the round's surprise came from: moving the refusal from lowering to execution made two `EXCLUDE USING btree` statements in `tests/exclusion_constraint.rs` agree with PostgreSQL, which now refuses them for their **access method** before any type is resolved. Neither was looked for; the ratchet found both, and the deferral sits closer to a real server's own precedence than the eager refusal did. The labels ride on the `TableDef`, hydrated where it is loaded beside `sequences`, and **only when a column has one** | `aa01202` |
| **`pk_and_sequence_for` bounded** — run 49's stopper, and the reason that round has no suite number | **done.** A comma-separated `FROM` list has no `ON` anywhere, so `plan_chain` built a nested loop per step with nothing to bound it and put the whole `WHERE` in one filter on top: five catalog relations crossed and then filtered to one row. `fixtures.rb` runs it once per fixture table and **133 of 426 files** reach it. Each conjunct is applied at the first step whose tables can answer it — `resolve` is the test, and a conjunct that fails to resolve is not an error but one naming a table further right — and a **plain** comma list is reordered greedily: a table a constant pins first, then whichever shares a conjunct with the tables already chosen. Never below an outer join, and never for an entry that is a derived table or a function. Red first as a ratio with a control: **361 ms against 4.01 s** for 2× the catalog, where the control went 325 µs to 472 µs. The curve was 4 tables 98 ms to 14 tables 6.90 s; it is now **14 tables 2.2 ms, 300 110 ms, 900 833 ms** — past the suite's 870 relations, at the size that never returned. The reordering introduced a bug of its own and has its own test: `Scope::written` decides what `SELECT *` expands to, and left alone a reordered list answers the same columns in a different order, which no `ORDER BY` and no single-table assertion would have seen | `b31da99` |
