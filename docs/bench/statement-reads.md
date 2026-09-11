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

## The shapes

| statement | before | after | after, cache warm |
|---|---|---|---|
| `pk_and_sequence_for` | **60** | 12 | **4** |
| `SELECT a FROM pk0 WHERE id = 1` | 9 | 5 | 5 |
| `INSERT INTO pk0 … RETURNING id` | 8 | 5 | 5 |
| `CREATE TABLE` | 17 | 16 | 16 |
| `ALTER TABLE … DISABLE TRIGGER ALL` | 17 | **10** | 10 |
| `DROP TABLE` | 29 | **22** | 22 |
| `BEGIN` / `SAVEPOINT` / `RELEASE` / `COMMIT` | 0 | 0 | 0 |

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

## The raw trace, after

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
