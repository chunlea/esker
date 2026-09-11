# 0107 — A borrowed representation needs somewhere to carry its identity

* Status: **Proposed**
* Date: 2026-09-10
* Deciders: b4 (types), the coordinator
* Supersedes / relates: [0050](0050-a-user-defined-type-is-a-value.md),
  [0077](0077-regtype-is-an-oid-that-prints-as-a-name.md),
  [0103](0103-a-domain-is-a-type-a-client-can-be-sent.md)

## Context

Wire v3 family **F6** is thirty-six census rows over three types — `int2vector`, `oidvector` and
`lquery` — and one sentence: each of them is a `Datum::Text` here, so `Datum::column_type()` cannot
tell it from a string, and **every boundary that asks the value what it is gets `text`.**

The family shows through five shapes at once — `ARRAY[…]`, `array_agg`, a subscript, `unnest` and
`||` — because those are five boundaries, not five bugs.

### What a real server says these three are

Measured on 19beta1, 2026-09-10 (`pg_type` first, then behaviour):

```text
typname      typcategory  typlen  typelem     typarray       typinput        typoutput
int2vector   A            -1      smallint    int2vector[]   int2vectorin    int2vectorout
oidvector    A            -1      oid         oidvector[]    oidvectorin     oidvectorout
lquery       U            -1      -           lquery[]       lquery_in       lquery_out
int2         N            2       -           smallint[]     int2in          int2out
text         S            -1      -           text[]         textin          textout
```

**They are two different problems and the family name hides it.**

`int2vector` and `oidvector` are **arrays** — category `A`, with an element type — and they behave
like arrays wherever an array is asked for:

```text
('1 2'::int2vector)[1]                   2            -- and its type is smallint
array_length('1 2'::int2vector, 1)       2
unnest('1 2'::int2vector)                smallint
2 = ANY('1 2'::int2vector)               t
ARRAY['1 2'::int2vector]                 int2vector[]
array_agg('1 2'::int2vector)             int2vector   -- one more dimension of the same type
'1 2'::int2vector || '3 4'::int2vector   smallint[]   -- and the VALUE is [0:3]={1,2,3,4}
'1 2'::int2vector::int2[]                [0:1]={1,2}
```

**They are zero-based**, which is the detail every reader gets wrong: `(v)[1]` is the *second*
element, and casting to `int2[]` produces an array whose lower bound is `0`. And they are **not**
interchangeable with the plain array:

```text
'1 2'::int2vector = '{1,2}'::int2[]      !42883 operator does not exist
'{1,2}'::int2[]::int2vector              !42846 cannot cast type smallint[] to int2vector
```

`lquery` is none of that. It is an ordinary scalar of category `U` with its own I/O and an array
type, exactly like `hstore` or `tsquery` — and this node already has those. Its whole F6 share is
that **`ColumnType::LQueryArray` does not exist**.

### What it costs today

`Int2Vector` and `OidVector` are named in **fourteen** files across `esker-sql` and `esker-keys`,
and this queue has added to that count in four consecutive units:

* `exec::query::same_family` gives each of them a family of its own (96, 97) so that they compare
  with themselves and nothing else;
* `concat_pair` needed a rule saying they answer `text` beside themselves and **no** operator
  beside a string, because `anynonarray || text` would otherwise reach them (F3b);
* `array_of_void`'s gate had to be written as "is it `void`" rather than "does this node have an
  array for it", because `array_of` answers `None` for the vectors and a real server builds
  `int2vector[]` (F5);
* `common_of`'s verify pass leaves **56 of its remaining 100 shape-rows** on these two types,
  because `reaches_implicitly` cannot see that an `int2vector` reaches a `smallint[]` (F9);
* the quantified-array path already lists them beside the `*Array` family by hand, which is what
  makes `attnum = ANY(indkey)` work at all.

Every one of those is the same fact re-derived: **the plan knows what the value is and the value
does not.** ADR 0077 said it for `regtype`, ADR 0103 said it for a domain, and ADR 0050 said it
for a user-defined type. This is the fourth time.

## Options

### A. Leave it, and special-case each boundary as it is found

What has happened so far. Each unit is small and the count is fourteen files and rising; the
failure mode is that a boundary nobody has probed answers `text` and no test says so. Two of this
queue's families were exactly that discovery.

### B. Give the vectors a real array value and keep `lquery` a string

`Datum::Array` already carries an element type and a dimension list, and the vectors *are* arrays.
Model them as `Datum::Array { element: Int2 | Oid, lower_bound: 0 }`, with `ColumnType::Int2Vector`
and `OidVector` kept as the **declared** types so the wire oids and the input/output functions stay
theirs. `column_type()` then answers `int2vector` from the value, `element_of` answers `smallint`,
and the four special cases above collapse into the array rules that already exist.

The zero-based lower bound is the new thing: `ArrayValue` has `dims` and would need a lower bound
beside it, which is a change to a stored shape — and `esker-keys::columnar` and `row.rs` both name
these types, so the encoded form has to be considered rather than assumed.

`lquery` is untouched: it needs `ColumnType::LQueryArray` and nothing else — the name table, the
oid table, `ArrayValue::array_of`/`element_of`, `typcategory`, and the **reverse mapping in
`esker-keys::columnar`**, which is the one the compiler does not point at.

### C. A `Datum` variant per borrowing type

`Datum::Vector { element, values }` and `Datum::LQuery(String)`. Narrow, explicit, and it grows the
enum by one variant per type that borrows — which is the shape this ADR exists to stop.

## Decision

**Proposed: B**, in two steps that can land apart, and *not* as one unit.

1. **`lquery` first**, because it is a type and not a model: add `ColumnType::LQueryArray`, its wire
   oid, and its five places. It closes its share of F6 and it is measurable on its own.
2. **The vectors second**, behind their own measurement: the zero-based lower bound and the
   `esker-keys` encoding are the risk, and neither is visible from the SQL layer. That step should
   start by asking what a stored `int2vector` column round-trips as today — this node has one only
   in `pg_catalog`'s computed views, which is why the question has not come up.

## Consequences

* The four special cases listed above become dead code, and the tests that pin them
  (`tests/concat_operator.rs`'s `the_two_vectors_are_still_text`,
  `tests/array_of_void.rs`'s `the_three_arrays_this_node_does_not_have`) become red — which is how
  the work will be recognised as finished. Both were written so that they name this ADR.
* `attnum = ANY(indkey)` must keep working through the change; it is the one shape a schema dump
  depends on, and it is the reason the quantified-array path lists the vectors by hand today.
* **A `regclass` array is *not* part of this.** `'{t}'::text::regclass[]` is `0A000` here for a
  different reason — nothing resolves an array literal's element **names** in `Executor::bound` —
  and that is a pass, not a representation.
* If the vectors' step is deferred indefinitely, the honest cost is one line in each new boundary
  that meets them, and a test that says so. That is what the four units above did, and it is
  affordable; what is not affordable is doing it silently.

### Step 2, decided 2026-09-10 after measuring: the SQL-visible half, and **no byte moves**

Step 1 landed (`lquery`). Step 2 was written as *"give the vectors a real array value"* and its
opening instruction was to **start by asking what a stored `int2vector` column round-trips as
today**. Asked, on this node:

```text
CREATE TABLE vv (id bigint primary key, iv int2vector)   accepted
INSERT INTO vv VALUES (1, '1 2 3')                       accepted
SELECT iv::text FROM vv                                  1 2 3      it round-trips
SELECT array_length(iv, 1) FROM vv                       3
SELECT (iv)[0] FROM vv                                   1          already zero-based
SELECT array_lower('1 2 3'::int2vector, 1)               0          already zero
SELECT (iv::int2[])::text FROM vv                        42846      19beta1: [0:2]={1,2,3}
```

**Two of this ADR's own sentences were refuted by that, and both in the cheap direction.**

*"This node does not store an `int2vector` today, so the storage form is an open, cheap question"*
— **it stores one.** `catalog::record` has `TAG_INT2VECTOR = 91`, so a column can be declared, and
the value goes into the row as `Datum::Text`'s bytes. Giving the type an array value would change
what those bytes mean **under the same tag**, and the format version lives in the *catalog record*,
not in the *row* — so old bytes and new bytes would be indistinguishable. That is a migration, not
a representation change, and this ADR was explicit that it does not decide storage.

*"The zero-based lower bound … is the risk"* — **it is already right**: `array_lower` answers 0 and
`(iv)[0]` is the first element, both from the computed path, and `esker-keys`' row encoding has
persisted an array's lower bound since it was written (`row.rs`, "the lower bound is part of the
value"). So one of the two named risks does not exist and the other is larger than stated.

**Decided by the user 2026-09-10: step 2 is the SQL-visible half and moves no stored byte.**
`int2vector -> int2[]` (which 19beta1 answers `[0:2]={1,2,3}`), `typarray` and category `A`,
and the `= ANY`/`unnest` shapes listed above. **The storage form stays undecided**, which is what
this ADR said it would do — and now with the cost of deciding it later written down rather than
assumed: a tag that already has data behind it, a version number that cannot tell the two
representations apart, and therefore a migration or a second tag. Neither is a type question.

## What this ADR does not decide

Whether `int2vector`'s **stored** form changes. Nothing in this node stores one today, so the
question is open and cheap to answer later — and answering it now would be answering it without a
measurement.
