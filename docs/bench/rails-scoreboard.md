# Rails scoreboard

How far a real `ActiveRecord` gets against this node, measured rather than claimed. Design:
[ADR 0031](../adr/0031-rails-compatibility-is-measured.md). Plan:
[`docs/plans/phase-9-rails.md`](../plans/phase-9-rails.md) unit 6.

This file is **regenerated, not edited**. A number nobody can reproduce is not a measurement, so
the commands that produce every number are in §Reproducing it, and the harness they run is outside
this repository — `esker` is 100% Rust plus documentation and the `rails/rails` checkout, the
`config.yml` and the runners are Ruby (plan §0).

**The first number is the baseline whatever it is.** ADR 0031's rule 3 is what keeps it from being
gamed: the exclusion list may only hold a declared divergence, a named missing feature, or a test
about PostgreSQL's own internals — and "it fails and I do not know why" is not one of the three.

---

## Run — 2026-09-01

| Field | Value |
|---|---|
| Esker | `b0eca1c` for the suite run; the ladder re-taken after `IN` landed, at the commit that adds this file. Both columns are below. |
| Rails | `v8.1.3.1` (`3989ebf`), the tag matching the `activerecord` gem the capture used |
| Ruby | 4.0.6, `pg` 1.6.3, `minitest` 5.27 |
| Node | one `esker-sql` process, `--release`, on the in-process backend: no cluster, no PD |
| Machine | Darwin 27.0.0, aarch64 |

### The ladder

`docs/plans/phase-9-rails.md` §3's four rungs. They exist because the suite is all-or-nothing —
it dies at `establish_connection`, so a suite score alone cannot say **how far** a client gets.

| Rung | What it is | At `b0eca1c` | With `IN` |
|---|---|---|---|
| 1 | `PG.connect` and one `SELECT 1`, over a socket | **PASS** | **PASS** |
| 2 | `establish_connection` and one migration | FAIL — `the expression t.typname IN (…) is not supported` | FAIL — **`relation "pg_type" does not exist`** |
| 3 | CRUD through the adapter, and a schema dump | not reached | not reached |
| 4 | the suite's own schema loads | not reached | not reached |

**Rung 1 passes**, which is not nothing: `libpq` negotiates the protocol, authenticates and runs a
query against a server written from scratch in this repository.

**Rung 2's blocker moved.** `AbstractAdapter` builds its type map from the first query it ever
sends — `SELECT t.oid, t.typname FROM pg_type as t WHERE t.typname IN ('int2', …)` — and at
`b0eca1c` that was `0A000` for **`IN (list)`**, before the missing `pg_type` was ever reached. With
`IN` implemented the same statement gets one step further and stops on the **catalog**, which is
the destination rather than another thing in front of it. That move is the scoreboard's whole
output for this round.

### The suite

`ARCONN=postgresql`, one process per file, the whole of `activerecord/test/cases/**/*_test.rb`.

| | |
|---|---|
| Files run | **426** |
| Files that reached a first test | **59** (13.8%) |
| Files that never loaded | **367** — every one at `establish_connection` |
| Tests run | 772 |
| Passed | 525 |
| Errors | 247 |
| Failures | 0 |
| Excluded | **0** |
| Pass rate with conflict retry | not applicable — see below |

**All 59 files that ran are `test/cases/arel/`, and that is every Arel file in the suite.** Not one
test outside Arel ran at all. Arel is `ActiveRecord`'s SQL-string builder: it composes an AST and
prints it, and most of its tests never open a connection — which is exactly why they are the ones
that survive a node that cannot be connected to. So 525 of 772 is **68% of the part of the suite
that does not test this server**, and quoting it as a pass rate would be the flattery ADR 0031
exists to prevent. The number that describes this server is the 367.

(The 247 errors are inside Arel too: 39 of the 59 files are fully green and the other 20 hold the
Arel tests that *do* reach for an adapter.)

**Conflict retry** (ADR 0031's second number, the stated price of snapshot isolation) is **not
zero — it is undefined**: a `40001` cannot happen in a session that never opens a transaction. It
gets a number the first time a test reaches one.

**The exclusion list is empty**, and that is the honest state rather than a starting position.
ADR 0031's three rules admit a declared divergence, a named missing feature, or a test about
PostgreSQL's own internals; nothing has been *shown* to be any of the three, because nothing that
matters has run. `exclusions.txt` in the harness records that, and every entry it grows will name
its reason.

---

## What each of ActiveRecord's 36 boot statements answers today

A suite score of "nothing loaded" has no resolution, so this is the measurement that does. The 36
statements are `crates/esker-sql/tests/corpus/activerecord_8_1_statements.txt`, in the order a real
`ActiveRecord` issued them to a real PostgreSQL 19beta1; each was put to a running Esker node and
its answer recorded.

**11 of 36 run.** The other 25, by what stops them **first**:

| Stops on | Statements | At `b0eca1c` | With `IN` |
|---|---|---|---|
| — *runs* | 1, 2, 3, 5, 6, 10, 11, 18, 19, 25, 27 | **11** | **11** |
| more than one `JOIN` | 15, 26, 32, 33, 34, 35, 36 | 7 | 7 |
| **the catalog itself** (`pg_type`, `pg_extension`) | 4, 7, 8, 9, 23 | 1 | **5** |
| `= ANY(…)` | 16, 17, 21, 28 | 4 | 4 |
| a qualified name (`pg_catalog.pg_class`) | 29, 30, 31 | 3 | 3 |
| a type — `character varying`, `integer`, `timestamp(6)` | 13, and 14 + 20 behind it | 3 | 3 |
| a cast (`'integer'::regtype::oid`) | 12 | 1 | 1 |
| `current_schemas(false)` | 22 | 1 | 1 |
| a bare `current_schema` | 24 | 1 | 1 |
| `IN (list)` | — | 4 | **0** |

The `IN` column is why this table has two of them. It is also why the reclassification a reader
would make from the blocker counts alone is wrong: nineteen statements *read* `pg_catalog`, so the
catalog looks like the obvious next unit — but before `IN`, exactly **one** of the thirty-six got
far enough to say so. The refusals come from lowering, which runs before the catalog is consulted,
so every statement stopped in the query surface would have been stopped there whatever the catalog
held.

### What that says about the next unit

Ranked by statements-per-unit, which is the only ranking this file can support:

1. **`pg_type` and `pg_range`** — 4 statements (4, 7, 8, 9), and **the first thing `ActiveRecord`
   asks for**. It is now what rung 2 stops on, and it is the smallest useful slice of the catalog:
   two relations of fixed content, read-only, no per-tenant state at all. It is the only item that
   can move the ladder.
2. **a second `JOIN`** — 7 statements, the largest group, and the one unit 4 named and left on
   purpose. A real unit: a nested `NestedLoop`, a three-table scope, and a probe boundary that is
   no longer "the last table".
3. **a qualified name** (`pg_catalog.pg_class`) — 3 statements, and needed by the catalog anyway,
   since that is how half of them spell it.
4. **`= ANY(…)`** — 4 statements, which needs an **array**: a stored type, so `esker-keys`, so the
   same block the type surface hit.
5. **the rest of the catalog** — `pg_class`, `pg_attribute`, `pg_namespace`, `pg_index` — which is
   what the remaining fourteen need once the four above are done.

### Blocked, and not on this lane

Three of the 36 (13, and 14 + 20 behind it) need `character varying`, `integer` or `timestamp(6)`,
and none can be built in `crates/esker-sql/`: `ColumnType` is `esker_keys::value::ColumnType`, a
six-variant enum with the row-value codec beside it and a second copy in `esker-columnar`.
`= ANY(…)`'s array is the same class of change. Both are reported rather than worked around —
`docs/plans/phase-9-rails.md` §2 has the argument for why mapping `integer` onto `int8` is not an
option.

---

## Reproducing it

The harness is at `/Users/chunlea/workspace/lab/esker-rails-harness/` and is never committed here.

```sh
cd ~/workspace/lab/esker-rails-harness

# once: the toolchain and the suite under test
bundle install                                             # rails 8.1.3.1, pg 1.6.3, minitest
git clone --depth 1 --branch v8.1.3.1 \
    https://github.com/rails/rails.git rails

./run-scoreboard.sh --ladder-only    # the four rungs; seconds
./run-scoreboard.sh                  # the rungs, then all 426 files; about half an hour
```

`run-scoreboard.sh` builds `esker-sql --release` from `$ESKER_REPO` (default `../esker`) with the
toolchain named in the repo's `rust-toolchain.toml`, starts a node on `$ESKER_PORT` (default
55433), and writes `results/ladder.txt`, `results/suite.txt` and `results/versions.txt`.
`exclusions.txt` is the exclusion list; the runner reads the file names out of it and records each
as `EXCLUDED`.

Two things the runner had to be told, both of which cost a run to find: the suite is run against
the **harness's** `Gemfile` and not the rails checkout's (which pulls the whole framework's
200-gem development bundle, none of it about the adapter), and the build is done with
`RUSTUP_TOOLCHAIN` set explicitly, because a `cd` into the repo is not enough under every launcher
and this machine's default toolchain is a nightly that ICEs on `tokio`.

The per-statement table comes from replaying the corpus against a running node:

```sh
cd ~/workspace/lab/esker
cargo build --release -p esker-sql --bin esker-sql
./target/release/esker-sql 127.0.0.1:55442 &
psql -h 127.0.0.1 -p 55442 -U esker -d esker -X -q -A -t \
  -c "CREATE TABLE harness_widgets (id int8 PRIMARY KEY, name text NOT NULL, n int8, live bool)"
grep -v '^#' crates/esker-sql/tests/corpus/activerecord_8_1_statements.txt | grep -v '^$' |
  while IFS= read -r s; do
    printf '%s\t%s\n' "$s" \
      "$(psql -h 127.0.0.1 -p 55442 -U esker -d esker -X -q -A -t -c "$s" 2>&1 |
         grep -E '^ERROR' | head -1)"
  done
```

`cargo test -p esker-sql --test activerecord_surface` asserts the count of 11 exactly, so a unit
that moves it has to say which unit moved it.
