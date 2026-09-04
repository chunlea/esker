# Plan — `tsvector` and `tsquery`

Status: **written before the capture**. `r1` owns the oracle session and
`captures/pg19_tsvector.txt` is queued behind runs 78 and 79; this file is the plan, and every
number in §1 is measured against the node. **Nothing in §3 is settled until the capture lands** —
where this plan and the capture disagree, the capture wins (ADR 0031).

## 1. What the row actually is

`the type TSVECTOR is not supported` is the largest row on the board: **51 tests over two files**.
Measured on `0ded84af`, the two files are not the same size and not the same problem.

| file | tests | what it needs |
|---|---:|---|
| `adapters/postgresql/full_text_test.rb` | **3** | a `tsvector` column that stores and returns text |
| `adapters/postgresql/schema_test.rb` | **48 raises** | one column and one index, both in `setup` |

`full_text_test.rb` is the whole feature's own file and it asks for almost nothing:

```ruby
t.tsvector "text_vector"                      # column.type :tsvector, sql_type "tsvector"
Tsvector.create text_vector: "'text' 'vector'" # stored and read back verbatim
dump_table_schema("tsvectors")                 # t.tsvector "text_vector"
```

**It never calls `to_tsvector`, never uses `@@`, and never concatenates.** It writes a tsvector
*literal* and reads the same characters back.

`schema_test.rb` needs two statements, both in the `setup` every one of its 78 tests runs:

```sql
CREATE TABLE test_schema.things (…, name_vector tsvector, …);
CREATE INDEX c_index_full_text_search ON test_schema.things
  USING gin ((to_tsvector('english', coalesce(things.name, ''))));
```

> **So the 51 is one type and one index expression in one file's setup**, not a full-text search
> engine. Exactly one test in either file reads the index back
> (`index_name_exists?(TABLE_NAME, INDEX_C_NAME)`), and none searches with it.

### Two blockers, not one

Probed on the node, with the suite's own statements:

```text
CREATE TABLE … (name_vector tsvector)        -> 0A000 the type TSVECTOR is not supported
CREATE INDEX … USING gin ((to_tsvector(…)))  -> 0A000 an index USING GIN is not supported
```

`USING gin` is refused independently of the type, so the type alone does not unblock
`schema_test.rb`. Both have to land in the same unit or the file stays where it is.

## 2. The decision that needs an ADR

**A `tsvector` is a stored type, so its representation is an on-disk format decision** — the same
kind ADR 0030 and ADR 0050 record. The ADR to write is *the representation of a tsvector value*,
and the question it has to answer is what a value **is**, not what the functions do:

* a `tsvector` is a sorted, deduplicated set of **lexemes**, each optionally carrying a list of
  **positions**, each position optionally carrying a **weight** `A`–`D`;
* its text form is what `full_text_test.rb` round-trips — `'text' 'vector'`, and with positions
  `'cat':1 'sat':3A`;
* a `tsquery` is a different shape again: lexemes joined by `&`, `|`, `!` and `<->`, with
  parentheses, and it is *not* a set.

The candidate representations, and what each costs:

1. **Text, canonicalised on the way in.** The value is the printed form; every operation parses.
   Cheapest to land, and it makes `@@` a parse-per-row. It also makes equality and ordering the
   text's, which is what a `tsvector` column's index would be over.
2. **A parsed structure with its own codec**, the way a range is (ADR 0063). Correct ordering and
   cheap `@@`, and it is a new on-disk format with a golden test and a record version.
3. **Text in, structure derived per statement.** The worst of both.

The plan proposes **(1) for v1**, and the ADR must say so and say why: the two files store a
literal and read it back, nothing compares two tsvectors, and nothing searches. A format this node
cannot yet exercise is a format it cannot yet get right — and ADR 0042's rule points the same way
(a type may share another's representation only if it shares its comparison; a `tsvector`'s
comparison, when it arrives, is the *canonical text*'s).

**The canonicalisation is the part that is not free**, and it is where the capture is needed: PG
sorts the lexemes, deduplicates them, merges position lists, and prints them in a fixed form. A
node that stored the user's characters unchanged would round-trip `full_text_test.rb` and disagree
with the oracle the moment a value arrives unsorted.

## 3. Scope for v1 — to be confirmed against the capture

**In:**

* `tsvector` and `tsquery` as column types, with `typname`, `format_type`, `information_schema`
  and the schema dumper's `t.tsvector`;
* the text round-trip, canonicalised as the capture shows;
* `to_tsvector([config,] text)` and `to_tsquery`/`plainto_tsquery`, enough for the index expression
  the suite writes — **the `config` argument is accepted and, for v1, only `'english'` and the
  default are honoured**; another name is refused by name rather than silently ignored;
* `@@` between a `tsvector` and a `tsquery`, and `||` concatenating two tsvectors;
* `CREATE INDEX … USING gin (…)`, including over an **expression**.

**Out, and declared:**

* **A GIN index is recorded and not built.** This node has one index shape; `USING gin` will mean
  *an index the catalog reports as GIN* over the same structure, which is what makes
  `index_name_exists?` and the schema dumper right. Nothing in either file searches through it, so
  the difference a client can see is `pg_am`, not an answer. This must be declared in the corpus,
  not left implicit.
* **Stemming.** `to_tsvector('english', 'running')` is `'run':1` on a real server — a full
  Snowball stemmer per language. v1 will not stem, and that is the one place a *wrong answer*
  rather than a refusal is on offer, so the capture decides whether `to_tsvector` ships at all or
  is refused by name until it can stem. **If the capture shows the suite reading a stemmed value
  back, `to_tsvector` is refused by name instead** — the index expression only needs to *parse and
  be stored*, and the suite never reads its result.
* Ranking (`ts_rank`, `ts_headline`), `setweight`, `tsquery` rewriting, `websearch_to_tsquery`,
  and any configuration a `CREATE TEXT SEARCH CONFIGURATION` would make.

## 4. Order of work, once the capture lands

1. the type: `ColumnType::TsVector`/`TsQuery`, the five places ADR 0033's note names, the codec,
   `typname` and `format_type`;
2. the literal round-trip with the canonicalisation the capture shows — this is what
   `full_text_test.rb`'s three tests are;
3. `USING gin`, expression form included — this is what `schema_test.rb`'s setup is;
4. `to_tsvector` / `to_tsquery` / `@@` / `||`, in the capture's order, or the refusals in their
   place per §3.

**Measure after (3).** If `schema_test.rb`'s setup passes, the 48 raises are gone and the file's
remaining failures are the four other shapes already recorded in handover v19 — none above four
tests. The board's largest row is expected to fall to `full_text_test.rb`'s 3 plus whatever the
capture shows the functions owe.
