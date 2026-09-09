# 0097 — An `oid` is four bytes, and a derived one is not

Status: accepted · 2026-09-09

## Context

Every `pg_catalog` corpus in this tree carried the same sentence: a real server's `pg_class.oid`,
`pg_type.oid`, `attrelid`, `atttypid` are `oid` (26) and this node answered `bigint` (20). The
census is one query — 308 columns on PG 19beta1, 46 distinct names — and it is in
`tests/captures/pg19_oid_type.txt`.

The type itself already existed: [ADR 0077](0077-regtype-is-an-oid-that-prints-as-a-name.md) built
`ColumnType::Oid` because a `regtype` *is* an oid underneath. What was missing was the declaration.

## Decision

**A view's rows carry the types its column list declares.** `CatalogView::rows_of` now coerces
each row against `columns()` before returning it, so a builder that writes a `Datum::Int8` — which
every one of them does, because an id is a `u64` here — cannot leave a row disagreeing with its own
`RowDescription`. One coercion rather than a cast in forty builders: the declaration is the single
place that decides, and a builder added later inherits the rule instead of having to know it.

**The rule is one sentence: a column that names a *relation* stays `bigint`, and every other oid
column is an `oid`.** 43 columns are declared; the relation-naming ones are not. Two things put a
relation's oid outside four bytes, and both are arithmetic rather than counting:

| family | where it comes from | first bit |
|---|---|---|
| primary key's index | `pg_relations::PRIMARY_KEY_OID_BASE + table_id` | 62 |
| check / not null / foreign key / exclusion | `pg_constraint`'s four bases `+ (id << 16) + at` | 56–61 |
| a catalog view's own id | `VIEW_ID_BASE`, near `i64::MAX` | 62 |

Everything else — a type, a namespace, a collation, a language, a procedure, a role, a database,
an access method, an advisory key's half — comes from a counter that starts at
`catalog::FIRST_USER_ID` (16384) and increments, and fits with room to spare.

**The rule is about queries, not about stored values.** Three of the four witnesses that decided it
were not saturated rows:

* `no_declared_oid_column_saturates` named `pg_class.oid` and `pg_attribute.attrelid` — those two
  really do carry a `PRIMARY_KEY_OID_BASE` value, because a primary key's index has no record of
  its own and its columns are in `pg_attribute`.
* `ActiveRecord`'s serial-sequence join — `WHERE dep.classid = 'pg_class'::regclass` — was
  `22003 value "9223372036854774786" is out of range for type oid`. Nothing was stored wrong; the
  **comparison** coerced a catalog view's id into four bytes.
* `pg_catalog_namespace`'s `WHERE i.indrelid = '"pg_type"'::regclass` failed the same way, and its
  refusal then swallowed the rest of the corpus file.

So a column is judged by what a client may compare it against, not by what this fixture happens to
hold: `pg_attrdef.adrelid`'s values all fit and it is a `bigint` all the same, because
`WHERE adrelid = 'pg_class'::regclass` is a query somebody may write.

**Closing the space is a separate unit**, and it is a change to how an oid is *allocated* rather
than to how it is declared: five regions carved out of `u32` leave about 19 bits for the table id
once each keeps 10 for its position, which bounds a tenant at roughly half a million relations
ever. PostgreSQL solves the same problem by allocating from a wrapping counter and checking for a
collision, not by deriving. Recorded here so the next reader has the numbers.

## What `oid` is, measured

* **`typcategory` is `N`** — numeric, with the integers. `CASE WHEN true THEN 1::oid ELSE 1::int8
  END` is an `oid`, where the same shape over a `"char"` is `42804`
  ([ADR 0095](0095-char-is-one-byte-and-the-quotes-are-part-of-the-name.md)).
* **`min`/`max` keep it**, which is where it parts company with `varchar`, `name`, `cidr` and
  `"char"` — all four of those decay to `text`. `count` is a `bigint`; **`sum(oid)` does not
  exist**, and neither does `oid + integer`. An oid is an identifier, not a number to do sums with.
* **`oidin` is C's `strtoul` with base 0, and `int4in` is not.** The same digits are different
  numbers in the two types:

  ```text
    '010'::oid    8        '010'::int4   10      <- a leading zero is octal to one, nothing to the other
    '0x10'::oid   16       '0x10'::int4  16
    '0o17'::oid   22P02    '0o17'::int4  15
    '0b101'::oid  22P02    '0b101'::int4 5
    '1_000'::oid  22P02    '1_000'::int4 1000
    '08'::oid     22P02                          <- 8 is not an octal digit
  ```

* **Unsigned, and the two ends are not symmetric.** `'-1'::oid` and `(-1)::int4::oid` are
  `4294967295` — the bits, reinterpreted — while `(-1)::int8::oid` is `22003 OID out of range` and
  `'4294967296'::oid` is `22003 value … is out of range`.

## Consequences

* **31 declared divergences deleted across 13 files**, all named by the ratchet.
* **`26::oid` used to be `0A000`.** `cast_operand` accepted a single-quoted string and nothing
  else, so `'26'::oid` answered and the spelling a person writes did not. It now asks
  `cast_literal_text`, which already knew every shape a literal has here — a number, a signed
  number, a bit string, a folded cast chain
  ([ADR 0086](0086-a-folded-cast-keeps-the-type-it-named.md)). Third time in this crate that one
  grammar had two readers and the narrow one was the bug.
* **A `regtype` in a comparison started resolving.** `tests/domain_schema.rs` declared that
  `WHERE contypid = 'ds_ci'::regtype` was `22P02`, because a bare `::regtype` lowers to the type's
  name and a name cannot be compared with a `bigint`. `contypid` is an `oid` now, and a `regtype`
  beside an `oid` is one representation rather than two. The same limitation over
  `pg_enum.enumtypid` closed with it.
* `pg_locks.classid` and `objid` *are* `oid`s, and `pg_depend`'s columns of the same names are not:
  an advisory key is already split into two 32-bit halves here, and a `pg_depend` classid names a
  catalog. Two pairs, one spelling, opposite answers.
