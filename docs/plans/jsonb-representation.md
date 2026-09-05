# `jsonb` gets a representation of its own

**Status**: queued, after `serial_test.rb`'s remaining two. Not before `pg_trgm`/GIN — 48 tests sit
behind that and none behind this.

## Why

`jsonb` has **no `Datum` variant**. It is a `Datum::Text`, where `hstore`, `ltree`, `citext`,
`tsvector` and `tsquery` each have one. So at the value layer nothing can tell a `jsonb` from a
string, and every operator the two share has to guess.

[ADR 0042](../adr/0042-a-type-shares-a-representation-only-if-it-shares-a-comparison.md) is the
rule this breaks: a type may share another's representation **only if it shares its comparison**.
`jsonb` does not share `text`'s. Two documents that differ only in key order or in whitespace are
equal as `jsonb` and unequal as `text`; `{"a":1}` and `{"a": 1}` are the same document and two
different strings. The node already normalises on input — `'{"a":1}'::jsonb` prints `{"a": 1}` —
which is what has kept the equality *mostly* right, and normalisation is not the same as sharing a
comparison.

## What it costs today

`||`. The operator means five things and is told apart by its operands
([ADR 0070](../adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md) is a
different symbol with the same shape of problem): hstore merge, ltree concatenation, tsvector
concatenation-with-renumbering, array append, and string concatenation. `jsonb || jsonb` is a
sixth — **document merge** — and it cannot be reached, because by the time the evaluator sees the
operands they are two `Datum::Text`s.

Guarded rather than answered, in two places, so the wrong answer is not shipped:

* the **lowerer** refuses when either operand is syntactically a cast or typed string to
  `json`/`jsonb`, which is where `'{"a":1}'::jsonb` is still visible — a literal cast is folded
  away before the executor sees it;
* the **evaluator** refuses when either operand is an `Expr::Ordinal` whose `ty` is `Json` or
  `Jsonb`, which is how a jsonb *column* reaches it.

Both give the `0A000` the operator gave before `||` over text existed. A value that arrives as a
bound parameter is the residue and is not guarded — nothing in the suite sends one.

## What the unit is

1. `Datum::Jsonb(String)` beside `Datum::Text`, carrying the normalised document.
2. Its comparison: key order and whitespace insignificant, so `=` and `ORDER BY` mean what
   PostgreSQL means. This is the ADR 0042 obligation and the reason the variant has to exist.
3. `||` as document merge, right operand winning on a duplicate key — captured, not reasoned.
4. Delete both guards above and the corpus's declared refusal rows; the ratchet will insist.
5. The same question for `json`, which is **not** `jsonb`: it preserves key order, whitespace and
   duplicate keys, so it may genuinely be a `text` — and if so that should be written down rather
   than left as an accident.

## What to capture first

`||`, `=`, `ORDER BY`, `->`/`->>`, `@>` and `jsonb_build_object` over documents that differ only in
key order, in whitespace, and in duplicate keys — the three axes on which `jsonb` and `text`
disagree. Then `json` beside each, which should differ from `jsonb` on all three.
