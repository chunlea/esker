# A column of a composite type — the plan

**Status**: sized, not started. Run 97's `a column of the composite type full_address is not
supported` — **4 tests in one file**, `adapters/postgresql/composite_test.rb`, and the largest
actionable row on that board.

## What already works, which is half of it

`CREATE TYPE full_address AS (city VARCHAR(90), street VARCHAR(90))` is **already** parsed, stored
and reported: `TypeKind::Composite` is in the catalog, `pg_type.typtype` is `c`, and `format_type`
answers `full_address`. Probed on this node, so the unit does not start from nothing.

Exactly two things are missing:

```
CREATE TABLE pc (id int8, address full_address)    0A000 a column of the composite type … is not supported
INSERT INTO pc VALUES (1, ROW('Paris','Champs'))   0A000 the function ROW is not supported
```

## The measured spec

Against 19beta1, one rolled-back session. The capture goes in `tests/captures/pg19_composite.txt`
with the unit; these are its rows.

| written | read back |
|---|---|
| `ROW('Paris','Champs-Élysées')` | `(Paris,Champs-Élysées)` |
| the text `'(Paris,Rue Basse)'`, assigned | `(Paris,"Rue Basse")` |
| `ROW('a,b','c"d')` | `("a,b","c""d")` |
| `ROW(NULL,'x')` | `(,x)` |
| `ROW('',' ')` | `(""," ")` |

And the catalog side: `pg_typeof(address)` is `full_address`, `typtype` is `c`, `format_type` on
the column gives `full_address`, and `address = ROW('Paris','Rue Basse')::full_address` is **true**.

### Things reasoning would get wrong

* **NULL renders as nothing and an empty string renders as `""`.** That single pair is what makes
  the text form lossless, and it is what a "join the fields with commas" implementation gets wrong
  in both directions — it would render NULL and `''` identically and could not read either back.
* **A field is quoted when it has to be, not when it looks like it should.** A space, a comma, a
  quote, a paren or a backslash forces quotes; `Champs-Élysées` gets none. A `"` inside is
  **doubled**, not backslash-escaped.
* **The input function is not a store.** Assigning the *text* `'(Paris,Rue Basse)'` and reading it
  back gives `(Paris,"Rue Basse")` — it is parsed and re-rendered canonically, so the round trip is
  through the value and not through the bytes the client sent.
* **The tests need no field access.** `(address).city` works on a real server and is in the
  capture, but `composite_test.rb` splits the string in Ruby. Building it would be scope this unit
  does not need.

## What the four tests actually do

Two classes over one schema. `PostgresqlCompositeTest` asserts `column.type` is nil,
`sql_type` is `full_address`, and that reading back gives the two strings above.
`PostgresqlCompositeWithCustomOIDTest` registers a Ruby type whose `serialize` produces
`"(#{city},#{street})"` — again a text literal into the column — and reads the halves back out.

So the client only ever sends **`ROW(…)`** or **composite text**, and only ever reads **composite
text**. That is the whole surface.

## Design

**The value is canonical text**, the way `jsonb` is. ADR 0042 permits sharing a representation when
the comparison comes with it, and it does here: composite equality is field by field, and two
field-equal composites have the same canonical rendering — which is exactly the argument
`docs/plans/jsonb-representation.md` closed for `jsonb`. So no new `Datum` variant, and the storage
type is `ColumnType::Text` with `ColumnDef::user_type` naming the composite, the arrangement an
enum already uses (`ColumnType::Int2` + `user_type`, ADR 0050).

* `value::composite::render(fields: &[Option<String>]) -> String` — the output function, with the
  quoting rule above.
* `value::composite::parse(text: &str) -> Result<Vec<Option<String>>>` — the input function.
* `value::composite::canonicalise(text) -> Result<String>` — `parse` then `render`, which is what a
  text literal assigned to the column goes through.
* `ROW(a, b, …)` lowers to a call the executor completes, because the **arity check needs the
  catalog** — the same seam `UserRegType` and `UserCast` sit on. `exec::assign` builds the
  canonical text against the column's `TypeKind::Composite` fields, exactly as `into_enum` maps a
  label to its ordinal.

## Test list

* `tests/composite.rs` — the four Rails shapes end to end: declare the type, declare the column,
  `INSERT … ROW(…)`, read back, assign composite text, read back the re-rendered form.
* `tests/corpus/pg19_composite.txt` + its capture — the table above, plus the quoting family
  (comma, quote, paren, backslash, leading and trailing space) and the NULL/empty pair.
* A property test over `render` ∘ `parse`: every field vector round-trips, **NULL and `""`
  included**, which is the pair the format exists to keep apart.
* The non-public-schema case, per the standing rule: a composite type and a column of it inside a
  schema that is not `public`.

## Risks, and what this unit will not do

* **The input function's tolerances are not yet captured.** Whitespace after a comma, a backslash
  escape inside a quoted field, and a trailing `)` inside quotes are all things PostgreSQL has a
  rule for and I have not measured. They go in the capture **before** the parser is written, not
  after — the range-bound unit learned that the hard way (`docs/plans/phase-9-rails.md`'s range
  row: quoted and unquoted runs *concatenate*, which nobody would guess).
* **No field access** (`(x).f`), no `ROW(…)` outside an assignment context, no composite arrays,
  no comparison operators beyond equality. Each is its own row if a test ever asks.
* A composite column is **not** an index key in this unit; `is_index_key` refuses it, and
  `tests/row_order.rs`'s fixture list gets the exclusion with its reason — the same shape
  `regtype` took, and the property that caught a real hole when it did.
