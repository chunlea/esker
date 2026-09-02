# 0045. `numeric` is a decimal, and its key is the normalised one

* Status: accepted
* Date: 2026-09-02
* Supersedes: nothing. Extends ADR 0033 (tier 1 of the type surface) and ADR 0030 (the row codec).

## Context

`numeric` is statement 389 and 474 of Rails' `schema.rb` and the tenth of the ten type names
`ActiveRecord`'s first boot query asks `pg_type` for. ADR 0033 called it "tier 2's hard half",
and it is hard for one reason: **it is the only type this node stores whose text is part of its
value and whose value is not its text.** `1.0` and `1.00` print differently and compare equal;
`1230` and `123`-at-scale-`-1` print the same and are one number written two ways.

A `numeric` is also the first type here that must be orderable without being fixed width. Every
other stored type is either a fixed-width integer with a total order or a byte string ordered by
its bytes. A decimal is neither: `"10" < "9"` as bytes and `10 > 9` as numbers.

## Options

1. **`f64` underneath.** Cheap, wrong. `0.1` is not representable, PostgreSQL's `numeric` is
   arbitrary precision, and the declared scale is observable in every value the type prints.
2. **`i128` with a fixed scale.** Exact within a range and wrong outside it. `numeric` with no
   typmod has no range, and Rails writes plain `numeric` columns.
3. **Digits and a scale, with the written text preserved.** What PostgreSQL does. Chosen.

## Decision

`esker_keys::numeric::Numeric` is `NaN | PosInfinity | NegInfinity | Finite(Decimal)`, and
`Decimal` is `{ negative, digits: Vec<u8>, scale: i32 }` — one digit per byte, most significant
first, `sign × digits × 10^-scale`. A **negative scale multiplies**, which is how `numeric(10,-2)`
stores `12300` as three digits. It lives in `esker-keys` rather than `esker-sql` because
`esker-store` renders a columnar value and cannot see `esker-sql` (ADR 0022).

**The row encoding preserves what was written**: a kind byte, then for a finite value a zigzag
scale and one byte per digit. `1.0` and `1.00` round-trip as themselves, because the trailing
zeros are the declared scale and printing them is the whole point of the type.

**The key encoding normalises first, and that is the difference between the two.** An index key
is five ordered groups — `-Infinity` < negative < zero < positive < `Infinity` < `NaN` — and
inside a finite group the order-preserving `i64` exponent (`codec::encode_i64`, big-endian with
the sign bit flipped) comes ahead of the digits, each written as `digit + 1` with a `0`
terminator so that `1` sorts before `11`. The whole body is complemented when the value is
negative. `NaN` is the top: **it equals itself and sorts above `Infinity`**, PostgreSQL's
deliberate departure from IEEE, and `float8` makes the identical one — measured, after a test in
this unit assumed the IEEE contrast and was corrected by the oracle.

In the columnar file a `numeric` **rides the byte run as its text**. Its statistics bounds are
therefore byte bounds of that text, and their order is not the type's. Nothing prunes yet; the
first pruner that does must decode both bounds and compare with `numeric`'s ordering, or skip
this type. The warning is written at `stats::ColumnStats::fit`, beside the list that puts it
there.

On the wire it is **text only**. `numeric_recv` reads a four-`i16` header and base-10000 digit
groups; nothing here has ever sent or read that shape, so both directions refuse with the `0A000`
that names the type rather than guessing a format.

## Consequences

* A `numeric` can be a primary key, an index key and a `GROUP BY` key, because the key encoding
  is total and order-preserving. The property that says so is
  `row::tests::a_numeric_key_sorts_the_way_the_number_does`, a ladder of values in ascending
  order where each rung holds every spelling of one value: within a rung the keys must be
  **identical bytes**, across rungs strictly increasing.
* That ladder earned itself immediately. `Decimal::normalised` stripped trailing zeros only
  `while scale > 0`, which satisfied its own doc comment for `1.0` and broke it for `1230`:
  digits `1230` at scale 0 and digits `123` at scale -1 are one value and got two index keys, so
  a unique index would have admitted both and a lookup by one spelling would have missed a row
  stored under the other. A round-trip property cannot see this; only an order property can.
* 80 statements of PostgreSQL 19 are captured in `tests/corpus/pg19_numeric.txt`. Seven `types`
  divergences and twenty-nine `answers` divergences are declared, and they are arithmetic and
  functions — `numeric` addition, `round`, `trunc`, `sum` — not representation. The type is
  stored, compared, ordered, indexed and printed; it is not yet computed with.
* `sum(bigint)` still does not promote to `numeric`, so DESIGN §16.2's declared divergence
  stands for a different reason than it did: the type now exists, and the aggregate does not use
  it.
