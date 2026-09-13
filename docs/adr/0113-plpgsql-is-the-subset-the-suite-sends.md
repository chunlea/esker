# 0113 — PL/pgSQL is the subset the suite sends, run inside the statement

**Status:** Accepted, 2026-09-13 — by the user, with the catalog-write question below ruled
**(b)**. Proposed the same day · Number reserved for lane s2-plpgsql by the coordinator.
Supersedes the decision of [ADR 0058](0058-a-do-block-is-two-templates-not-a-language.md) when
accepted; 0058's measurement stays true. The census and the order of work are
[`docs/plans/plpgsql-subset.md`](../plans/plpgsql-subset.md).

## Context

ADR 0058 made a `DO` block two templates — `create_enum`'s guard and `RAISE NOTICE|WARNING` — and
refused every other body by name. The user's "DO minimal" ruling kept it there, and a second ruling
kept stored triggers unfired. Attempt 6 of run 127 (2026-09-12, 426 of 426 files) left eighteen
failing files, and three of them are those two rulings and nothing else:

* `fixtures_test.rb` (1F 1E) and `referential_integrity_test.rb` (1E): `check_all_foreign_keys_valid!`
  sends a `DO` block that loops over `information_schema.table_constraints` and `EXECUTE`s a
  `format()`-built string — `0A000 DO is not supported`;
* `persistence_test.rb` (1E): `before_insert_trigger` is created by every schema load and never
  fires, so `NOT NULL` refuses a row that really is `(null)`.

On 2026-09-13 the user ruled that the node's remaining capability gaps are to be closed.

The census (plan §2) measured what the suite actually sends: three `DO` shapes, two
`LANGUAGE plpgsql` trigger functions, two `BEFORE INSERT … FOR EACH ROW` triggers. ADR 0058 ended on
the condition for reading this differently — *"if it sends a fourth and a fifth, that is the
evidence the 'language, not a statement' reading needs"* — and the census holds five bodies, three
of which are not templates and share no text with each other.

What the oracle added (plan §3): PostgreSQL's `VALIDATE CONSTRAINT` does nothing to a constraint
already validated, so the suite's `DO` body cannot work without the write to
`pg_catalog.pg_constraint` it contains; `EXECUTE` of a string holding two statements runs both; and
a `BEFORE` trigger that returns `NULL` makes an `INSERT` answer `INSERT 0 0` with no `RETURNING` row.

## Options

1. **A template per body** — ADR 0058 extended with three more recognisers. Rejected. The third body
   is a loop over a query whose body is dynamic SQL; a template for it is an interpreter of one
   program. Four readers of one grammar is the shape that drifts: each learns a different half of
   the lexical rules (0058's own tokenizer knows no comments, no nested dollar quotes, no `E''`).
2. **All of PL/pgSQL.** Rejected. `EXCEPTION WHEN` needs a subtransaction per block, cursors need
   portals inside a statement, `RETURN QUERY` needs set-returning user functions, and none is
   measured; each is a unit larger than this one. A half-built general interpreter is worse than a
   named boundary, because what it silently gets wrong has no list.
3. **One interpreter whose grammar is the census, refusing every other construct by name.** Chosen.

## Decision

1. **The subset is the census** (plan §2.4) plus the forms an existing capture already pins —
   `NULL;`, `RAISE NOTICE` and `RAISE EXCEPTION`, both `LANGUAGE plpgsql` spellings, a named dollar
   tag — and, for triggers, the brief's `BEFORE`/`AFTER` × `INSERT`/`UPDATE`/`DELETE` with `OLD`.
   Every other construct PostgreSQL has is refused by name (plan §11).
2. **A body runs inside the statement that reached it**: the `DO`, or the `INSERT`, `UPDATE` or
   `DELETE` that fired the trigger. Every SQL statement in the body goes through
   `Executor::run_recording` with that statement's transaction. There is no second transaction and
   no autonomous write; an error anywhere is the statement's error and is undone with it.
3. **A variable reaches SQL as a typed bound value**, never printed into the statement text.
4. **A row trigger fires where PostgreSQL fires it**: `BEFORE` after defaults and sequences and
   before generated columns, `NOT NULL` and `CHECK`; `AFTER` once the statement's rows are written,
   in name order. `RETURN NULL` from a `BEFORE` trigger skips the row, its `RETURNING` row and its
   count.
5. **Where half-firing would be silent, the node refuses**: `FOR EACH STATEMENT` (stored today and
   never fired), trigger arguments (dropped today), triggers on partitioned tables and partitions,
   `INSERT … ON CONFLICT` into a table with a row trigger, and a foreign-key cascade reaching one —
   unless the cascade can fire through the same hook, which lane C decides by reading it.
6. **ADR 0058's templates are removed.** `create_enum`'s block and `RAISE WARNING` become ordinary
   bodies of the one interpreter.
7. **Refusal wording comes from the oracle when the oracle has any.** Where PostgreSQL itself
   answers a situation with an error, the node answers with PostgreSQL's code and sentence, taken
   from a capture — `42601 unexpected end of function definition at end of input`,
   `42601 query has no destination for result data`, `0A000 trigger functions can only be called as
   triggers`, `54001 stack depth limit exceeded`. Where PostgreSQL *runs* a construct the subset
   does not, there is no oracle sentence to copy — a server has no refusal for its own grammar — and
   the answer is contract C2's: `0A000`, the construct named, in the form
   `PL/pgSQL <construct> is not supported`. Which of the two cases a form is in is itself decided by
   a capture, never by reasoning about what PostgreSQL probably says.
8. **No format change.** A function's body is stored today (`KIND_FUNCTION`, record version 18),
   and so are a table's triggers (the table record, version 18) and `triggers_disabled`
   (version 12); the interpreter reads what is there, and a parsed body is cached in memory only.
   Anything that would need a new record kind or a new field — a function's return type, for
   PostgreSQL's refusal of a non-trigger function named by `CREATE TRIGGER`, is the one candidate —
   is a format change, stops for the user, and is not built: the census does not need it.

## Consequences

* The three rows stop failing on `DO` and on an unfired trigger. **How far the `DO` row moves is not
  this ADR's to decide**: its body writes `pg_catalog.pg_constraint`, which a separate ruling
  refuses with `42501` (`tests/user_decided_divergences.rs`). With that ruling standing, the
  interpreter moves `fixtures_test.rb` and `referential_integrity_test.rb` from `0A000` to `42501`
  and no further; the narrow write that would finish them is put to the user as a question
  (plan §6), with no format change either way. **Ruled (b), 2026-09-13**: `UPDATE
  pg_catalog.pg_constraint SET convalidated = …` writes the `validated` flag a foreign key or a
  `CHECK` already stores, and every other write to a system catalog stays `42501`.
* `insert_partitioning_trigger` fires too. Four `postgresql_adapter_test.rb` tests that pass today,
  because the row lands in the parent, must pass with the row in the inheritance child — `NEW.*`, an
  insert from inside a trigger, `INSERT 0 0`, and `max(id)` through inheritance. That sequence is the
  trigger unit's acceptance.
* Tests that pinned the old rulings change their assertions to the oracle's answer (plan §9). None
  is deleted or skipped.
* Every `INSERT`, `UPDATE` and `DELETE` asks its cached `TableDef` whether it has an enabled row
  trigger; a table with none pays an `is_empty`.
* A statement restart (ADR 0057) re-runs a body, so a notice raised before a lock wait can be sent
  twice. An error raised inside a body carries no `CONTEXT` line.
* ADR 0058 becomes *Superseded by 0113* on acceptance; `docs/plans/do-blocks.md` §2's closed
  question is reopened by the 2026-09-13 ruling and points here.

## The rule this is an instance of

ADR 0031: implement what the capture shows. ADR 0058 measured one template where a language was
predicted, and was right to build a template. This measures five bodies and a loop over dynamic SQL,
and the same rule makes it a grammar — sized by the census rather than by the language's name, with
the rest refused where a reader can see the list.
