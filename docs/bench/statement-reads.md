# What one statement reads below the SQL

Taken 2026-09-10 on the in-process node (`MemoryBackend`) with `ESKER_STMT_STATS=1`
`ESKER_STMT_STATS_TRACE=1`, by `crates/esker-sql/tests/statement_reads.rs`. Fixture: one
user table, `CREATE TABLE pk0 (id bigserial primary key, a int8, b text)`, with one row.

**Topology-independent by construction.** Which keys a statement reads is a property of the
catalog code; only the *round-trip* count depends on the client and its buffer, and that
number stays r1's to take on a real cluster. So the counts below are **KV reads at the store
boundary**, which is where `Txn::get` and `Txn::scan` are the only two doors.

## The four shapes

| statement | reads | point | range |
|---|---|---|---|
| `pk_and_sequence_for` | **60** | 37 | 23 |
| `SELECT a FROM pk0 WHERE id = 1` | **9** | 9 | 0 |
| `INSERT INTO pk0 … RETURNING id` | **8** | 8 | 0 |
| `BEGIN` / `SAVEPOINT` / `RELEASE` / `COMMIT` | **0** | 0 | 0 |

## The raw trace

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
