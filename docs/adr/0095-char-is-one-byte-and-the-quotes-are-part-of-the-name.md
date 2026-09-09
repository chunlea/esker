# 0095 — `"char"` is one byte, and the quotes are part of the name

Status: accepted · 2026-09-09

## Context

r1's wire sweep filed six rows: `pg_class.relkind`, `pg_constraint.contype`, `pg_type.typcategory`
and `pg_type.typdelim` are `"char"` (oid 18) on a real server and were `text` (25) here. It is the
catalog's own one-character type, and it is **not** `character(1)` — that is `bpchar`.

## Decision

`ColumnType::Char` (oid 18, `typname` `char`, `typlen` 1, `typinput` `charin`, `typarray` 1002) and
`ColumnType::CharArray` (1002, `_char`). Additive on both tag spaces: catalog record 102 and 103,
columnar 103 and 104.

**Eighteen catalog columns, not the four r1 filed.** A wire sweep looks at what a suite happens to
select, and the four it named would have left fourteen columns of the same type answering `text`
for no reason but that nobody had asked. The census is a query rather than a list to remember:

```sql
SELECT c.relname, a.attname FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid
 WHERE a.atttypid = 18 AND c.relnamespace = 'pg_catalog'::regnamespace
   AND c.relkind IN ('r','v') AND a.attnum > 0 AND NOT a.attisdropped;
```

46 columns on PG 19beta1, of which this node declares eighteen — `amtype`, `attidentity`,
`attgenerated`, `castcontext`, `castmethod`, `relpersistence`, `relkind`, `contype`, `confupdtype`,
`confdeltype`, `deptype`, `partstrat`, `prokind`, `provolatile`, `tgenabled`, `typtype`,
`typcategory`, `typdelim`. Every one of the fourteen that was `text` already had a `\gdesc` line in
this tree recording the oracle calling it `"char"`, taken when some other unit captured its corpus;
`tests/char_type.rs::every_catalog_column_a_real_server_calls_char_is_one_here` walks the eighteen
through `describe` and carries each citation. The values did not change — each is a
`Datum::Text` of one character and was before.

Four facts carry it, and each was measured rather than reasoned:

**1. `typcategory` is `Z`**, its own group rather than `S` with the strings. That one letter is what
makes `CASE WHEN true THEN 'r'::"char" ELSE 'x'::text END` a
`42804 CASE types text and "char" cannot be matched` **while `'r'::text = 'r'::"char"` is `t`** —
the two compare and have no common type, which is a pair reasoning gets backwards. It also forced a
correction one level up: the `CASE` arm asked `same_family`, and the question it wants is
`unify` — "is there an operator" and "is there a common type" are different, and `"char"` is the
type that separates them.

**2. The byte is what is kept, not the character.** `'abc'::"char"` is `a`; `'é'::"char"` is the
first *byte* of a two-byte character, which is not valid UTF-8 alone, so `charout` writes it as the
octal escape `\303` and `octet_length` then counts that escape's four characters. `charin` reads the
escape form back — `'\303'::"char"` is `\303` and not `\` — so the conversion is **idempotent**, and
that is PostgreSQL's own behaviour rather than a convenience. It has to be, because a folded cast
keeps its `Cast` node ([ADR 0086](0086-a-folded-cast-keeps-the-type-it-named.md)) and the evaluator
therefore reads the rendered value a second time.

**3. `min`/`max` decay to `text`**, which makes this the fourth member of that arm after `varchar`,
`name` and `cidr`. `array_agg` keeps `"char"[]`. And `||` over one is **`42725 operator is not
unique`** — ambiguous rather than missing, because a real server has a candidate at every string
width and category `Z` picks none of them. That is decided at plan time, because the evaluator sees
a `Datum::Text` for a `"char"` and cannot tell.

**4. The quotes are part of the name, and this is the only type where they change the answer.**
Bare `char` is `bpchar`; `"char"` is oid 18. `lower_type` had a pre-pass reading "a quoted type name
is a type name" that stripped the quotes and re-read — right for `"bit"`, `"varchar"` and `"int4"`,
where both spellings are the same type, and wrong here. It now asks `pg_type` first and falls
through to the strip only for a name the catalog does not hold, so a quoted domain still resolves.
`value::type_by_name` forced the SQL grammar on where `named_type` decided it from the quoting; the
two entry points disagreeing was invisible until a name existed where it mattered.

**And `name.typelem` is 18.** A `name` is 64 `"char"`s on a real server and its `typelem` says so.
Here it was a zero, and the zero was honest for exactly as long as the type was missing: a
`typelem` naming a `pg_type` row that is not there is what `array_delimiter.rs::no_typarray_dangles`
forbids one column over. It stopped being honest the day the row existed. Same shape as `_name`
itself, which was a named gap until [ADR 0086](0086-a-folded-cast-keeps-the-type-it-named.md).

## Consequences

* **Declared divergences deleted across the corpora** — every corpus that read one of the eighteen
  columns. All named by the ratchet: a listed divergence that starts agreeing fails until it is
  removed, which is how the fourteen extra columns paid for themselves rather than costing a
  separate sweep.
* `pg_type` gained two rows, so `ActiveRecord`'s type query answers one more and its array query
  forty-two rather than forty-one; `_char`'s element was already in the adapter's fixed list.
* `"char"` sorts by byte and is an index key; its fixture leads with the empty string, which is a
  legal `"char"` of zero characters and is not NULL.
* **`pg_cast`'s eight `"char"` rows were already there**, taken when the oids were measured. Adding
  them again duplicated every one and the corpus said so at once — the same "verify the symptom,
  not the site" this queue keeps re-learning, and cheap to catch only because the row was captured.
* What is left of the catalog's own types is `oid` answered as a `bigint` and `regproc` as `text`,
  each its own unit on the type-surface queue and each with the same census available to it.
