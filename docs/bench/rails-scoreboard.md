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

**Three numbers, and they measure three different things.** They moved apart in the second run and
that is the useful part rather than a problem:

| Number | What it measures | Where it lives |
|---|---|---|
| the **ladder** | how far a client gets before it stops | §The ladder |
| **statements served** | how many of `ActiveRecord`'s 36 boot statements are answered | §What each of ActiveRecord's 36 boot statements answers |
| the **suite** | how many of `activerecord/test`'s tests pass | §The suite |

A unit can move one and not the others — a table alias moved none of them and was still the thing
nineteen statements were waiting on — so a claim about "Rails support" that quotes one of the three
is a claim with the other two hidden behind it.

---

## Run 2 — 2026-09-01, `pg_type` and `pg_range`

| Field | Value |
|---|---|
| Esker | the commit that adds `crates/esker-sql/src/catalog/pg_catalog.rs`; run 1 was `b0eca1c` / `6971a30` |
| Rails | `v8.1.3.1` (`3989ebf`), the tag matching the `activerecord` gem the capture used |
| Ruby | 4.0.6, `pg` 1.6.3, `minitest` 5.27 |
| Node | one `esker-sql` process, `--release`, on the in-process backend: no cluster, no PD |
| Machine | Darwin 27.0.0, aarch64 |

**Why the in-process backend and not a cluster.** Run 1 was taken that way, and changing the
backend and the feature set in the same run would make the difference between them unreadable.
The node under test is the same binary either way — `esker-sql` over a real socket, speaking the
real protocol — and what a cluster adds is the store, which no statement here has reached. It is
worth taking once against `--pd` and three stores, and it is worth taking *after* the ladder gets
past rung 2, because until then the answer would be the same and the run would cost an hour.

### The ladder

`docs/plans/phase-9-rails.md` §3's four rungs. They exist because the suite is all-or-nothing —
it dies at `establish_connection`, so a suite score alone cannot say **how far** a client gets.

| Rung | What it is | Run 1 | Run 2 |
|---|---|---|---|
| 1 | `PG.connect` and one `SELECT 1`, over a socket | **PASS** | **PASS** |
| 2 | `establish_connection` and one migration | FAIL — `relation "pg_type" does not exist` | FAIL — **`the expression 'integer'::regtype::oid is not supported`** |
| 3 | CRUD through the adapter, and a schema dump | not reached | not reached |
| 4 | the suite's own schema loads | not reached | not reached |

**Rung 2's blocker moved off the catalog.** That is this run's whole output, and it is worth
saying what it means rather than only that it changed: `pg_type` and `pg_range` are answered, the
type map is built, and `ActiveRecord` gets as far as **running the migration** before it stops.
What stops it there is `ActiveRecord::ConnectionAdapters::PostgreSQL::Quoting#lookup_cast_type`,
which asks the server what OID a type name has:

```ruby
super(query_value("SELECT #{quote(sql_type)}::regtype::oid", "SCHEMA").to_i)
```

with `sql_type` being `character varying` — so **rung 2 now needs two things at once**, and they
are the same thing seen twice: a `::regtype` cast to *ask* the question, and a `pg_type` that has
an answer. This node's `pg_type` has six types and `character varying` is not one of them. The
next statement, 13, is the `CREATE TABLE` that names all three of `character varying`, `integer`
and `timestamp(6)`.

### What that says about the next unit

Two rankings, and they disagree — which is exactly why both are printed.

**By statements unblocked**, the largest group is a second `JOIN` at 7. **By the ladder**, the only
item that can move rung 2 is the **type surface**, and it is not close: nothing else in the table
is on the path a migration takes. A client that cannot run a migration never issues statements 15
or 26 in anger, so the seven behind a second join are seven statements no application reaches.

1. **The type surface — `integer`, `character varying`, `timestamp` — plus `'x'::regtype::oid`.**
   3 statements (13, and 14 + 20 behind it), 1 more for the cast, and **the ladder**. It is the
   only item here that needs another crate: `ColumnType` is `esker_keys::value::ColumnType` with
   the row codec beside it and a second copy in `esker-columnar`, so it is an ADR and two crates
   rather than a unit in `esker-sql`.
2. **a second `JOIN` — 7 statements**, the largest group and the one unit 4 named and left. A real
   unit: a nested `NestedLoop`, a three-table scope, and a probe boundary that is no longer "the
   last table". Nothing else blocks it.
3. **`= ANY(…)` — 4 statements**, which needs an **array**: a stored type, so the same crates the
   type surface needs, and worth doing in the same round for that reason.
4. **a qualified name (`pg_catalog.pg_class`) — 3 statements.** Cheaper than it looks now that the
   catalog exists: this node has no schemas, so `pg_catalog.x` is `x` and `anything_else.x` stays
   `0A000`.
5. **the rest of the catalog** — `pg_class`, `pg_attribute`, `pg_namespace`, `pg_index`,
   `pg_extension` — behind the four above, and the shape is now built rather than designed.

### The suite

`ARCONN=postgresql`, one process per file, the whole of `activerecord/test/cases/**/*_test.rb`.

| | Run 1 | Run 2 |
|---|---|---|
| Files run | 426 | 426 |
| Files that reached a first test | 59 (13.8%) | **59 (13.8%)** |
| Files that never loaded | 367 — every one at `establish_connection` | **367**, the same |
| Tests run | 772 | 772 |
| Passed | 525 | **771** |
| Errors | 247 | **0** |
| Failures | 0 | **1** |
| Excluded | 0 | **0** |
| Pass rate with conflict retry | not applicable | not applicable |

**Run 1's 247 errors were the harness's, not the server's, and this run says so.** Every one of
them was `NameError: uninitialized constant Arel::Nodes::ActiveModel`:
`test/cases/arel/helper.rb` requires `active_support` and `arel` and *not* `active_model`, while
`Arel::Nodes.build_quoted` names `ActiveModel::Attribute` — so Ruby resolves the constant inside
`Arel::Nodes`, fails, and every test through that path errors. In a full Rails checkout something
else loads it first, which is why the framework's own runs never see it.

The runner now does `bundle exec ruby -Itest -e 'require "active_model"; load ARGV[0]'`, and one
file went from **97 errors to 0** with that line and nothing else. Run 1's scoreboard said those
errors were "the Arel tests that *do* reach for an adapter"; that was wrong, and it is the second
time on this board that a number turned out to be about the measurement rather than about the
server. (It has to be `-e … load` rather than `RUBYOPT=-ractive_model`: a `-r` runs before bundler
has set the load path up, and fails with `cannot load such file -- active_model`.)

**What is left is one failure, and it is not about this server either.** `to_sql_test.rb`'s
`visit_BigDecimal` expects `2.14` and gets `0.214e1` — Ruby 4.0.6's `BigDecimal#to_s`, in a visitor
that builds a string and never opens a connection. It is **not excluded**: ADR 0031's three rules
admit a declared divergence, a named missing feature, or a test about PostgreSQL's own internals,
and "a test about Ruby's `BigDecimal`" is none of the three. Naming the cause and leaving the 1 in
the total is the honest treatment; widening the rules is an ADR amendment and not this unit's to
make.

**The suite still did not move, and that is this round's honest half.** 367 files never load, the
same 367, and the reason is one sentence: `ActiveRecord` cannot `establish_connection`. The ladder
moved a rung's worth and the statement count moved by four; the suite moved by nothing that this
unit did. Publishing that next to two numbers that did change is what the three-number split is
for — a scoreboard carrying only the suite would have recorded this round as wasted.

**All 59 files that ran are `test/cases/arel/`, and that is every Arel file in the suite** —
`find test/cases -name '*_test.rb' | grep -c /arel/` is 59, and the 59 that reached a test are
exactly those. (Checked by position rather than by name: the runner records a *basename*, and
`attribute_test.rb` exists both under `cases/` and under `cases/arel/attributes/`, so matching the
names would have mis-attributed six of them.) Arel is `ActiveRecord`'s SQL-string builder: it
composes an AST and prints it, and most of its tests never open a connection, which is exactly why
they survive a node that cannot be connected to. So **771 of 772 is 99.9% of the part of the suite
that does not test this server**, and quoting it as a pass rate would be worse flattery than run
1's 68% was. The number that describes this server is still the 367.

---

## What each of ActiveRecord's 36 boot statements answers today

A suite score of "nothing loaded" has no resolution, so this is the measurement that does. The 36
statements are `crates/esker-sql/tests/corpus/activerecord_8_1_statements.txt`, in the order a real
`ActiveRecord` issued them to a real PostgreSQL 19beta1; each was put to a running Esker node and
its answer recorded. `cargo test -p esker-sql --test activerecord_surface` asserts the count
exactly, so a unit that moves it has to say which unit moved it.

**15 of 36 run**, up from 11. The four are `4`, `7`, `8` and `9` — the whole of what `pg_type` and
`pg_range` were blocking, and the first four questions `ActiveRecord` asks a server.

The other 21, by what stops them **first**:

| Stops on | Statements | Run 1 | Run 2 |
|---|---|---|---|
| — *runs* | 1, 2, 3, **4**, 5, 6, **7**, **8**, **9**, 10, 11, 18, 19, 25, 27 | 11 | **15** |
| more than one `JOIN` | 15, 26, 32, 33, 34, 35, 36 | 7 | 7 |
| `= ANY(…)` | 16, 17, 21, 28 | 4 | 4 |
| a qualified name (`pg_catalog.pg_class`) | 29, 30, 31 | 3 | 3 |
| a type — `character varying`, `integer`, `timestamp(6)` | 13, and 14 + 20 behind it | 3 | 3 |
| a cast (`'integer'::regtype::oid`) | 12 | 1 | 1 |
| `current_schemas(false)` | 22 | 1 | 1 |
| a bare `current_schema` | 24 | 1 | 1 |
| `pg_extension` | 23 | 1 | 1 |
| **the catalog — `pg_type`, `pg_range`** | 4, 7, 8, 9 | **4** | **0** |

Statement 23 is worth a line, because it is the one that moved *sideways*: it was in the catalog
group and it is still there — it needs `pg_extension`, which this unit did not build. The group
went from five to one rather than to zero, and the row above splits what "the catalog" meant.

**A `0A000` is not one blocker, it is the first of several.** Statements 14 and 20 read
`relation "harness_widgets" does not exist` rather than a type error, because statement 13 — the
`CREATE TABLE` — is what failed; the fixture the replay builds is in the six types this node has,
and the table `ActiveRecord` asked for is not. They are counted under the type surface for that
reason and not under a missing table.

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

Three things the runner had to be told, each of which cost a run to find:

* the suite is run against the **harness's** `Gemfile` and not the rails checkout's, which pulls
  the whole framework's 200-gem development bundle, none of it about the adapter;
* the build is done with `RUSTUP_TOOLCHAIN` set explicitly, because a `cd` into the repo is not
  enough under every launcher and this machine's default toolchain is a nightly that ICEs on
  `tokio`;
* each file is loaded with `active_model` already required, which is what turned run 1's 247
  errors into 0 — see §The suite.

The per-statement table comes from replaying the corpus against a running node:

```sh
cd ~/workspace/lab/esker
cargo build --release -p esker-sql --bin esker-sql
./target/release/esker-sql 127.0.0.1:55442 &
psql -h 127.0.0.1 -p 55442 -U esker -d esker -X -q -A -t \
  -c "CREATE TABLE harness_widgets (id int8 PRIMARY KEY, name text NOT NULL, n int8, live bool)"
grep -v '^#' crates/esker-sql/tests/corpus/activerecord_8_1_statements.txt | grep -v '^$' |
  cat -n | while IFS=$'\t' read -r number statement; do
    printf '%s\t%s\n' "$number" \
      "$(psql -h 127.0.0.1 -p 55442 -U esker -d esker -X -q -A -t -c "$statement" 2>&1 |
         grep -E '^ERROR' | head -1)"
  done
```
