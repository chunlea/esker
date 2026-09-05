# 0066 — A tsvector is its canonical text

- Status: **proposed** — the decision below is what `docs/plans/tsvector.md` will implement, and
  the capture (`captures/pg19_tsvector.txt`, owned by the harness lane) settles the two questions
  §"Open until the capture lands" names. This ADR is written first because a stored type is an
  on-disk format and the format is what has to be decided before code.
- Date: 2026-09-04
- Follows [ADR 0030](0030-the-row-codec-moves-down.md) on stored representations,
  [ADR 0042](0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md) on when two
  types may share one, and [ADR 0031](0031-rails-compatibility-is-measured.md)'s rule that the
  capture decides.

## Context

`the type TSVECTOR is not supported` is the largest row on the compatibility board: 51 tests over
two files. Measured, the two files want very different things.

`full_text_test.rb` — the feature's own file, and **three** tests — writes a tsvector *literal* and
reads the same characters back:

```ruby
Tsvector.create text_vector: "'text' 'vector'"
assert_equal "'text' 'vector'", Tsvector.first.text_vector
```

It never calls `to_tsvector`, never uses `@@`, and never concatenates.

`schema_test.rb` — **48 of its raises** — needs one column and one index, both in the `setup` that
all 78 of its tests run:

```sql
CREATE TABLE test_schema.things (…, name_vector tsvector, …);
CREATE INDEX … USING gin ((to_tsvector('english', coalesce(things.name, ''))));
```

Exactly one test in either file reads that index back, by name. **Neither file searches.**

## Decision

**A `tsvector` value is its canonical text, and a `tsquery` value is its canonical text.**

The stored bytes are the printed form — the same string `pg_catalog`'s output function would
produce — canonicalised on the way in: lexemes sorted, deduplicated, position lists merged and
printed in PostgreSQL's order. A column of either type is a string column with a different
`typname`, and nothing below `esker-sql` can tell it from one.

Three things follow, and they are the reason:

1. **The equality is the text's** — two values that print the same are the same value — which is
   what [ADR 0042](0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md) requires of
   two types sharing a representation.

   > **Corrected on measurement.** This point first said *the comparison* is the text's, and that
   > a tsvector's ordering on a real server is over its canonical form. **The ordering is not.**
   > Measured on 19beta1 over ten values, ascending:
   >
   > ```text
   > (empty)  'A'  'a'  'b'  'ab'  'a b'  'a':1A  'a':1  'a' 'b'  'a':1,2
   > ```
   >
   > `'b'` sorts **before** `'ab'` and `'a':1A` **before** `'a':1`, where plain bytes give the
   > reverse of both, and a two-lexeme vector lands between two one-lexeme ones. PostgreSQL orders
   > by lexeme **length**, then bytes, then positions — not by the printed form.
   >
   > So a `tsvector` column is stored, compared, grouped and round-tripped by its canonical text,
   > and it is **not an index key here**: `esker_keys::row` refuses one beside `hstore`, and
   > `tests/row_order.rs` records why it has no ordering fixture. This is the inverse of `jsonb`,
   > which cannot be a key because values that are *equal* differ in bytes; a tsvector cannot
   > because values that are *ordered* differ in order. `tsvector_ops` being a real btree operator
   > class on a real server is what makes this a declared divergence rather than a gap — the
   > server can index one and this node cannot.
   >
   > The guard that caught it is
   > `tests/row_order.rs::encoded_keys_sort_the_way_postgresql_sorts_the_values`, whose message is
   > the whole argument: *a type whose key order is unchecked is a type whose range scans are
   > unchecked*. It was written for exactly this and it fired on the first type that needed it.

   > **Amended 2026-09-05: under `gin` the ordering is unused, not absent, and a `tsvector` column
   > is a `gin` index key.** The correction above is right about the order and drew one conclusion
   > too many from it. What it establishes is that this node's byte order for a `tsvector` is not
   > `tsvector_ops`' order — which matters wherever the order of the key *is* the index, and
   > nowhere else. A `gin` index is not read in key order: nothing range-scans it, no `ORDER BY`
   > is answered from it, and [ADR 0070](0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md)
   > already says nothing claims it accelerates a search. So the wrong order is never consulted.
   >
   > **The code was already inconsistent with itself, which is what forced this.** An *expression*
   > index over `to_tsvector('english', …)` — a value of exactly this type — was accepted, because
   > `crate::exec::ddl::index_expression` has no `is_index_key` gate, and it writes rows correctly;
   > a *column* of the same type was refused. One of the two had to move, and admitting the column
   > is the direction the capture points: `pg_opclass` gives `tsvector_ops` as the **default**
   > class for `btree`, `gin` *and* `gist`, so a real server accepts all three
   > (`tests/corpus/pg19_gin_tsvector.txt`, taken by the harness lane).
   >
   > **`btree` and `gist` stay refused and stay a declared divergence.** Under `btree` the order is
   > the index, and this node's is not the server's — the paragraph above is why. `gist` is the
   > same case as `gin` one word away and is left refused until something needs it, rather than
   > widened on an argument nothing exercises. The refusal is this node's own `0A000` naming the
   > construct, **not** `42704 has no default operator class`: that is an error a real server does
   > not give for a `tsvector`, and the brief for this change assumed it did. Where PostgreSQL does
   > give it is `hash` and `brin`, which have no default class for the type — also in the capture.
   >
   > What this buys: `corpus/pg19_tsvector.txt` replays **whole**, part 3 included, which is
   > `schema_test.rb`'s `setup` — the 48 raises this ADR's Context opens with.
2. **The round-trip is the feature** for the file that owns it. A parsed structure would have to
   reproduce the printed form exactly anyway, so the printed form is the shorter path to the same
   answer — and the only one whose correctness the suite can currently check.
3. **A format this node cannot exercise is a format it cannot get right.** Nothing in either file
   compares two tsvectors, indexes one for search, or reads a position list back. A structural
   codec would be a new on-disk format, a record version and a golden test, decided against
   requirements nobody has measured.

**The canonicalisation is not free and is the whole of the risk.** Storing the user's characters
unchanged would round-trip `full_text_test.rb` and disagree with the oracle the first time a value
arrives unsorted or repeated. So the input function is where the work is, and it is what the
capture pins.

## Consequences

- No record-format change and no new codec: a `tsvector` column stores a string, and
  `ColumnType::TsVector` exists to give it its own `typname`, `format_type` and dumper spelling.
- `@@` and `||` parse and evaluate over the canonical text. `||` is a merge of two sorted lexeme
  sets, which is the same operation the input function already performs.
- **A GIN index is recorded and not built.** This node has one index shape; `USING gin` means an
  index the catalog *reports* as GIN over that shape. `index_name_exists?` and the schema dumper
  are right, and the difference a client can see is `pg_am` rather than an answer. Declared in the
  corpus, never implicit.
- Ranking, `ts_headline`, `setweight`, `websearch_to_tsquery` and text-search configurations are
  out of v1 and refused by name.

## Open until the capture lands

Both are wrong-answer risks rather than gaps, which is why neither is decided here:

1. **Stemming.** `to_tsvector('english', 'running')` is `'run':1` on a real server — a Snowball
   stemmer per language. v1 will not stem. If the capture shows the suite reading a stemmed value
   back, **`to_tsvector` is refused by name** instead of answering an unstemmed vector: the index
   expression only needs to parse and be stored, and neither file reads that function's result.
   A refusal is available, so a wrong answer is not.
2. **The `config` argument.** `'english'` and the default are what the suite writes. Whether
   another configuration name is refused or accepted-and-ignored is the capture's to say; the
   default position is refused by name, because a configuration silently ignored changes which
   lexemes come out.
