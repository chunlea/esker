# ADR 0115: `regnamespace` is an oid that prints as a schema

**Status:** Accepted, 2026-09-13 — (a) ruled by the coordinator under the standing type-surface directive, reported
to the user and not vetoed; its tags and the format version approved by the coordinator the same day. Proposed the
same day
**Date:** 2026-09-13
**Deciders:** lane s2-plpgsql
**Relates to:** [ADR 0098](0098-regproc-is-an-oid-that-prints-as-a-function.md) (the kind),
[ADR 0113](0113-plpgsql-is-the-subset-the-suite-sends.md) (the statement that needs it),
[ADR 0077](0077-regtype-is-an-oid-that-prints-as-a-name.md) (`regtype`)

## Context

`check_all_foreign_keys_valid!` — the third `DO` body of ADR 0113's census, run twice by `fixtures_test.rb` and once by
`referential_integrity_test.rb` — builds, for every foreign key, the statement

```sql
UPDATE pg_catalog.pg_constraint SET convalidated=false
 WHERE conname = '<name>' AND connamespace::regnamespace = '<schema>'::regnamespace;
ALTER TABLE <schema>.<table> VALIDATE CONSTRAINT <name>;
```

ADR 0113's ruling (b) took the write. The predicate is the half this node cannot read, because it has no
`regnamespace`, so the block still stops on all three calls; `pg_constraint_convalidated.rs` could only exercise it
with the predicate left out.

What PostgreSQL 19beta1 answers (`tests/corpus/pg19_regnamespace.txt`, three sessions, each inside `BEGIN … ROLLBACK`):

* `'public'::regnamespace` prints `public`, and `pg_typeof` says `regnamespace`; `regnamespace[]` prints `{public,s2ns}`.
* Input folds like an identifier — `'PUBLIC'` is `public`, `'"S2 Mixed"'` prints back quoted, a leading space is
  trimmed — and a string that is not one identifier (`'S2 Mixed'`, `'a.b'`, `''`) is `42602 invalid name syntax`, where
  a well-formed name no schema has is `3F000 schema "nosuch" does not exist`. `to_regnamespace` answers `NULL` there.
* Digits are an oid: an oid a schema has prints its name, one no schema has prints its digits, and `0` prints `-`.
* It is `oid` underneath. `pg_type` gives 4089, `typlen` 4, category `N`, and the array 4090; `pg_cast` has
  `oid ↔ regnamespace` implicit in both directions without a function. `min` and `max` answer `oid`.
* **An untyped literal beside a `regnamespace` is read as an `oid`**: `ns = 'public'` is
  `22P02 invalid input syntax for type oid: "public"`. An assignment reads the same literal as a name.
* The census's `UPDATE` selects its constraint by `connamespace::regnamespace = '<schema>'::regnamespace`, and the
  `VALIDATE CONSTRAINT` after it validates what it marked.
* **A stored value is its number.** After `ALTER SCHEMA … RENAME TO`, a `regnamespace` read back from a table prints the
  schema's new name, and once the schema is dropped, its digits.

## Options

1. **A type, following ADR 0098** — `ColumnType::RegNamespace` (4089) and `RegNamespaceArray` (4090). The datum carries
   the oid and the name, because `esker-keys` has no catalog to print one from (invariant 7). Additive tags in both
   vocabularies.
2. **An expression only** — `::regnamespace` folds to the schema's oid and the type reports `oid`. The census predicate
   passes, while `'public'::regnamespace::text` answers a number and `pg_typeof` answers `oid`: two wrong answers
   rather than refusals.
3. **Recognise the census's predicate inside ruling (b)'s narrow write.** Smallest, and exactly the text-shape template
   ADR 0113 refused.

## Decision

Option 1, as ruled.

* **Resolved like `regclass`, not like `regproc`.** A schema is a catalog object: a name is resolved against the
  statement's own view of the catalog, and an oid printed against the executor's schema list — the path `regclass`
  already takes. `regproc` reads a table measured once, which is the right source for built-in functions and the wrong
  one for a tenant's schemas.
* **This node's oids, not PostgreSQL's bootstrap oids.** `pg_namespace.oid`, `pg_class.relnamespace` and
  `pg_constraint.connamespace` all carry this node's numbers — `public` is 11 and every other schema its record id —
  and `::regnamespace` agrees with them, which is what lets `connamespace::regnamespace = 's2ns'::regnamespace` select
  the right rows. PostgreSQL's `public` is 2200 and its `pg_catalog` 11, so the corpus rows that print or read those
  bootstrap numbers are declared divergences; every row that compares or prints a name agrees.
* **ADR 0098's rules, unchanged for the second of its kind:** an oid no schema has prints its digits, and `0` prints
  `-`; `min` and `max` decay to `oid`; a comparison reads a bare literal as an `oid` and an assignment reads it as a
  name; `regnamespace` and `oid` are one representation.
* **A row holds the number, not the name — `regclass`'s rule (`debts-v1.1.md` #35), not `regproc`'s.** Four bytes and
  no name in a row, and the name goes back in where a row is decoded with a catalog to ask (`crate::row::decode_row`,
  through the same namer a `regclass` column's name comes from). `regproc` keeps its name in the row because nothing
  renames a built-in function; a tenant renames and drops its schemas, and the third session measures both.
* **Tags, claimed out loud before they were written** (`esker-coord/s2-claims-format-regnamespace.md`): catalog record
  `TAG_REGNAMESPACE = 110` and `TAG_REGNAMESPACE_ARRAY = 111` in `crates/esker-sql/src/catalog/record.rs`, whose largest
  was 109 (`TAG_OIDVECTOR_ARRAY`); columnar codes 111 and 112 in `crates/esker-keys/src/columnar.rs`, whose largest was
  110 (`OidVectorArray`). That numbering has no named constants — it is the bare numbers of two matches, the forward
  `tag_of` and the reverse `type_of` — and both reverse mappings are written, because the compiler points at neither.
* **`CATALOG_FORMAT_VERSION` stays 38.** ADR 0098 left it where it was for `regproc`, and so did `regclass[]` after it;
  the ruling keeps every existing byte and every golden identical. The trade-off, stated rather than implied: an
  older build that reads a record holding tag 110 answers that the tag is not one of its types, instead of refusing the
  record's version. It refuses either way and never guesses; what the older build loses is the plainer sentence. ADR
  0107's two steps took the other reading and bumped the version — both are defensible, and this one is the ruling.

## Consequences

* `fixtures_test.rb`'s two calls and `referential_integrity_test.rb`'s one reach `VALIDATE CONSTRAINT` in the shape
  Rails sends: the census block runs end to end with its schema predicate in (`tests/regnamespace.rs`).
* **A constraint's name is compared and printed bare on a table in any schema** — found by this ADR's corpus, not
  planned: the census selects a foreign key by `conname` in a schema, and a derived name is stored qualified
  (`plan::make_object_name` re-qualifies), so `pg_constraint` printed `s2ns\0t_pkey`, `VALIDATE CONSTRAINT` could not
  find the key it listed, and the `42704`, `42710`, `23514` and `23503` sentences carried the NUL. Fixed where names are
  shown and matched, not where they are stored; `tests/constraint_name_in_a_schema.rs` holds it to PostgreSQL's answers.
  Its capture also found three older gaps that are not about schemas at all — a table `CHECK`'s derived name, a `CHECK`
  named like a foreign key, `UNIQUE`'s `42P07` — declared there first and closed since as debt #92
  (`tests/constraint_names.rs`).
* **Not an index key — declared.** PostgreSQL 19 builds a primary key and an index over a `regnamespace` column
  (`oid_ops`; `esker-coord/s2-d92c.out`). This node refuses all three by name — `0A000 an index on a column of type
  regnamespace is not supported`, and `a primary key` or `a unique constraint` in the same sentence — because the row
  codec writes no key bytes for the type, as for `regproc` and `regtype`, and a key that writes nothing would give every
  row of the table the same key. The number alone would allow one; nothing the suite sends needs it
  (`tests/regnamespace.rs::a_regnamespace_column_is_not_an_index_key`).
* `ColumnType::ALL` grows from 107 to 109, so every test that loops over it covers the new type without being edited.
* No wire change beyond the type's own oid, no dependency, no format version.

## The rule this is an instance of

ADR 0098: an oid that prints as a name is a type, and the name it prints is the catalog's to give. `regproc`'s catalog
is a table measured once; `regnamespace`'s is the tenant's, so it is read where `regclass`'s is.
