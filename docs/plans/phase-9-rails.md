# Phase 9 plan — a PostgreSQL that Rails can talk to, scored by Rails' own tests

Status: **units 0–4 landed** (unit 4 bar a second join); **unit 5 in progress in the measured order** — the table alias, the eight session statements and `IN (list)` are built, and `ActiveRecord`'s 36 went 3 → **11**; the type surface is blocked on `esker-keys` and said so. **Unit 6's first scoreboard is in** ([`docs/bench/rails-scoreboard.md`](../bench/rails-scoreboard.md)): rung 1 of the ladder passes and rung 2 now stops on `relation "pg_type" does not exist` — the catalog, which is where the next unit is. §2 says what the measurement changed, §9 records progress per unit, §6 the divergences, §7 what unit 1 changed and §8 what is watched.

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
this unit; `IN` inherits it exactly because it shares `reconcile`. Recorded in §6 and in
`tests/in_list.rs`'s `DIVERGENCES`, and it wants a unit of its own.

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
| the catalog's content | **not started**; `pg_type` + `pg_range` first |

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
| `SET <parameter>` for a parameter this node does not have is `0A000` naming it, where PostgreSQL answers `42704` | PostgreSQL knows that an un-namespaced name it does not have cannot be a custom GUC. Telling `work_mem` — a real parameter this node does not implement — from a name nobody has would mean carrying PostgreSQL's whole GUC table, so a `SET` this node does not run names itself under contract C2 rather than claim the parameter is absent. `SHOW` and `RESET` make the opposite trade and answer `42704` for both; that asymmetry is older than this unit and is worth closing in one direction when there is a reason to pick one. | `tests/session_parameters.rs`'s `DIVERGENCES` |
| A `SET` that changes a `GUC_REPORT` parameter sends no `ParameterStatus` | PostgreSQL tells a client when `standard_conforming_strings`, `TimeZone` or `IntervalStyle` changes, so a driver can track it. Of the values this node accepts, only `IntervalStyle` ever *changes* from what the startup packet announced — and it governs how an `interval` prints, of which this node has none. The other two are honoured only at the value they were announced with. Recorded rather than built: the report would have to leave the executor through `Outcome`, and nothing measurable is wrong today. | this table, `src/parameter.rs` |
| A column alias list (`FROM t AS x (c, d)`), and an alias on `UPDATE` / `DELETE`, are `0A000` naming themselves | A real server takes all three. The column list renames the table's columns, so ignoring it would answer a query about `c` with a column called `id` — a wrong answer rather than a gap. `UPDATE`/`DELETE` resolve against one table and have no second name to tell apart, so the alias buys nothing there; the `SELECT` side is what the 19 catalog statements need. | `tests/alias.rs`'s `DIVERGENCES` |
| **`SELECT 1 = '1'` is `f`, where PostgreSQL answers `t`** | Two literals with no column to type either against are compared untyped, so an `int8` never equals a `text`. It is a **wrong answer and not a refusal**, which is the one outcome this crate is built to avoid, and it is *older than the unit that found it*: `IN` shares `reconcile` with `=` and inherits it exactly. Against a column both are right — `id = '1'` and `id IN ('1')` match — because there the column gives the literal a type. It wants a unit of its own: the fix is a type for an untyped literal in a comparison that has no column in it, which is PostgreSQL's `unknown` resolution and is a rule, not a patch. | [`tests/in_list.rs`]'s `DIVERGENCES`, `tests/corpus/pg19_in.txt` |
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
| 2026-09-01, **twice** | `joint_gate`'s two transport calls have each missed their 30-second deadline once under a fully parallel `cargo test`: first `a_lock_the_ttl_kills_resolves_the_same_way_on_both_engines` on the `TxnKv` call, then `the_learner_answers_fragments_that_agree_with_a_row_scan` on the fragment call — `Timeout { no answer from 127.0.0.1:60224 in 30s }`. Both pass standalone. **Two different tests, one failure mode**, which narrows it: this is not the lock-expiry clock `1f22077` hardened, it is a 30-second RPC deadline against however many test binaries this machine is running at once. | **chased — see below.** Both calls dump instead of unwrapping: `target/joint-gate-transport-<ts>.txt`, naming who led the region, every store's address, leadership and applied index, and the deadline. |
| 2026-09-01, **third** | The same test and the same call, and the dump says it is **not the deadline**: `connection closed: region 1 stopped leading with this proposal in its log`, with **no store leading** and all four agreed at `applied=16`. An election gap under a saturated machine, not an expired timeout. | **chased, and handed over.** The finding and the one-line fix are below; the file is another lane's. |

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

**Handed over rather than fixed**: `tests/joint_gate.rs` is another lane's file (§ "Lane", above),
and the change is theirs to make — one retry, on the leadership error only, at each of the two
transport call sites that already dump.

## 9. Progress

| Unit | State | Commit |
|---|---|---|
| 0 — ADR, plan, aggregate capture | **done** | `de3c465`, `32fb58f` |
| 1 — aggregates | **done** | `1d78a96` |
| 2 — sequences and `RETURNING` | **done** | `e1b1bd2`, `7a7d4f3`, `bf28e0d` |
| 3 — savepoints | **done** | `779ae2e` (capture), `a74b724` |
| 4 — joins | **`LEFT`, `ON`, `USING` done**; a second join is not | `56d23e2` |
| 5 — `pg_catalog` | **in progress, in the measured order.** Captured and re-ordered (`b8d90e7`); the **table alias**, the **six `SET`s + two `SHOW`s** and **`IN (list)`** built, taking `ActiveRecord`'s 36 from 3 served to **11** and moving five statements onto the catalog; the type surface **blocked on `esker-keys`** and reported; the translation approach **decided** (views over records). What is left is the catalog's content, starting at `pg_type`. | `b8d90e7`, `b0eca1c`, this commit |
| 6 — the scoreboard | **first run done**: the harness runs the ladder and all 426 suite files, `docs/bench/rails-scoreboard.md` carries both. 59 files reach a test, 367 die at `establish_connection`; rung 1 passes. Its finding — `IN (list)`, the first query `ActiveRecord` sends — was built in the same round, and rung 2 now stops on the catalog | this commit |
