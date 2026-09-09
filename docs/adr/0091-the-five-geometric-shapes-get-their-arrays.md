# 0091 — The five geometric shapes get their arrays, and an array's `=` is its element's

Status: accepted · 2026-09-09

## Context

r1's 349-probe wire sweep filed six rows: `array_agg` and `ARRAY[…]` over `circle`, `path` and
`polygon` all lost their array type. The aggregate came back a **scalar `text`** (OID 25) and the
constructor a `text[]` (1009), where a real server declares `circle[]` (719), `path[]` (1019) and
`polygon[]` (1027). The same shape `name[]` was
([ADR 0086](0086-a-folded-cast-keeps-the-type-it-named.md)): an element type with no array to be
aggregated into falls back to `text`.

`tests/array_delimiter.rs` had carried "the five remaining geometric shapes" as **one** named gap
with one reason — `geometric_test.rb` declares no array of one — and `tests/geometric.rs` carried
the `typarray = 0` row for the same reason. The sweep made that reason false for three of the five.

## Decision

**1. All five, not the three that were probed.** `lseg` and `line` are built with `circle`, `path`
and `polygon`. Splitting one named gap 3/2 leaves a worse gap than it closes — the two that stayed
would have had no reason of their own, only "nobody probed them yet" — and the mechanism is
identical for all five. Oids measured one row at a time: `_lseg` 1018, `_path` 1019, `_polygon`
1027, `_circle` 719, `_line` 629, all delimiter `,`. `_box`'s `;` is still the only one that is not.

Additive on both tag spaces, as ADR 0086 was: catalog record tags 96–100, columnar tags 97–101, no
existing byte reinterpreted. Each array gets its **own comparison family**, which is every array's
rule here.

**2. An array's `=` needs its element's *btree* equality, and the refusal names the element.**
Measured for six element types at once:

```text
  '{…}'::circle[] = '{…}'::circle[]   42883 could not identify an equality operator for type circle
  the same for point, line, path, xml and json
  '{a}'::text[]   = '{a}'::text[]     t                                        <- the control
  '<…>'::circle   = '<…>'::circle     t                                        <- the scalar answers
```

The scalar `=` answering and the array `=` refusing is the same split
`ColumnType::Lseg`'s doc comment already recorded for `CREATE INDEX` and `count(DISTINCT)`. The list
is `value::has_equality_operator`, which `SELECT DISTINCT` and `count(DISTINCT)` already read, so a
type cannot be refused by one caller and answered by another.

**3. `min` and `max` do not exist over any of the seven shapes.** Measured one at a time —
`min(point)`, `min(box)`, `min(lseg)`, `min(path)`, `max(polygon)`, `min(circle)`, `max(line)` are
each `42883 function min(<type>) does not exist`. This node answered all seven; `point` and `box`
were wrong before this unit and are fixed with the five. It is the ninth entry on
[ADR 0031](0031-rails-compatibility-is-measured.md)'s list and the one that shows the rule best: an
`lseg`'s `=` **answers** and its `min` still does not exist, because an aggregate needs a btree
family and equality alone is not one.

## Consequences

* The six rows r1 filed answer PostgreSQL's OIDs over the wire, and so do `lseg[]` and `line[]`.
* `pg_type` gained five rows, so `ActiveRecord`'s array-type query answers forty-one rather than
  thirty-six — every one of the five element oids (601, 602, 604, 628, 718) was already in the
  adapter's fixed list, waiting for a row, the way 3614 and 19 were.
* `tests/array_delimiter.rs`'s list of base types with no array is down to four, and none of them is
  geometric: `int2vector`, `oidvector` and `regclass` are catalog types a client never stores an
  array of, and `lquery` is a pattern with no writer.
* `tests/geometric.rs`'s one `answers` entry moved to `types`: every value in that row agrees now,
  and what is left is the catalog's own columns — `oid`, `"char"` and `regproc` against `bigint` and
  `text` — each its own unit.
* **None of the five is an index key**, for `point[]`'s reason exactly: an array key is built out of
  its element's key encoding and a shape has none. A real server cannot *order* one either —
  `ORDER BY` over a `circle[]` is `42883 could not identify an ordering operator for type circle[]`,
  and note that this message names the **array** where the equality one names the **element**. Both
  measured, and both in `tests/corpus/pg19_order.txt`'s exclusion list rather than in a fixture.
* Every element quotes itself inside an array literal, because each of the five prints characters
  the array grammar reserves — including a `path`, whose **bracket is data**: `[…]` is open and
  `(…)` is closed, and both survive the round trip.
