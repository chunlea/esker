# 0030 — the row codec moves down to `esker-keys`

## Context

ADR 0022 makes a columnar replica a Raft **learner** whose apply writes columns instead of rows.
The learner's apply target has to turn a committed row into typed columns, which means decoding a
row value — `esker_sql::row::decode_row`.

It cannot. `esker-sql` depends on `esker-store`, so a store calling into `esker-sql` inverts the
layering. And there is no composition root to inject a decoder from: **no crate in the tree links
`esker-sql`, and it has no binary** — it is a leaf library nothing consumes yet, so "hand the store
an implementation from above" has no above.

The types themselves can travel as data — a schema is a list of column types — but a **codec is
code**. Something below `esker-sql` has to own it.

## Decision

`esker-keys` gains the ADR 0019 row codec and, with it, the bare value vocabulary the codec names.

**What moves:** `encode_row`, `decode_row`, `RowSchema`, the key builders (`row_key`, `index_key`,
`decode_key_columns`, the range helpers), and the enums `Datum` and `ColumnType` with the two
methods that are facts about the data — `column_type` and `fits` — plus `sort_bits_of_f64` and its
inverse.

**What stays in `esker-sql`,** behind two extension traits, `PgType` and `PgDatum`: type OIDs, the
name PostgreSQL uses in messages, `type_len`, the text and binary I/O formats, and `pg_cmp`. These
are contract C3's surface. None of them was written from documentation — each was put to a running
19beta1 and recorded in `tests/corpus/pg19_values.txt` — and a crate that owns byte layout has no
business carrying them.

`esker-sql::row` re-exports the codec, so nothing above changes an import, and
`From<RowError> for SqlError` maps the three failures back to the three conditions a client is told
about.

### The line is drawn between two comparisons that disagree

`Datum`'s `PartialEq` moves down and is **bitwise for doubles**: `-0.0` is not `0.0`, and one `NaN`
payload is not another. `pg_cmp` stays up and says the opposite — `NaN` equals itself and sorts
above `Infinity`, which is what PostgreSQL does and what `WHERE x > 5` has to agree with.

That is not an inconsistency to resolve; it is the seam, and it is why the seam is in the right
place. Equality below answers *did these bytes survive the round trip*, which is a storage question
and must be exact. `pg_cmp` above answers *how does a user's query order these*, which is a SQL
question and must match a real server. One crate holding both is what would let somebody reach for
the wrong one.

### Three errors, not one

`RowError` has `Corrupt`, `Mismatch` and `InvalidUtf8` because they map to three different
conditions above: `DataCorrupted`, `Internal` — only a bug produces a schema mismatch — and
PostgreSQL's own `22021`, which a *user* can cause and a client reads the message of. One variant
would have lost two SQLSTATEs at the boundary. The pg **wording** stays up with `SqlError`; the
codec carries the offending byte, not the sentence.

## Options rejected

**A new crate for the codec.** It would keep `esker-keys` untouched and cost no call-site churn, but
it leaves two byte-meaning crates where the whole point is that one crate owns what bytes mean —
keys and values are the two halves of the same question.

**A codec generic over a `RowValue` trait, so no types move.** Superficially the tidiest: each crate
keeps its own vocabulary and implements the trait. It is the worst of the three, and for the reason
this move exists. A generic codec pushes *format knowledge* into every implementor — how a value is
laid out becomes a thing each crate answers for its own type — so the format would live in as many
places as there are implementors. That is precisely the drift the move is meant to prevent, dressed
as an abstraction.

**Leave it, and have the store keep rows undecoded.** Columnar runs would hold
`(table, pk, commit_ts, value_bytes)` and a fragment could not see typed columns, which defeats the
purpose of a columnar copy.

## Applied a second time: the columnar record

The same argument, the same week, for a record whose *whole design point* is that two layers
share it. `esker-sql` writes a per-table columnar record (ADR 0022 Decision 5); a store holding a
columnar learner reads it to decode rows, and a placement driver reads its replica count. Neither
links `esker-sql`, and the codec was written there — so the layout was shared and the function
that parses it was not.

The columnar record's key builders, its `Published` schema type and its codec now live in
`esker_keys::columnar`, beside the row codec they are built on. `esker-sql` keeps the *writing* of
it, because the `TableDef` it is written from is `esker-sql`'s, and re-exports `Published` rather
than wrapping it so there is one definition of what the bytes mean. The goldens did not move a
byte, which is the test of whether this was a move.

**It is necessary and it is not sufficient for the placement driver, which is worth stating so
nobody reads this as closing that.** `esker-pd` links its own engine, `esker-proto` and
`esker-base`, and every PD method is *inbound* — stores and SQL nodes call PD; PD calls nobody.
So PD can now parse a record it still has no way to obtain. The decoder's home was one problem
and the access path is another, and only the first is closed here.

## Consequences

* `esker-store` can decode a stored row without linking `esker-sql`, which is what unblocks ADR
  0022's apply target.
* **`Datum` and `ColumnType` become the storage layer's shared type vocabulary.**
  `esker-columnar::Value` redefines the same six shapes independently today; it can now converge on
  one definition rather than a copied one. That is a follow-up, not part of this change.
* The goldens do not move: `esker-keys` carries the row format's golden bytes unchanged, and every
  catalog golden in `esker-sql` still passes byte for byte. A move that changed a byte would not be
  a move.
* Fourteen files in `esker-sql` gain a `use crate::value::{PgDatum, PgType}` line. The call sites
  keep their spelling: `datum.to_text()` and `Datum::from_text(..)` still read the same.
* Three property tests move *up* rather than down. `key_order_is_value_order` and its two siblings
  state that memcomparable byte order equals PostgreSQL's value order — they need both halves, and
  only `esker-sql` can see both. `esker-keys` cannot express them and must not grow a `pg_cmp` in
  order to.
