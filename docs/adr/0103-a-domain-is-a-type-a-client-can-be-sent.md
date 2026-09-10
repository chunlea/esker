# ADR 0103 — A domain is a type a client can be sent

Status: **proposed** (2026-09-09) · Numbered 0103 by the coordinator; 0102 is h1's draft on `main`.
**No decision is made here.** This records what the shapes are and what each costs, so the
milestone is the user's to set. Debt `debts-v1.1.md` #37 is this row.

## Context — the values are already right; the *name* is not

`CREATE DOMAIN` works. A domain's `NOT NULL`, `DEFAULT` and `CHECK (VALUE …)` are enforced, its
`23502` and `23514` name the domain, and `catalog::TypeKind::Domain` carries `base`, `typmod`,
`not_null`, `default` and `check`. A column declared as one stores exactly what its base type
stores, because **a domain adds a constraint, not a representation.** Every row this node returns
for such a column is the row a real server returns.

What differs is the type identity a client is told. Measured on 19beta1:

```text
pg_typeof(table_name)             information_schema.sql_identifier      here: name
pg_typeof(array_agg(table_name))  information_schema.sql_identifier[]    here: name[]
```

`information_schema` is built out of five of them, and each has an array type of its own:

| domain | oid | base | array oid | array |
|---|---|---|---|---|
| `cardinal_number` | 13356 | `int4` | 13355 | `_cardinal_number` |
| `character_data` | 13359 | `varchar` | 13358 | `_character_data` |
| `sql_identifier` | 13361 | `name` | 13360 | `_sql_identifier` |
| `time_stamp` | 13367 | `timestamptz` | 13366 | `_time_stamp` |
| `yes_or_no` | 13369 | `varchar` | 13368 | `_yes_or_no` |

**Those numbers are not constants to copy.** They are assigned when `information_schema` is
created, not fixed by catalog convention, so a node that hard-codes 13361 is pinning one build of
one server. What a client actually does is read the oid off the wire and ask `pg_type` what it is —
so the node may assign its own, exactly as it already does for an enum, provided its `pg_type`
agrees with what it sent.

### What already exists, and what the gap really is

The gap is narrower than "a domain has nowhere to live on the wire", and the difference matters for
sizing:

* **The wire road exists.** `pgwire::message::FieldDescription::of_user_type(name, oid, size)`
  sends a user-defined type's own oid with a rendered length. Enums use it (ADR 0050).
* **The catalog road exists.** `pg_attribute.atttypid` already answers `ColumnDef.user_type` when a
  column has one, so `format_type`, the `JOIN pg_type` a client writes, and
  `information_schema.columns`'s `USER-DEFINED` all work for any user type.
* **A column already remembers.** `ColumnDef` carries `ty` (the base, which is what the value *is*)
  beside `user_type: Option<u64>` (the identity, which is what the catalog reports).

So the missing pieces are two, and neither is the wire format:

1. **The lookup is `table.enums`, not "any user type".** `exec::bind` resolves a column's
   user-defined identity by `column.user_type.and_then(|oid| table.enums.get(&oid))`, so a domain's
   id finds nothing and the field falls back to the base type's oid.
2. **The identity survives only where a `ColumnDef` is at hand.** `ColumnType` is what the query
   pipeline carries — resolution, expression typing, aggregation, arrays — so the moment a value
   passes through anything that is not a bare column reference, the domain is gone. This is why
   `array_agg` is the hard half rather than an extra case: `_sql_identifier` is a *different* oid,
   and nothing in the pipeline is holding an identity to derive it from.

## The three shapes

### A. A domain oid table beside the enums, and a mapping at the exit

Give domains the same treatment enums have: a catalog table keyed by the domain's oid, and a lookup
at the point a `FieldDescription` is built. The query pipeline keeps carrying `ColumnType`; the
identity is re-attached at the exit from the `ColumnDef` the output column came from.

* **RowDescription** — right for a bare column reference (`SELECT table_name FROM …`), which is
  what `information_schema` queries mostly are. Wrong the moment the column is wrapped in anything.
* **pg_type** — one row per domain, `typtype = 'd'`, `typbasetype` the base. Straightforward: the
  catalog already knows all of it.
* **Array types** — **not solved.** `array_agg` produces a value whose type is computed in the
  pipeline, where no identity is carried, so `_sql_identifier` has nothing to come from. This shape
  buys the scalar and leaves the array.
* **ActiveRecord** — reads the oid, finds `typtype = 'd'` in `pg_type`, resolves through
  `typbasetype`, and decodes as the base. That is what it does against a real server, so the scalar
  case becomes indistinguishable.
* **Cost** — small, and mostly in one place. It is the shape that fits in a sprint.

### B. `ColumnType` carries a domain id

Make the identity part of the type the pipeline carries: a `ColumnType::Domain(id)` variant, or a
`ColumnType` paired with an `Option<u64>` everywhere it travels.

* **RowDescription** — right everywhere, including through expressions and aggregates.
* **pg_type** — same one row per domain as A.
* **Array types** — solved *if* the array constructor derives `_d` from `d`, which is the same rule
  `array_agg` already applies to base types. This is the only shape that reaches
  `pg_typeof(array_agg(x))`.
* **ActiveRecord** — as A, plus the array case, which is the one that actually bit: run 106 lost ten
  tests to an array whose declared type was wrong (ADR 0086), and it was invisible to `psql` and to
  `pg_typeof`.
* **Cost** — **large, and it is the closed enum that makes it large.** `ColumnType` is matched
  exhaustively across the crate — the row codec's six stored types, the columnar mapping, every
  `match` in typing and evaluation. [ADR 0050](0050-a-user-defined-type-is-a-value.md)'s "a new
  type touches five places" and [ADR 0077](0077-regtype-is-an-oid-that-prints-as-a-name.md)'s "its
  five places plus the columnar reverse map, which the compiler will not point at" are both about a
  *storage* type, which a domain is not — so the list is a lower bound here, not the shape. Whichever form it takes, every one of those sites
  has to answer "and if it is a domain, use the base" or it silently answers about a name rather
  than about a value. That is the risk to weigh: the failure mode is a value handled by its label.

### C. Declare the divergence, and say so where a reader looks

Keep the base type on the wire, record the difference against the five `information_schema`
domains and any user-created one, and let a client see `name` where a real server says
`sql_identifier`.

* **RowDescription** — the base type's oid, always.
* **pg_type** — the domain row can still exist (it costs nothing and makes `\dD` and a catalog join
  right); what the wire sends simply does not point at it.
* **Array types** — unchanged, `name[]` for `_sql_identifier`.
* **ActiveRecord** — decodes by the base oid and gets the right *value* every time. What it loses
  is `udt_name` and anything that branches on `typtype = 'd'`. No Rails test in this suite is known
  to do that today; group 10 of r1's wire-108 baseline is this row and nothing else.
* **Cost** — none, and it is honest as long as it is written down. The risk is the one this project
  has already been bitten by three times: *right bytes, wrong declared type* is invisible until a
  client decodes by the oid, and then it is ten tests at once.

## What is not in question

A domain is **not** a storage type, whichever shape is chosen. `esker-keys`'s row codec keeps its
six stored types (ADR 0030) and a domain column keeps writing its base's bytes. Nothing here
reaches the engine.

## For the milestone

The three shapes are not a ladder — A does not become B by adding to it, because A deliberately
does not carry the identity through the pipeline and B is exactly that carrying. Choosing A and
later wanting arrays means doing B anyway.

Both halves of that question have now been measured against the captured suites rather than
guessed. Sources: `esker-rails-harness/results/` and `triage/` (the `log_statement` captures, 378 MB),
and the ActiveRecord checkout the suite runs from, `esker-rails-harness/rails/`.

### Who branches on `typtype = 'd'`

**ActiveRecord does, on every connection, and it asks for domains by name.** Its type-map load is

```sql
SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, r.rngsubtype, t.typtype, t.typbasetype
FROM pg_type as t LEFT JOIN pg_range as r ON oid = rngtypid
WHERE t.typtype IN ('r', 'e', 'd')
```

— **712 occurrences across 164 captured files**. It selects `typbasetype` beside `typtype` for
exactly one reason: a domain's oid is registered against its *base type's* decoder.

That is the constraint the two building shapes have to meet, and it is sharper than "add a row to
`pg_type`": **if this node ever sends a domain's oid on the wire, that oid must come back from
this query, or ActiveRecord has no decoder for it** and falls back to a string. Shape C never
sends one, which is why it is safe today without anything being added at all.

Nothing else in the corpus branches on it. `domain_name` appears **zero** times.
`information_schema.domains` appears **once**, in `results/run-77/provenance.txt:18`, and it is one
of this project's own corpus probes rather than a client statement.

### How the suite reads a domain column

**Bare, and only twice, and neither read is about the type.**

* `activerecord/test/cases/adapters/postgresql/timestamp_test.rb:202` asserts
  `{"data_type" => "USER-DEFINED", "udt_name" => "custom_time_format"}` from
  `select data_type, udt_name from information_schema.columns where column_name = 'times'`. Both
  columns are domains (`character_data`, `sql_identifier`) and both are selected bare — but what
  the test checks is an **enum's name**, a value. It would pass against a node that answered
  `name` for the column's own type.
* `activerecord/lib/active_record/connection_adapters/postgresql/referential_integrity.rb:53`
  reads `constraint_name`, `table_schema` and `table_name` from
  `information_schema.table_constraints` — three `sql_identifier` domains, and it **wraps all three
  in `format()`**. A wrap is the case shape A gets wrong, and it does not matter here: `format()`
  returns `text` on both servers, so the domain is gone on the real one too.

**No statement in the suite puts a domain column into an aggregate.** The only one in the corpus is
`array_agg(table_name)`, which is r1's own wire probe — and r1 recorded the verdict beside it in
`results/run-107.md:43`: *"ACCEPTED. PG's `information_schema` column is a DOMAIN over `name`; the
node returns the base array type. An improvement on `text`, and ActiveRecord decodes 1003 as an
array either way."*

### What that leaves

The array half is what shape **B** exists for, and the wire sweep has already accepted it. The
scalar half is what shape **A** buys, and the two suite reads that touch it are a value assertion
and a `format()` wrap. So **nothing measured here is failing today for want of a domain type**, and
the risk runs the other way: sending a domain oid that the type-map query above does not return
would turn a correct value into a string.

That does not make C the answer — `udt_name` is one schema-dumper change away from mattering, and
this project has been bitten three times by *right bytes, wrong declared type* (ADR 0086). It makes
the milestone a **choice about when**, with no test currently forcing it, which is what a milestone
the user sets should look like.
