# 0060 — A `USING` clause is a licence, not an expression

**Status:** accepted · **Date:** 2026-09-04 · ADR number claimed at `bc7398e8` (0059 was the last).

## Context

Run 59 ranked `ALTER TABLE … ALTER COLUMN … TYPE TIMESTAMP is not supported` at 13 tests over 2
files. Twelve distinct `ALTER COLUMN … TYPE` statements were logged running those files against
PostgreSQL 19, and the five timestamp ones all carry an explicit `USING CAST("somedate" AS …)`.

Two facts shaped the design, and only one of them was visible from the ranking.

**A column's type is not metadata here.** A row is stored positionally and decoded against the
table's *current* schema (ADR 0030). A column that changes type and leaves its rows alone makes
every row already written decode as the wrong value — so this statement must rewrite the table.

**There is no per-row expression evaluator.** `crate::parse::lower_cast` says so in as many words:
*"a cast of a column has to happen per row and this node has no expression-level cast to do it
with."* Casts are folded over literals at plan time. So `USING <arbitrary expression>` cannot be
run, and the question is what to do about that.

## Decision

**Read `USING` far enough to know whether it licenses the conversion the statement already names,
and refuse it otherwise.**

`USING CAST(c AS t)` and `USING c::t` name the same column and a type; they ask for exactly the
conversion `TYPE t` already describes, so they are recorded as *"the caller licensed a conversion
PostgreSQL would not do implicitly"* and nothing is evaluated. The clause's own type is carried,
because it need not equal the target: `TYPE character varying USING s::text` casts to `text` and
lands in `varchar`, and PostgreSQL takes it because the second hop is an assignment cast. Both hops
are checked.

Any other `USING` is `0A000` naming the expression.

Conversion of a value goes **through the type's own text representation** — the same
`to_text`/`from_text` pair the wire protocol uses — except where two `ColumnType`s share bytes and
differ only in tag (`text`/`varchar`/`bpchar`, `citext`, and the two timestamps), which are
re-labelled. That is not an expression evaluator and is not a step toward one; it is the statement
"this value, read as that type".

### The conversion table is measured, not derived

Four rules in the capture that reasoning got wrong on the first attempt, each fixed by the corpus:

* **"Needs `USING`" is a property of the pair, not the target.** `timestamp → timestamptz` needs
  none and `varchar → timestamp` does, though both casts exist.
* **Integer conversions are symmetric.** `bigint → integer` is implicit; a value that no longer
  fits is that row's error. The first table allowed widening only and refused a statement
  PostgreSQL takes.
* **The default is held to the assignment cast, not to its value.** A `varchar` column defaulted to
  `'0'` going to `integer` is `42804` even though `'0'` is a fine integer — `USING` governs the rows
  and says nothing about the default, and a `SET DEFAULT` later in the same statement does not
  rescue it. Measured on two independent pairs.
* **A narrowing is checked per row, not compared once.** `varchar(5)` over a nineteen-character
  value is `22001`, and which row raises it depends on the data.

## Consequences

* The row goes from **13 tests over 2 files to 1 test over 1 file**.
  `adapters/postgresql/change_schema_test.rb` is 8 runs / 10 assertions / 0 errors — identical to
  PostgreSQL 19 on the same file.
* **The one remaining test is the boundary, on purpose.** `array_test.rb` sends
  `USING string_to_array("snippets", ',')`, which asks for a computation. Running the type change
  and ignoring the expression would leave the column `text[]` with every row holding a one-element
  array of the whole string — a wrong answer reached through a statement that succeeded. It is
  refused instead.
* **The whole table is read and written in the statement's transaction**, the same trade `backfill`
  makes. A large table costs a large transaction; `TODO(post-v1)` is the staged rewrite ADR 0020
  describes for indexes.
* `timestamptz(p)` gained a typmod arm it was missing — the spelling `change_column` sends. That
  was a hole in the type surface rather than in this statement, found by the capture.
* If a later Rails version sends a computing `USING`, this is the ADR to revisit — and the answer
  then is a per-row evaluator, which is a much larger decision than this one.
