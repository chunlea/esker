# 0058 — A `DO` block is two templates, not a language

**Status:** accepted · **Date:** 2026-09-04 · Supersedes nothing.
ADR number claimed at `797dc331`: 0055 (tls), 0056 (c6), 0057 (read-committed) were taken, so this
is 0058.

## Context

`DO $$ … $$` runs an anonymous PL/pgSQL block. Run 57 ranked `DO is not supported` at **34 tests
over 5 files**, and the standing assumption — written into `enum_value.rs`'s divergence list — was
that closing it meant a PL/pgSQL interpreter: *"its own unit, and a large one — it is a language,
not a statement."*

That assumption was never measured. This unit measured it.

The five files were run against PostgreSQL 19 with `log_statement = all`, and every `DO` statement
they send was extracted from the log:

| shape | count | where from |
|---|---:|---|
| `create_enum`'s idempotent `CREATE TYPE … AS ENUM` guard | 36 | `postgresql_adapter.rb:556` |
| `do $$ BEGIN RAISE WARNING '<text>'; END; $$` | 7 tests | `postgresql_adapter_test.rb:697+` |
| `check_all_foreign_keys_valid!` — a real program | 2 tests | `referential_integrity.rb:41` |

The first is one template with eleven textual variants that differ only in the type name, the label
list, whether the created name is schema-qualified, and which of two schema predicates the guard
uses. **None of the five files writes `DO` itself**; all of it comes from the adapter.

The third one is a different kind of thing entirely:

```sql
do $$ declare r record;
BEGIN FOR r IN (SELECT FORMAT('UPDATE pg_catalog.pg_constraint SET convalidated=false …', …)
                FROM information_schema.table_constraints WHERE constraint_type = 'FOREIGN KEY')
  LOOP EXECUTE (r.constraint_check); END LOOP; END; $$;
```

A declared record variable, a cursor loop over a query, `FORMAT` building SQL as text, `EXECUTE` of
that text, and a direct `UPDATE` of `pg_catalog.pg_constraint`.

## Decision

**Recognise two templates. Refuse every other body by name.**

1. `BEGIN IF NOT EXISTS (SELECT 1 FROM pg_type t JOIN pg_namespace n ON t.typnamespace = n.oid
   WHERE t.typname = '<name>' AND n.nspname = <ANY (current_schemas(false)) | '<schema>'>)
   THEN CREATE TYPE <name> AS ENUM (<labels>); END IF; END`
   → the `CREATE TYPE`, made a no-op when the type is already there.
2. `BEGIN RAISE <NOTICE | WARNING> '<text>'; END`
   → a notice at that severity, and the tag `DO`.

Anything else is `0A000` naming `DO`, which is contract C2's rule.

The recognisers **tokenise** the body rather than matching its text, so the same template survives
`EXISTS (SELECT` and `EXISTS ( SELECT`, one line and thirteen, and any indentation — all of which
the suite actually sends.

### What is deliberately not implemented, and why each

* **`RAISE EXCEPTION`** — an error (`P0001` with the raised text as the whole message), not a
  notice. Routing it through the notice path would turn a failed statement into a successful one.
* **`RAISE INFO` / `LOG` / `DEBUG`** — there is no severity token for them on this wire
  (`error::Severity` has four). Downgrading one to `NOTICE` would print the wrong word to a client
  that is reading exactly that word.
* **`DO LANGUAGE plpgsql $$ … $$`** and a trailing `LANGUAGE` — a body that says which language it
  is in is not one to guess at.
* **`check_all_foreign_keys_valid!`** — variables, loops, dynamic SQL, and a write straight into
  `pg_catalog`. This is the case the "it is a language" reading was right about, and it is 2 tests.

## Consequences

* The row goes from **34 tests over 5 files to 2 tests over 1 file**, and
  `invertible_migration_test.rb` goes to 28 runs / 0 errors.
* **A refusal is the load-bearing half.** A node that ran the templates and silently ignored other
  bodies would turn a missing feature into a wrong answer: `DO $$ BEGIN CREATE TABLE t (a int);
  END $$` really creates a table, and a quiet no-op leaves the *next* statement to fail on the
  absence. That case is a test.
* **`CREATE TYPE` grows an `if_not_exists` that no user can write.** PostgreSQL has no
  `CREATE TYPE IF NOT EXISTS` — the whole reason the adapter writes a block — so the flag is set
  only by the recogniser and is not reachable from SQL.
* One statement type, `plan::Statement::Raise`, is a plan node whose parse tree is a **placeholder**
  that the lowering discards. Every other rewrite in `crate::parse` keeps the tree and adds to it;
  this is the one that replaces it, because a `RAISE` is not any other statement in disguise.
* If a later Rails version sends a third template, this is where it goes — and if it sends a fourth
  and a fifth, that is the evidence the "language, not a statement" reading needs. It did not have
  that evidence when it was written, and neither would a reader who reversed this without
  re-running the measurement.

## The rule this is an instance of

ADR 0031: implement what the capture shows. The prediction here was off by a factor of eighteen —
one template, not a language — and the only thing that separated them was reading the oracle's
statement log instead of the feature's name.
