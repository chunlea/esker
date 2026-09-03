# 0047 — An array is a column type, over one element type

## Context

`postgresql_specific_schema.rb` declares `bigint_array` with `int8[]` and `numeric[]` columns and a
`numeric[]` **default** of `[1.23, 3.45]`. Against this node, `ActiveRecord` failed *client-side* —
`TypeError: can't quote Array` — because the adapter decides a column is an array by reading
`pg_type.typinput == 'array_in'`, and no type here answered that. Under it:

```
CREATE TABLE arrcol (a int8[])     →  0A000 the type INT8[] is not supported
```

`= ANY` over an array *expression* has worked since phase 6a; an array as a **stored type** did
not. It is the second of two blockers standing between this node and the Rails suite's 426 files.

There was a second, quieter problem. An array literal was never *read*: a cast to an array type was
the identity on its text, so `'{a,,b}'::text[]` answered `{a,,b}` where a real server raises
`22P02`, and `'{a,b}'::text[]` reported its type as `text`. That is ADR 0031's worst class —
answering where PostgreSQL raises — and it was invisible while there was no array type to compare
against.

## Options

**`ColumnType::Array(Box<ColumnType>)`.** The truthful shape: an array is a constructor over a
type, not a scalar. `ColumnType` is `Copy` and passed by value in several hundred places across
five crates; making it recursive puts a `Box` in every one of them and an allocation in paths that
currently copy four bytes.

**A flat variant per element type.** `Int8Array`, `Int4Array`, `NumericArray`, `TextArray` — the
four `ActiveRecord`'s schemas declare. Not extensible without an edit, and each new one costs a
variant, a wire tag and an ordering fixture.

**Keep storing arrays as text**, as `pg_index.indkey` and `pg_constraint.conkey` are. It is what
the array operators already read. It cannot answer `ORDER BY` — `{10}` and `{9}` sort by their
characters — and it cannot make a unique index correct.

## Decision

**Flat variants, four of them, and a real value behind them.**
`Datum::Array(ArrayValue { element, lower, dims, values })` is PostgreSQL's own model, and each
part earns its place:

* the elements are **flat and row-major**, so a two-dimensional array is one element sequence with
  a shape — which is why `('{{1,2},{3,4}}')[1]` is NULL rather than `{1,2}`;
* the shape is a **list of lengths, empty for an empty array**, which is what makes
  `array_length('{}', 1)` NULL where `cardinality('{}')` is 0;
* the **lower bound is part of the value**: `'[0:2]={1,2,3}'` prints back as `[0:2]={1,2,3}`, and
  it is the last tiebreak in the order — `[0:1]={1,2}` sorts before `{1,2}`, measured.

The element type lives **in the value**, so a datum can say what it is, and every question about an
element is the element type's to answer: `'{2147483648}'::int[]` fails with `int4`'s own overflow
message, and `'{1,x}'::int[]` with `int4`'s input error, while `'{a,,b}'` is the *array's*
`22P02 malformed array literal` with a `DETAIL` naming the character. Four DETAILs, measured.

The index key reproduces `array_cmp`: element by element with `0x01` before a value and `0xFF` for
a NULL — so **a NULL element sorts above every value** — then `0x00`, so a prefix sorts first, then
the shape and the lower bound as tiebreaks. The row encoding writes the shape rather than deriving
it, because an empty array's absence of dimensions cannot be recovered from its absence of
elements.

## Consequences

* Twenty-one files carry a new match arm, which is how the compiler listed what had to decide
  something. Two decided to **refuse**: the columnar format has no array run, so a query over an
  array column routes to the row engine (`NotExpressible`) and a columnar replica of such a table
  is refused where its decoder is built. Both doors on one rule, and neither is a wrong answer.
* `pg_type` gains four rows without being edited — it derives from `ColumnType::ALL` — and
  `typinput` answers `array_in`, which is the string `ActiveRecord` reads. Its array type-map query
  returned nothing before this and returns four rows now.
* The OIDs are **not** repeated: `ColumnType::Int8Array.oid()` is `array_oid(Int8)`, which is where
  1016 and 1007 and 1231 already lived. The two cannot disagree.
* A binary-format array is refused in both directions, as a `numeric` is: `array_send`'s shape has
  never been read here, and guessing it would put bytes on the wire that no client asked for.
* Four element types is a decision to revisit, not a limit of the design: a fifth is a variant, a
  tag, a `pg_type` row that appears by itself, and an ordering fixture.
