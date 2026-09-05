# Plan — `DO` blocks, the non-template forms

Status: **census first, and the census changes the unit.** The brief was "DO non-template forms,
34 tests on the v1 deferral sheet". Measured, the row is **2 tests**, and it has been 2 since run
70. What is actually worth doing here is a different and smaller thing, described in §3.

Nothing in this file is implemented yet. [ADR 0058](../adr/0058-a-do-block-is-two-templates-not-a-language.md)
is the standing decision and this plan does not reopen it.

## 1. The row is 2 tests, not 34

Counted from each run's own `ranking.txt` in the Rails harness, summing the `tests` column of every
`DO is not supported` shape:

| run | DO tests |
|---:|---:|
| 57 | 36 |
| 60 | 37 |
| 70 | **2** |
| 78 | 3 |
| 80 | 2 |
| 82 | 2 |
| 84 | **2** |

**ADR 0058's two templates closed it between runs 60 and 70**, fourteen runs ago. The 34 on the
deferral sheet is the pre-0058 figure and has simply never been re-taken. Run 84's board head is
`an index USING GIN is not supported` at 50 tests; `DO` does not appear in the top six shapes at
all, and its two rows sit at position 94 of 125.

## 2. Both surviving tests are the case ADR 0058 declined, and the ruling agrees with it

Run 84's two rows are the same test object:

```text
1  1  PG::FeatureNotSupported: ERROR: DO is not supported\n"X"fk_pointing_to_non_existent_objects".
1  1  PG::FeatureNotSupported: ERROR: DO is not supported
```

`fk_pointing_to_non_existent_objects` is `check_all_foreign_keys_valid!`
(`referential_integrity.rb:41`), which sends:

```sql
do $$ declare r record;
BEGIN FOR r IN (SELECT FORMAT('UPDATE pg_catalog.pg_constraint SET convalidated=false …', …)
                FROM information_schema.table_constraints WHERE constraint_type = 'FOREIGN KEY')
  LOOP EXECUTE (r.constraint_check); END LOOP; END; $$;
```

A declared record variable, a cursor loop over a query, `FORMAT` building SQL as text, `EXECUTE` of
that text, and a direct `UPDATE` of `pg_catalog.pg_constraint`. ADR 0058 named this exactly: *"this
is the case the 'it is a language' reading was right about, and it is 2 tests."*

> **The standing ruling is "DO minimal — only what the suite needs, no general PL/pgSQL."** These
> two tests need variables, a cursor loop, dynamic SQL and a catalog write. **Closing them is
> general PL/pgSQL by any reading**, so the ruling and the remaining row point the same way: leave
> them refused. There is no version of this unit that both honours the ruling and moves the 2.

**Closed question, ruled 2026-09-04.** The two stay refused. They are general PL/pgSQL and the
ruling excludes it; `docs/acceptance/v1.md`'s divergence row now says so in those words, with the
run numbers, so the next reader does not re-open it from a stale figure.

## 3. What is actually worth doing: a capture nothing replays

`crates/esker-sql/tests/corpus/pg19_do_block.txt` is **30 statements captured against PostgreSQL 19
and replayed by no test**. `tests/do_block.rs` replays `pg19_do_create_enum.txt` only; the file
named after it is not read by anything.

It is not alone — four corpora in the repo are unreplayed:

| capture | owner |
|---|---|
| `pg19_do_block.txt` | this lane |
| `pg19_lower.txt` | unclaimed |
| `pg19_pg_trgm.txt` | b4's in-flight ADR 0070 unit |
| `pg19_storage_parameters.txt` | unclaimed |

**A capture nobody replays is a measurement nobody is holding the node to.** Probed directly, the
node answers the two templates and refuses every other form in that file:

| form | line | PostgreSQL | this node |
|---|---:|---|---|
| `RAISE NOTICE` template | 54 | command | **command** ✓ |
| enum guard template | 49, 52 | command | **command** ✓ |
| `DECLARE n integer; … SELECT … INTO n` | 55 | command | `0A000` |
| `DO LANGUAGE plpgsql $$ … $$` | 56 | command | `0A000` |
| `DO $do$ … $do$` (a named dollar tag) | 57 | command | `0A000` |
| `BEGIN CREATE TABLE … END` | 70 | command, and the table exists | `0A000` |
| `RAISE EXCEPTION 'boom'` | 59 | `P0001 boom` | `0A000` |
| `BEGIN SELECT 1 END` (missing `;`) | 62 | `42601` | `0A000` |
| `… $$ LANGUAGE nosuchlang` | 65 | `42704` | `0A000` |
| `SELECT DO $$ … $$` | 68 | `42601` | `0A000` |

## 4. Scope, under "DO minimal"

**In**, because each is a form the capture holds and none is a language feature:

1. **Replay the capture.** `tests/do_block.rs` gains a second test over `pg19_do_block.txt`. This
   is the whole point: the forms below are then held to the oracle rather than to my reading.
2. **`DO $do$ … $do$`** — a named dollar tag is *lexical*, not semantic. The body between `$do$`
   and `$do$` is the same body; only the delimiter differs. Refusing it is a parser gap.
3. **`DO LANGUAGE plpgsql $$ … $$`** and the trailing `LANGUAGE plpgsql` form — the language named
   is the one already assumed. ADR 0058 refused it on the grounds that "a body that says which
   language it is in is not one to guess at"; that reason holds for an *unknown* language and not
   for `plpgsql` itself. **`LANGUAGE nosuchlang` stays `42704`**, which the capture pins.
4. **`RAISE EXCEPTION`** → `P0001` with the raised text as the whole message. ADR 0058 deferred it
   for a good reason — routing it through the notice path would turn a failed statement into a
   successful one — but that argues for a *separate* path, not for refusing it. The capture pins
   both the code and the message.

**Out, and each for its own reason:**

* **`DECLARE` + `SELECT … INTO`** — a variable and an assignment. That is the language, and the
  ruling excludes it. It is one capture row and no suite test.
* **`BEGIN CREATE TABLE … END`** — running arbitrary statements inside a block is an interpreter
  with one statement in it today and two tomorrow. One capture row, no suite test.
* **`check_all_foreign_keys_valid!`** — §2. The 2 tests stay red, deliberately, and the deferral
  sheet should say so in those words rather than carry a stale 34.
* **`RAISE INFO` / `LOG` / `DEBUG`** — ADR 0058's reason stands unchanged: this wire has four
  severities and downgrading one prints the wrong word to a client reading exactly that word.

## 5. Order

1. Replay `pg19_do_block.txt` and let it say which of §4's four are already right and which are not.
   **Expect the list above to be wrong somewhere** — that is what replaying a capture is for.
2. The dollar tag, then `LANGUAGE plpgsql`, then `RAISE EXCEPTION`, each with the capture's own
   answer as the test.
3. Amend ADR 0058 with what changed and why — its §"deliberately not implemented" loses two entries
   and keeps the rest.
4. Correct the deferral sheet: `DO` is **2 tests**, they are `check_all_foreign_keys_valid!`, and
   they are refused by a ruling rather than by an absence.

## 6. What this unit will not claim

It will not move the board. The two remaining `DO` tests are out of scope by the ruling, so the
honest statement of the outcome is **"the capture is now replayed and four forms answer where they
did not"** — not a test-count delta. A unit that improves the node without moving a number is still
worth doing; saying otherwise is how a stale 34 survives fourteen runs.
