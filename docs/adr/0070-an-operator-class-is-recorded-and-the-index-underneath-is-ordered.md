# 0070 — An operator class is recorded, and the index underneath is ordered

## Context

`schema_test.rb` has two tests about **operator classes**, and between them they are 15 of the
suite's failures across three files:

```ruby
@connection.execute "CREATE INDEX trains_desc ON trains USING btree(description text_pattern_ops)"
assert_match(/opclass: \{ description: :text_pattern_ops \}/, dump_table_schema("trains"))

@connection.enable_extension("pg_trgm")
@connection.execute "CREATE INDEX trains_position ON trains USING gin(position gin_trgm_ops)"
assert_match(/opclass: :gin_trgm_ops/, dump_table_schema("trains"))
```

`ActiveRecord`'s schema dumper reads an index's operator classes and writes them back out, so a
node that cannot record one cannot round-trip a schema that has one. **The extension is the
smaller half**: `text_pattern_ops` is built in and needs the same machinery without it.

Measured on 19beta1:

| | |
|---|---|
| `pg_trgm` | version **1.6** |
| its classes | `gin_trgm_ops` (access method `gin`) and `gist_trgm_ops` (`gist`), `opcdefault = f`, `opcintype` `text`, oids above 16383 |
| built-in classes | `text_pattern_ops` and `varchar_pattern_ops`, each for **both** `btree` and `hash` |
| `pg_am` | `btree`, `gin`, `gist`, `hash`, `brin`, `spgist`, all `amtype = 'i'` |
| `pg_index.indclass` | one class per key column, with the **default** filled in where none was written: `USING btree (name, position text_pattern_ops)` is `text_ops, text_pattern_ops` |
| `pg_get_indexdef` | `CREATE INDEX trains_position ON public.trains USING gin ("position" gin_trgm_ops)` |

and three refusals, each its own sentence: `USING gin(name)` with no default class is
`42704 data type character varying has no default operator class for access method "gin"`;
`USING btree(name gin_trgm_ops)` is `42704 operator class "gin_trgm_ops" does not exist for access
method "btree"`; `USING btree(id text_pattern_ops)` is `42804 operator class "text_pattern_ops"
does not accept data type bigint`.

Every index in this node is a range of the ordered key space, which is what a btree is
(`esker_keys::row`). A GIN trigram index is a different structure: it holds the three-character
substrings of a value, and it exists so that `LIKE '%ron%'` can be answered without a scan. This
node has neither that structure nor a planner that would choose it.

## Options

1. **Refuse `USING gin`**, as today (`0A000 an index USING gin`). Honest about the structure, and
   the 15 tests stay red — including the two that never mention `pg_trgm`, because the opclass
   machinery is what they actually need.
2. **Build a real GIN trigram index.** A second index structure, its own key encoding, its own
   planner rule, and a maintenance path on every write. Weeks, for a feature no correctness
   argument needs: every query it would accelerate is already answered correctly by a scan.
3. **Record the access method and the operator class, and build the ordered index underneath.**
   The catalog says exactly what the user asked for; the storage is what this node has.

## Decision

**Option 3.** An index's access method and per-column operator class are recorded in the catalog —
`pg_opclass` becomes a view, `pg_index.indclass` carries one class per key column with the default
filled in, and `pg_get_indexdef` prints both — while the index built underneath is the ordinary
ordered one every index here is.

The divergence is one sentence and it is declared where a reader will meet it: **an index this node
reports as `USING gin (col gin_trgm_ops)` is stored as an ordered index, and nothing claims it
accelerates a trigram search.** No plan mentions it, `EXPLAIN` never names a trigram scan, and a
`LIKE '%ron%'` is answered by the scan it was already answered by.

The three refusals above are implemented, because they are what makes the recording honest: a class
that does not exist for the method, or does not accept the column's type, is refused rather than
written down. A `CREATE INDEX` that a real server rejects must not be a row in this catalog.

## Consequences

* **What a schema dump round-trips is what was written.** That is the whole of what the 15 tests
  check, and it is a property of the catalog rather than of the storage.
* **The catalog can describe an index whose structure is not what the method names.** That is the
  cost, and it is the reason this is an ADR: a future reader who trusts `pg_am` to describe the
  *implementation* will be wrong. It describes the *declaration*. `EXPLAIN` is the thing that
  describes the implementation, and it is unchanged.
* **`pg_trgm` joins the extension allowlist** at 1.6 — under the allowlist's own rule, which is
  that `CREATE EXTENSION` means the extension *and* what it promises. What it promises here is the
  two operator classes; its functions (`similarity`, `%`, `word_similarity`) are **not** built and
  are a named gap. No test in `activerecord/test` uses one.
* **`text_pattern_ops` and `varchar_pattern_ops` come with it**, since they are the same machinery
  and are built in. Their comparison is C-collation byte order, which is what this node's keys
  already are — so for those two the recording is not a divergence at all.
* If a trigram index is ever built for real, this decision is what it replaces: the catalog surface
  stays and the storage under it changes, which is the direction that costs nothing to reverse.
