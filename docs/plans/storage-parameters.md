# Plan — storage parameters

**Claimed by the g1-ddl lane, 2026-09-04.** `crates/esker-sql/tests/corpus/pg19_storage_parameters.txt`
was the last unclaimed capture that nothing replayed.

## 1. The census

Eight statements, no transaction, so the file is replayable. Probed against the node:

| statement | PostgreSQL 19beta1 | this node |
|---|---|---|
| `ALTER TABLE t SET (columnar_replicas = 1)` | `22023 unrecognized parameter "columnar_replicas"` | **accepted** |
| `CREATE TABLE t (…) WITH (columnar_replicas = 1)` | `22023 unrecognized parameter` | `0A000 CREATE TABLE … WITH` |
| `ALTER TABLE t RESET (columnar_replicas)` | **ok** | `0A000 ALTER TABLE … RESET` |
| `ALTER TABLE t RESET (no_such_thing_at_all)` | **ok** | `0A000 ALTER TABLE … RESET` |
| `ALTER TABLE t SET (esker.columnar_replicas = 1)` | `22023 unrecognized parameter namespace "esker"` | **`42601` syntax error** |
| `ALTER TABLE t SET (toast.columnar_replicas = 1)` | **ok** | **`42601` syntax error** |
| `ALTER TABLE t SET (autovacuum_enabled = false)` | ok | `0A000 the storage parameter …` |
| `ALTER TABLE t SET (autovacuum_enabled = 'banana')` | `22023 invalid value for boolean option` | `0A000 the storage parameter …` |

**The first row is a deliberate divergence and the point of the file.** `columnar_replicas` is
Esker's own parameter ([ADR 0022](../adr/0022-columnar-learner-replica.md) Decision 5); PostgreSQL
has never heard of it and refuses it, this node accepts it. The capture exists to record what a real
server does *around* that choice, so the divergence is bounded and visible.

## 2. Two rows are a contract breach, and they outrank the rest

```text
ALTER TABLE t SET (toast.columnar_replicas = 1)   -- valid PostgreSQL, accepted there
ALTER TABLE t SET (esker.columnar_replicas = 1)   -- valid PostgreSQL syntax, 22023 there
```

Both answer **`42601 syntax error: Expected: =, found: .`** here. **Contract C1 is that no valid
PostgreSQL 19 statement is a parse error**, and a *namespaced* parameter name is valid syntax
whether or not the namespace exists — PostgreSQL proves it by answering one with `ok` and the other
with a *semantic* error, not a syntactic one.

This is the item with a contract behind it, so it is the one that gets fixed first.

## 3. The rest, and what each would cost

* **`RESET` accepts any name and always succeeds.** This is the capture's own headline: *"`RESET`
  of a parameter that has never existed anywhere is accepted, while `SET` of the same name is an
  error. The asymmetry is not symmetry with a hole in it — `RESET` does not validate names at
  all."* Cheap, and exactly reproducible.
* **`autovacuum_enabled` and friends** — a real storage-parameter surface with per-parameter types
  and validation (`'banana'` is `22023 invalid value for boolean option`). That is a unit of its
  own and nothing in the suite reaches it.
* **`CREATE TABLE … WITH (…)`** — the same surface at creation time. Same unit.

## 3b. Both items need parse-level work, and that is measured

Neither is a lowering fix, which is the thing to know before sizing them:

* **`sqlparser` 0.62.0 has `AlterTableOperation::SetOptionsParens` and no table-level
  `ResetOptionsParens`.** `ALTER TABLE t RESET (x)` cannot be parsed at all, so it has to be read
  from the source the way `REFRESH MATERIALIZED VIEW` is (`crate::parse::read_refresh`). Today it
  is caught earlier still, by the refusal table's `u("ALTER TABLE ... RESET", …)` row.
* **A namespaced parameter is a parse error inside `SetOptionsParens`** — `Expected: =, found: .`
  — so it needs a source rewrite that lifts the namespace out before `sqlparser` sees it, the shape
  `strip_domain_not_null` and `strip_with_data` already use.

So this is two `parse`-layer readers plus their lowering, not two match arms. Still small, and the
estimate is now from the parser's own vocabulary rather than from a guess.

## 4. Scope taken now

1. **The C1 breach**: a namespaced parameter name parses. `toast.` is accepted as PostgreSQL
   accepts it; a namespace this node does not know is `22023 unrecognized parameter namespace "…"`,
   which is PostgreSQL's own sentence rather than a syntax error.
2. **`RESET`**: accepted, validating nothing, as measured.
3. **Replay the capture** with the remaining divergences declared and their reasons written.

Out: the storage-parameter surface itself (`autovacuum_enabled`, `WITH` at creation). Declared, with
the note that nothing in the Rails suite reaches them — this file is `ALTER TABLE` shape work, not a
parameter catalogue.
