# 0042 — `json` and `jsonb` are two types, and one of them cannot be a key

## Status

Accepted. Tier 2's first type, and the first **format addition** since ADR 0033 — a new stored
representation rather than a new label on an existing one.

## Context

`r1-harness`'s triage found that **json is the single blocker for all 367 never-loading files** of
`ActiveRecord`'s own suite: the suite's schema has a `json` column, so no other type moves that
number at all. It is therefore the next unit whatever else is outstanding.

ADR 0033's roadmap singled this pair out in advance, for the reason this ADR exists to settle:
"jsonb's stored form, because it normalises key order and whitespace, so a capture decides what is
stored rather than a preference."

The capture is `crates/esker-sql/tests/corpus/pg19_json.txt`, 80 statements, taken by `r1-harness`
and **re-verified here**: replayed twice against 19beta1 for self-idempotence, then diffed against
the committed file byte for byte.

## What the capture decided

Almost every fact about these two types is a *difference* between them, which is why one ADR covers
both.

### `json` is a validated string, and that is the whole of it

The text you sent comes back byte for byte: whitespace, key order and **duplicate keys** all
survive. `'{"a":1,"a":2}'::json` prints `{"a":1,"a":2}`.

It follows — and the capture confirms — that **`json` has no equality operator at all**.
`'{"a":1}'::json = '{"a":1}'::json` is `42883 operator does not exist: json = json`. So a `json`
column cannot be a primary key, cannot be `DISTINCT`ed, and cannot be grouped, on a real server.
That is a property of the type, not a gap in an implementation.

### `jsonb` stores a value and prints a canonical form, and the canonical form is not the input

`'{"b":1, "a":2}'::jsonb` prints `{"a": 2, "b": 1}`: reordered, deduplicated (**last key wins**),
and with **a space after every colon and comma** that was not in the input. An implementation that
stored the text and sorted it on read would still print the wrong separators.

**Key order is by length first, then bytewise.** Measured twice, because one example cannot tell
the two rules apart: `{"bb":1,"a":2,"ccc":3}` is `{"a": 2, "bb": 1, "ccc": 3}` and
`{"ab":1,"ba":2,"aa":3}` is `{"aa": 3, "ab": 1, "ba": 2}`. Plain lexicographic ordering gets the
first one wrong.

### The decision this ADR turns on: printing preserves scale, equality does not

```
'1.0'::jsonb   prints 1.0
'1.00'::jsonb  prints 1.00
'1.0'::jsonb = '1.00'::jsonb            is  t
'{"a":1}'::jsonb = '{"a":1.0}'::jsonb   is  t
```

A jsonb number is a `numeric`, so it carries `numeric`'s scale rule in its *output* and `numeric`'s
equality in its *comparisons*. **Two equal jsonb values can therefore have different canonical
text.**

That is the whole problem. This project's key encoding is byte-ordered, and `esker-keys`'
invariant — proved by its property tests — is that **equal values encode identically**. `char(n)`
met the same wall in tier 1 and had a way through: pad to `n`, and byte comparison *becomes* the
blank-insensitive comparison. There is no equivalent here. Normalising `1.00` to `1` to make bytes
equal would print a value a real server never printed, which is a wrong answer; keeping `1.00` and
comparing bytes would answer `f` where a real server answers `t`, which is also a wrong answer.

## Decision

1. **Two `ColumnType`s**, `Json` (OID 114) and `Jsonb` (3802), both varlena, appended to the tag
   space the way tier 1's types were. Old bytes decode unchanged; a reader that meets the new tags
   answers corruption, which is the direction this format already fails in.

2. **`json` stores the text as given**, validated on the way in and never reformatted. Its
   representation is a `Text`'s, exactly as `varchar` and `bpchar` are, and it is a distinct type
   because its OID, its input validation and its *absent* operators differ.

3. **`jsonb` stores the canonical text** — reordered, deduplicated, respaced, with each number in
   the form `numeric` would print it. Not a parsed binary form: the canonical text is what the
   client is shown, what `->>` returns, and what a byte comparison sorts *within a kind*, so a
   second representation would be a second thing to keep in step for no answer it changes.

4. **A `jsonb` column may not be a primary key or an index column**, and says so by name
   (`0A000`). This is the consequence of the decision above and the one place this ADR knowingly
   answers less than a real server: PostgreSQL has btree operators for `jsonb` and will happily
   index one. Here, an index over a type whose equality is not its byte equality would return rows
   a scan does not, which is the failure mode ADR 0019's pad rule and tier 1's `char(n)` argument
   both exist to prevent. A `json` key is refused too, and there it *matches* a real server.

   The gap closes when `numeric` lands — at which point a jsonb number can be stored in a form
   that is both printable and comparable — and `numeric` is the next unit but one in r1's triage.

5. **Ordering between kinds is PostgreSQL's**, measured rather than assumed:
   `Object > Array > Boolean > Number > String > Null`. `'true' > '1'` is `t` and `'"s"' > '1'` is
   `f`, which is enough to rule out every ordering a reader would guess.

6. **`min`/`max` over `jsonb` do not exist** (`42883`), which is a third totally-ordered type in
   this corpus with no aggregate over it, after `boolean` and `uuid`. Refusing is being right.

## Consequences

* `ColumnType::ALL` goes from twelve to fourteen, and every exhaustive match is a compile error
  until both are handled — which is the point of it.
* **The proto `ValueType` needs neither.** Both travel as `ValueType::Text`, because the fragment
  wire maps by *value* shape and a `json` or `jsonb` value is a `Datum::Text` carrying validated
  or canonical text. This is the same reason `bpchar` cost no wire change and `real` did.
* **The columnar encoding needs neither**: both ride the `Bytes` run that `text`, `varchar` and
  `bpchar` already use, under their own tag bytes.
* `pg_type` grows two rows without being touched, because its rows derive from `ColumnType::ALL`.
* **Neither type takes a typmod**, and the refusal is the *parser's*: `json(10)` is
  `42601 syntax error at or near "("`, like `integer(4)` and unlike `uuid(10)`'s `42601 type
  modifier is not allowed`. Catalog record version 4 is unaffected — these columns carry `-1`.
* **A NUL escape is the one input `jsonb` refuses that `json` takes**: `22P05 unsupported Unicode
  escape sequence`, a SQLSTATE that appears nowhere else in this project. Casting a stored `json`
  containing one to `jsonb` raises the same error later, which is what makes `json`'s
  permissiveness safe rather than a trap.
* Everything past `->`, `->>`, `#>`, `#>>`, `@>`, `<@` and the comparisons is `0A000` naming
  itself — `?`, `||`, `-`, `#-`, `jsonb_build_object`, `json_agg` and the rest. Each is a line in
  the corpus with a declared divergence beside it, so the next unit starts from the measurement.
