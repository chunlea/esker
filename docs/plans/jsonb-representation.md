# `jsonb`'s representation — asked, and answered by measurement

**Status**: closed. The unit this file was written to plan turned out not to exist; what it was
really about — `||` as document merge — is done, and the premise behind the rest was wrong.

## What this file used to say

That `jsonb` has no `Datum` of its own — it is a `Datum::Text`, where `hstore`, `ltree`, `citext`
and `tsvector` each have a variant — and that this breaks
[ADR 0042](../adr/0042-a-type-shares-a-representation-only-if-it-shares-a-comparison.md): a type
may share another's representation **only if it shares its comparison**, and two documents that
differ in key order or whitespace are equal as `jsonb` and unequal as `text`.

## Why that was wrong

The text `jsonb` shares is **canonical**. `value::json::canonicalise` runs on every value on the
way in (`value::mod`'s `ColumnType::Jsonb` arm) and does all three normalisations at once:

* keys reordered **by length, then bytes** — PostgreSQL's own `jsonb` key order, not lexicographic,
  so `{"z":1,"aa":2}` stores as `{"z": 1, "aa": 2}`;
* duplicate keys dropped with the **last** winning;
* every separator normalised to exactly one space.

Measured against 19beta1 on each axis. So two documents equal as `jsonb` are already the same
string, and text's comparison **is** jsonb's. That is precisely the condition ADR 0042 permits
sharing under — the rule is not "never share", it is "share only when the comparison comes too".

It is the same argument the six geometric shapes settled: the canonicalisation is the subject and
the storage is not.

## What was actually missing, and is now done

`||`. The operator means six things and is told apart by its operands — hstore merge, ltree
concatenation, tsvector concatenation-with-renumbering, array append, string concatenation, and
**jsonb document merge**. The last had no implementation, so it was refused.

`value::json::concat` implements it, from a capture of every combination on 19beta1:

| left | right | answer |
|---|---|---|
| `{"a":1,"b":2}` | `{"b":3,"c":4}` | `{"a": 1, "b": 3, "c": 4}` — the **right** wins a shared key |
| `{"a":{"x":1}}` | `{"a":{"y":2}}` | `{"a": {"y": 2}}` — **not** a deep merge |
| `[1,2]` | `[3]` | `[1, 2, 3]` |
| `[1,2]` | `3` | `[1, 2, 3]` |
| `{"a":1}` | `[1]` | `[{"a": 1}, 1]` |
| `"x"` | `"y"` | `["x", "y"]` |
| `null` | `null` | `[null, null]` |

One special case and one rule: both objects merge; otherwise each side reads as an array and they
concatenate.

Two things measurement settled that reasoning would have got wrong:

* **`jsonb || text` is not an operator.** A jsonb column beside a text column falls back to
  `text || text` and answers `{"a": 1}x`. A merge that fired on *one* jsonb operand would corrupt
  ordinary concatenation, so both sides must be jsonb — with a bare literal counting, because
  `'{"a":1}'::jsonb || 'tail'` coerces the literal and fails `22P02`.
* **`json` has no `||` at all** — `42883 operator does not exist: json || json`. The corpus briefly
  claimed PostgreSQL concatenated them as text; nobody had measured it.

## What is genuinely left, and it is small

**Ordering.** `ORDER BY` on a `jsonb` column sorts by text here and by type rank there:
`null < string < number < boolean < array < object`, measured with `row_number()`. Canonical text
gives the right *equality* and not the right *order*. Nothing in the suite has asked for it; when
something does, it is a comparison function over the parsed value, not a new representation.

`json`'s own status is unchanged and correct: it keeps key order, whitespace and duplicates, has no
equality operator on a real server (`42883 operator does not exist: json = json`), and really is a
validated string.
