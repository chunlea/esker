# What one statement reads below the SQL

Taken 2026-09-10 on the in-process node (`MemoryBackend`) with `ESKER_STMT_STATS=1`
`ESKER_STMT_STATS_TRACE=1`, by `crates/esker-sql/tests/statement_reads.rs`. Fixture: one
user table, `CREATE TABLE pk0 (id bigserial primary key, a int8, b text)`, with one row.

**Topology-independent by construction.** Which keys a statement reads is a property of the
catalog code; only the *round-trip* count depends on the client and its buffer, and that
number stays r1's to take on a real cluster. So the counts below are **KV reads at the store
boundary**, which is where `Txn::get` and `Txn::scan` are the only two doors.

**Re-taken 2026-09-10 after `debts-v1.1.md` #49 option (b)** (`docs/plans/debt-49-catalog-cache.md`,
[ADR 0106](../adr/0106-what-a-statement-reads-below-the-sql.md)). The "before" column is the census
this file held when the plan was written, and the whole of it is kept at the end — a plan whose
"after" number is not measured the same way as its "before" has not been measured.

**Re-taken again 2026-09-11 on `009fc783`**, the tip after #78 and #79, by the same command on the
same machine. It was asked for to price #79's client-side paging, and the first thing to say about
it is what it **cannot** show: this census runs on `MemoryBackend` through `parity::Node`, so there
is no client and no wire in it, and it counts **one entry per `Txn::scan` call** rather than per
round trip. #79 changed neither which keys a statement reads nor how many times it opens either
door — only how many wire calls each one becomes. **So this census is the control, and the price is
in [its own section below](#what-79s-paging-costs-in-round-trips), measured where the round trips
are.** What did move in it is a day of other lanes' work, and that is worth having.

## The shapes

| statement | before | after (09-10) | **09-11, `009fc783`** | after, cache warm |
|---|---|---|---|---|
| `pk_and_sequence_for` | **60** | 12 | **13** | **4** |
| `SELECT a FROM pk0 WHERE id = 1` | 9 | 5 | 5 | 5 |
| `INSERT INTO pk0 … RETURNING id` | 8 | 5 | 5 | 5 |
| `CREATE TABLE` | 17 | 16 | 16 | 16 |
| `ALTER TABLE … DISABLE TRIGGER ALL` | 17 | **10** | 10 | 10 |
| `DROP TABLE` | 29 | **22** | **14** | 14 |
| `BEGIN` / `SAVEPOINT` / `RELEASE` / `COMMIT` | 0 | 0 | 0 | 0 |

**Two of the seven moved in a day, and neither is #79's.** `DROP TABLE` went **22 → 14**: its
composition changed from 9 point + 13 range to 9 point + **5** range, so eight scans left it, and
they left it in `21654f97 perf(sql): dropping a table stops reading every relation`.
`pk_and_sequence_for` went **12 → 13**, one range scan more, on a day when
`f8209599 perf(sql): a relations listing reads the table records in one scan` traded that listing's
point reads for a scan — the direction is right but this file did not measure the trade, so the
attribution is a candidate and not a claim. The other five are identical read for read.

**"Cache warm" is the number that matters** and it is not in the trace below, because the trace's
fixture runs each statement once. It is `crates/esker-sql/tests/catalog_read_slope.rs`'s: the
*second* run of `pk_and_sequence_for` at an unchanged version reads **four keys — two catalog
views' two version counters — at a catalog of twenty relations and at a catalog of a hundred.**

### What is left, and whose it is

* **The four version reads.** Two views per statement, two counters each. `debts-v1.1.md` #50 and
  [ADR 0105](../adr/0105-a-catalog-read-never-waits.md) are the row about them; option (b) kept
  exactly the one per view that is its validator.
* **`CREATE TABLE` barely moved**, and the plan's estimate for it was optimistic for the reason
  the plan's own *"a DDL turns the cache off on purpose"* section gives: its `2 x name(…)` pairs
  are one read from the resolution and one from `create_table`'s duplicate check, and the
  statement is on the **uncached** view for both — a memo between them would be a transaction not
  seeing its own writes. Its two `layout` reads are `stamp_layout`'s, which is reached from
  `bump_version` and has no `Catalog` in hand to memoise against; that is a threading change
  across every DDL and it was not taken here.
* **`DROP TABLE`'s scans are its own**: it enumerates the catalog *after* writing to it, so it
  runs on the uncached view by design.

## What #79's paging costs in round trips

`debts-v1.1.md` #79 made a scan end on an **empty** batch and never on a short one, because a
store's answer is bounded by a byte budget no caller can see. The rule spends a round trip to buy
that: a range scan that returned rows asks once more and is told there are none.

**Priced by an A/B on one machine**, `crates/esker-sql/tests/ddl_cost.rs` against the three-store
in-process cluster with `ESKER_STMT_STATS=1` — arm **A** is `009fc783`, arm **B** the same tree with
the client's resume taken back out (one call per region, the pre-#79 rule). `trips` is
`esker_client::stmt_stats`, counted at `Router::call`, which is the one place that sees a wire call.

    what_the_catalog_walkers_read, 150 relations        scans     B      A      Δ
    CREATE EXTENSION IF NOT EXISTS "uuid-ossp"              1     6      7     +1
    pg_class listing (ActiveRecord's)                       5    11     13     +2
    column introspection of ONE table                       1     8      9     +1
    DROP EXTENSION IF EXISTS hstore CASCADE                 5     9     12     +3

    what_each_ddl_statement_costs                       scans     B      A      Δ
    CREATE TABLE base / CREATE TABLE t                    0,0    17,14  17,14   0
    SELECT n FROM base WHERE id = 1                         0     5      5      0
    INSERT INTO base … / UPDATE base …                    2,2    13,10  13,10   0
    CREATE INDEX t_a ON t (a)                               2    20     20      0
    ALTER TABLE t ADD COLUMN / DISABLE / ENABLE           1,1,1  15,14,14 15,14,14  0
    DROP TABLE t  (100 rows and an index)                   8    18     21     +3
    DROP TABLE d0 · d1 · d2                               5,6,8  18,16,21 19,18,22  +1,+2,+1
    DROP TABLE k0 · k1 · k2 · child                     5,5,5,5  18,18,18,18 19,20,20,19  +1,+2,+2,+1

**The rule is exact and it is not "per scan".** It is **+1 wire call per range scan that returned
rows**; a scan whose first batch is empty is finished by that batch and costs nothing extra. That
is why nine of the seventeen DDL shapes move by zero — a `CREATE INDEX`'s two scans and an
`INSERT`'s two are lookups into an empty index — and why `DROP TABLE t` moves by three out of its
eight. The client's own scan counter says the same thing from the other side: `DROP EXTENSION …
CASCADE`'s wire scan calls go **17 → 26**, so nine of its seventeen logical scans had rows in them.

**As a share**: +1 to +3 on statements costing 5 to 22 round trips, so **0% for a statement with no
populated range and 5–25% for one whose work is scanning**. Nothing here is a real topology — every
one of these round trips is a loopback — so what transfers is the **count**, and r1's pass is where
the milliseconds are.

**What the alternative would buy back.** `docs/DESIGN.md` §9 records the design not taken: a
resumption cursor on the wire, where the store says whether more remains. It removes every one of
the `Δ` above and costs a field in `TxnKvResp::Scan` plus a golden. This table is the price of not
having it.

**Single runs.** Each arm is one pass; `trips` includes retries and region resolution, so a lone
±1 is noise. The pattern is not: it reproduces across two different tests, agrees with the client's
independent scan-call counter, and matches what the mechanism predicts key for key.

## The raw trace, 2026-09-11 (`009fc783`)

```text
=== pk_and_sequence_for
    SELECT attr.attname, nsp.nspname, seq.relname FROM pg_class seq, pg_attribute attr, pg_depend dep, pg_constraint cons, pg_namespace nsp WHERE seq.oid = dep.objid AND seq.relkind = 'S' AND attr.attrelid = dep.refobjid AND attr.attnum = dep.refobjsubid AND attr.attrelid = cons.conrelid AND attr.attnum = cons.conkey[1] AND seq.relnamespace = nsp.oid AND cons.contype = 'p' AND dep.classid = 'pg_class'::regclass AND dep.refobjid = '"pk0"'::regclass
    13 reads: 9 point, 4 range
        1 x  get  name(t1,"pg_attribute")
        1 x  get  name(t1,"pg_class")
        1 x  get  name(t1,"pg_constraint")
        1 x  get  name(t1,"pg_depend")
        1 x  get  name(t1,"pg_namespace")
        2 x  get  version(t1)
        2 x  get  version(t18446744073709551615)
        1 x  scan name(t1)..name(t1,…)
        1 x  scan schema(t1)..schema(t1,…)
        1 x  scan table(t1)..table(t1,…)
        1 x  scan type(t1)..type(t1,…)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  name(t1,"pg_class")
        4. scan table(t1)..table(t1,…)
        5. scan name(t1)..name(t1,…)
        6. scan type(t1)..type(t1,…)
        7. get  version(t1)
        8. get  version(t18446744073709551615)
        9. get  name(t1,"pg_attribute")
       10. get  name(t1,"pg_depend")
       11. get  name(t1,"pg_constraint")
       12. get  name(t1,"pg_namespace")
       13. scan schema(t1)..schema(t1,…)
=== a point select
    SELECT a FROM pk0 WHERE id = 1
    5 reads: 5 point, 0 range
        1 x  get  row(t1,#16384)
        2 x  get  version(t1)
        2 x  get  version(t18446744073709551615)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  version(t1)
        4. get  version(t18446744073709551615)
        5. get  row(t1,#16384)
=== an insert returning
    INSERT INTO pk0 (a, b) VALUES (2, 'y') RETURNING id
    5 reads: 5 point, 0 range
        1 x  get  row(t1,#16384)
        2 x  get  version(t1)
        2 x  get  version(t18446744073709551615)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  version(t1)
        4. get  version(t18446744073709551615)
        5. get  row(t1,#16384)
=== begin
    BEGIN
    0 reads: 0 point, 0 range
    in order:
=== savepoint
    SAVEPOINT s1
    0 reads: 0 point, 0 range
    in order:
=== release
    RELEASE SAVEPOINT s1
    0 reads: 0 point, 0 range
    in order:
=== commit
    COMMIT
    0 reads: 0 point, 0 range
    in order:
=== a point select, again
    SELECT a FROM pk0 WHERE id = 1
    5 reads: 5 point, 0 range
        1 x  get  row(t1,#16384)
        2 x  get  version(t1)
        2 x  get  version(t18446744073709551615)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  version(t1)
        4. get  version(t18446744073709551615)
        5. get  row(t1,#16384)
=== CREATE TABLE
    CREATE TABLE d2 (id bigserial primary key, a int8, b text)
    16 reads: 16 point, 0 range
        2 x  get  layout
        2 x  get  name(t1,"d2")
        2 x  get  name(t1,"d2_id_seq")
        2 x  get  name(t1,"d2_pkey")
        2 x  get  next-id(t1)
        2 x  get  schema(t1,"esker")
        3 x  get  version(t1)
        1 x  get  version(t18446744073709551615)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  schema(t1,"esker")
        4. get  schema(t1,"esker")
        5. get  name(t1,"d2")
        6. get  name(t1,"d2_pkey")
        7. get  next-id(t1)
        8. get  name(t1,"d2_id_seq")
        9. get  next-id(t1)
       10. get  name(t1,"d2")
       11. get  name(t1,"d2_pkey")
       12. get  layout
       13. get  version(t1)
       14. get  name(t1,"d2_id_seq")
       15. get  layout
       16. get  version(t1)
=== ALTER TABLE ... DISABLE TRIGGER ALL
    ALTER TABLE d0 DISABLE TRIGGER ALL
    10 reads: 9 point, 1 range
        1 x  get  layout
        2 x  get  name(t1,"d0")
        2 x  get  schema(t1,"esker")
        1 x  get  table(t1,#16384)
        2 x  get  version(t1)
        1 x  get  version(t18446744073709551615)
        1 x  scan sequence(t1,#16384)..sequence(t1,#16384,…)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  schema(t1,"esker")
        4. get  schema(t1,"esker")
        5. get  name(t1,"d0")
        6. get  name(t1,"d0")
        7. get  table(t1,#16384)
        8. scan sequence(t1,#16384)..sequence(t1,#16384,…)
        9. get  layout
       10. get  version(t1)
=== DROP TABLE
    DROP TABLE d1
    14 reads: 9 point, 5 range
        1 x  get  layout
        2 x  get  name(t1,"d1")
        2 x  get  schema(t1,"esker")
        1 x  get  table(t1,#16386)
        2 x  get  version(t1)
        1 x  get  version(t18446744073709551615)
        1 x  scan fk-backref(t1,#16386)..fk-backref(t1,#16386,…)
        1 x  scan row(t1,#16386)..sql-index
        1 x  scan sequence(t1,#16386)..sequence(t1,#16386,…)
        1 x  scan table(t1)..table(t1,…)
        1 x  scan view(t1)..view(t1,…)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  schema(t1,"esker")
        4. get  schema(t1,"esker")
        5. get  name(t1,"d1")
        6. get  name(t1,"d1")
        7. get  table(t1,#16386)
        8. scan sequence(t1,#16386)..sequence(t1,#16386,…)
        9. scan view(t1)..view(t1,…)
       10. scan table(t1)..table(t1,…)
       11. scan fk-backref(t1,#16386)..fk-backref(t1,#16386,…)
       12. scan row(t1,#16386)..sql-index
       13. get  layout
       14. get  version(t1)
```

## The raw trace, 2026-09-10 (#49 option (b) as it landed)

```text
=== CREATE TABLE
    CREATE TABLE d2 (id bigserial primary key, a int8, b text)
    16 reads: 16 point, 0 range
        2 x  get  layout
        2 x  get  name(t1,"d2")
        2 x  get  name(t1,"d2_id_seq")
        2 x  get  name(t1,"d2_pkey")
        2 x  get  next-id(t1)
        2 x  get  schema(t1,"esker")
        3 x  get  version(t1)
        1 x  get  version(t18446744073709551615)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  schema(t1,"esker")
        4. get  schema(t1,"esker")
        5. get  name(t1,"d2")
        6. get  name(t1,"d2_pkey")
        7. get  next-id(t1)
        8. get  name(t1,"d2_id_seq")
        9. get  next-id(t1)
       10. get  name(t1,"d2")
       11. get  name(t1,"d2_pkey")
       12. get  layout
       13. get  version(t1)
       14. get  name(t1,"d2_id_seq")
       15. get  layout
       16. get  version(t1)

=== ALTER TABLE ... DISABLE TRIGGER ALL
    ALTER TABLE d0 DISABLE TRIGGER ALL
    10 reads: 9 point, 1 range
        1 x  get  layout
        2 x  get  name(t1,"d0")
        2 x  get  schema(t1,"esker")
        1 x  get  table(t1,#16384)
        2 x  get  version(t1)
        1 x  get  version(t18446744073709551615)
        1 x  scan sequence(t1,#16384)..sequence(t1,#16384,…)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  schema(t1,"esker")
        4. get  schema(t1,"esker")
        5. get  name(t1,"d0")
        6. get  name(t1,"d0")
        7. get  table(t1,#16384)
        8. scan sequence(t1,#16384)..sequence(t1,#16384,…)
        9. get  layout
       10. get  version(t1)

=== DROP TABLE
    DROP TABLE d1
    22 reads: 12 point, 10 range
        1 x  get  layout
        2 x  get  name(t1,"d1")
        2 x  get  schema(t1,"esker")
        1 x  get  table(t1,#16384)
        2 x  get  table(t1,#16386)
        1 x  get  table(t1,#16389)
        2 x  get  version(t1)
        1 x  get  version(t18446744073709551615)
        1 x  scan fk-backref(t1,#16386)..fk-backref(t1,#16386,…)
        1 x  scan name(t1)..name(t1,…)
        1 x  scan row(t1,#16386)..sql-index
        1 x  scan sequence(t1,#16384)..sequence(t1,#16384,…)
        2 x  scan sequence(t1,#16386)..sequence(t1,#16386,…)
        1 x  scan sequence(t1,#16389)..sequence(t1,#16389,…)
        1 x  scan type(t1)..type(t1,…)
        2 x  scan view(t1)..view(t1,…)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  schema(t1,"esker")
        4. get  schema(t1,"esker")
        5. get  name(t1,"d1")
        6. get  name(t1,"d1")
        7. get  table(t1,#16386)
        8. scan sequence(t1,#16386)..sequence(t1,#16386,…)
        9. scan view(t1)..view(t1,…)
       10. scan name(t1)..name(t1,…)
       11. get  table(t1,#16384)
       12. scan sequence(t1,#16384)..sequence(t1,#16384,…)
       13. get  table(t1,#16386)
       14. scan sequence(t1,#16386)..sequence(t1,#16386,…)
       15. get  table(t1,#16389)
       16. scan sequence(t1,#16389)..sequence(t1,#16389,…)
       17. scan type(t1)..type(t1,…)
       18. scan view(t1)..view(t1,…)
       19. scan fk-backref(t1,#16386)..fk-backref(t1,#16386,…)
       20. scan row(t1,#16386)..sql-index
       21. get  layout
       22. get  version(t1)
=== pk_and_sequence_for
    SELECT attr.attname, nsp.nspname, seq.relname FROM pg_class seq, pg_attribute attr, pg_depend dep, pg_constraint cons, pg_namespace nsp WHERE seq.oid = dep.objid AND seq.relkind = 'S' AND attr.attrelid = dep.refobjid AND attr.attnum = dep.refobjsubid AND attr.attrelid = cons.conrelid AND attr.attnum = cons.conkey[1] AND seq.relnamespace = nsp.oid AND cons.contype = 'p' AND dep.classid = 'pg_class'::regclass AND dep.refobjid = '"pk0"'::regclass
    12 reads: 9 point, 3 range
        1 x  get  name(t1,"pg_attribute")
        1 x  get  name(t1,"pg_class")
        1 x  get  name(t1,"pg_constraint")
        1 x  get  name(t1,"pg_depend")
        1 x  get  name(t1,"pg_namespace")
        2 x  get  version(t1)
        2 x  get  version(t18446744073709551615)
        1 x  scan name(t1)..name(t1,…)
        1 x  scan schema(t1)..schema(t1,…)
        1 x  scan type(t1)..type(t1,…)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  name(t1,"pg_class")
        4. scan name(t1)..name(t1,…)
        5. scan type(t1)..type(t1,…)
        6. get  version(t1)
        7. get  version(t18446744073709551615)
        8. get  name(t1,"pg_attribute")
        9. get  name(t1,"pg_depend")
       10. get  name(t1,"pg_constraint")
       11. get  name(t1,"pg_namespace")
       12. scan schema(t1)..schema(t1,…)

=== a point select
    SELECT a FROM pk0 WHERE id = 1
    5 reads: 5 point, 0 range
        1 x  get  row(t1,#16384)
        2 x  get  version(t1)
        2 x  get  version(t18446744073709551615)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  version(t1)
        4. get  version(t18446744073709551615)
        5. get  row(t1,#16384)

=== an insert returning
    INSERT INTO pk0 (a, b) VALUES (2, 'y') RETURNING id
    5 reads: 5 point, 0 range
        1 x  get  row(t1,#16384)
        2 x  get  version(t1)
        2 x  get  version(t18446744073709551615)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  version(t1)
        4. get  version(t18446744073709551615)
        5. get  row(t1,#16384)

=== begin
    BEGIN
    0 reads: 0 point, 0 range
    in order:

=== savepoint
    SAVEPOINT s1
    0 reads: 0 point, 0 range
    in order:

=== release
    RELEASE SAVEPOINT s1
    0 reads: 0 point, 0 range
    in order:

=== commit
    COMMIT
    0 reads: 0 point, 0 range
    in order:

=== a point select, again
    SELECT a FROM pk0 WHERE id = 1
    5 reads: 5 point, 0 range
        1 x  get  row(t1,#16384)
        2 x  get  version(t1)
        2 x  get  version(t18446744073709551615)
    in order:
        1. get  version(t1)
        2. get  version(t18446744073709551615)
        3. get  version(t1)
        4. get  version(t18446744073709551615)
        5. get  row(t1,#16384)
```

## The raw trace, before

Kept exactly as it was taken, which is what makes the table above a measurement rather than a
claim. The DDL statements are not in it: they were added to the census on the same day, after it,
by `esker-coord/h1-ddl-cost.md`'s question, and their "before" numbers are in
`docs/plans/debt-49-catalog-cache.md`.

```text
────────────
=== pk_and_sequence_for
SELECT attr.attname, nsp.nspname, seq.relname FROM pg_class seq, pg_attribute attr, pg_depend dep, pg_constraint cons, pg_namespace nsp WHERE seq.oid = dep.objid AND seq.relkind = 'S' AND attr.attrelid = dep.refobjid AND attr.attnum = dep.refobjsubid AND attr.attrelid = cons.conrelid AND attr.attnum = cons.conkey[1] AND seq.relnamespace = nsp.oid AND cons.contype = 'p' AND dep.classid = 'pg_class'::regclass AND dep.refobjid = '"pk0"'::regclass
60 reads: 37 point, 23 range
    2 x  get  layout
    1 x  get  name(t1,"pg_attribute")
    1 x  get  name(t1,"pg_class")
    1 x  get  name(t1,"pg_constraint")
    1 x  get  name(t1,"pg_depend")
    1 x  get  name(t1,"pg_namespace")
   16 x  get  schema(t1,"esker")
    5 x  get  table(t1,#16384)
    2 x  get  version(t1)
    2 x  get  version(t18446744073709551615)
    1 x  get  view(t1,"pg_attribute")
    1 x  get  view(t1,"pg_class")
    1 x  get  view(t1,"pg_constraint")
    1 x  get  view(t1,"pg_depend")
    1 x  get  view(t1,"pg_namespace")
    5 x  scan name(t1)..name(t1,…)
    3 x  scan schema(t1)..schema(t1,…)
    5 x  scan sequence(t1,#16384)..sequence(t1,#16384,…)
    5 x  scan type(t1)..type(t1,…)
    5 x  scan view(t1)..view(t1,…)
in order:
    1. get  version(t1)
    2. get  version(t18446744073709551615)
    3. get  layout
    4. get  schema(t1,"esker")
    5. get  name(t1,"pg_class")
    6. get  schema(t1,"esker")
    7. get  schema(t1,"esker")
    8. scan name(t1)..name(t1,…)
    9. get  table(t1,#16384)
   10. scan sequence(t1,#16384)..sequence(t1,#16384,…)
   11. scan type(t1)..type(t1,…)
   12. scan view(t1)..view(t1,…)
   13. get  schema(t1,"esker")
   14. get  schema(t1,"esker")
   15. get  schema(t1,"esker")
   16. get  view(t1,"pg_class")
   17. get  schema(t1,"esker")
   18. get  view(t1,"pg_attribute")
   19. get  schema(t1,"esker")
   20. get  view(t1,"pg_depend")
   21. get  schema(t1,"esker")
   22. get  view(t1,"pg_constraint")
   23. get  schema(t1,"esker")
   24. get  view(t1,"pg_namespace")
   25. get  version(t1)
   26. get  version(t18446744073709551615)
   27. get  layout
   28. get  schema(t1,"esker")
   29. get  schema(t1,"esker")
   30. get  name(t1,"pg_attribute")
   31. get  schema(t1,"esker")
   32. get  name(t1,"pg_depend")
   33. get  schema(t1,"esker")
   34. get  name(t1,"pg_constraint")
   35. get  schema(t1,"esker")
   36. get  name(t1,"pg_namespace")
   37. get  schema(t1,"esker")
   38. scan schema(t1)..schema(t1,…)
   39. scan name(t1)..name(t1,…)
   40. get  table(t1,#16384)
   41. scan sequence(t1,#16384)..sequence(t1,#16384,…)
   42. scan type(t1)..type(t1,…)
   43. scan view(t1)..view(t1,…)
   44. scan schema(t1)..schema(t1,…)
   45. scan name(t1)..name(t1,…)
   46. get  table(t1,#16384)
   47. scan sequence(t1,#16384)..sequence(t1,#16384,…)
   48. scan type(t1)..type(t1,…)
   49. scan view(t1)..view(t1,…)
   50. scan name(t1)..name(t1,…)
   51. get  table(t1,#16384)
   52. scan sequence(t1,#16384)..sequence(t1,#16384,…)
   53. scan type(t1)..type(t1,…)
   54. scan view(t1)..view(t1,…)
   55. scan name(t1)..name(t1,…)
   56. get  table(t1,#16384)
   57. scan sequence(t1,#16384)..sequence(t1,#16384,…)
   58. scan type(t1)..type(t1,…)
   59. scan view(t1)..view(t1,…)
   60. scan schema(t1)..schema(t1,…)
=== a point select
SELECT a FROM pk0 WHERE id = 1
9 reads: 9 point, 0 range
    1 x  get  layout
    1 x  get  row(t1,#16384)
    4 x  get  schema(t1,"esker")
    1 x  get  version(t1)
    1 x  get  version(t18446744073709551615)
    1 x  get  view(t1,"pk0")
in order:
    1. get  schema(t1,"esker")
    2. get  schema(t1,"esker")
    3. get  view(t1,"pk0")
    4. get  version(t1)
    5. get  version(t18446744073709551615)
    6. get  layout
    7. get  schema(t1,"esker")
    8. get  schema(t1,"esker")
    9. get  row(t1,#16384)
=== an insert returning
INSERT INTO pk0 (a, b) VALUES (2, 'y') RETURNING id
8 reads: 8 point, 0 range
    1 x  get  layout
    1 x  get  row(t1,#16384)
    3 x  get  schema(t1,"esker")
    1 x  get  version(t1)
    1 x  get  version(t18446744073709551615)
    1 x  get  view(t1,"pk0")
in order:
    1. get  schema(t1,"esker")
    2. get  schema(t1,"esker")
    3. get  view(t1,"pk0")
    4. get  version(t1)
    5. get  version(t18446744073709551615)
    6. get  layout
    7. get  schema(t1,"esker")
    8. get  row(t1,#16384)
=== begin
BEGIN
0 reads: 0 point, 0 range
in order:
=== savepoint
SAVEPOINT s1
0 reads: 0 point, 0 range
in order:
=== release
RELEASE SAVEPOINT s1
0 reads: 0 point, 0 range
in order:
=== commit
COMMIT
0 reads: 0 point, 0 range
in order:
=== a point select, again
SELECT a FROM pk0 WHERE id = 1
9 reads: 9 point, 0 range
    1 x  get  layout
    1 x  get  row(t1,#16384)
    4 x  get  schema(t1,"esker")
    1 x  get  version(t1)
    1 x  get  version(t18446744073709551615)
    1 x  get  view(t1,"pk0")
in order:
    1. get  schema(t1,"esker")
    2. get  schema(t1,"esker")
    3. get  view(t1,"pk0")
    4. get  version(t1)
    5. get  version(t18446744073709551615)
    6. get  layout
    7. get  schema(t1,"esker")
    8. get  schema(t1,"esker")
    9. get  row(t1,#16384)
────────────
```
