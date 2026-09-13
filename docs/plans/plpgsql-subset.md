# Plan — the PL/pgSQL subset: `DO` blocks and row triggers

Status: **written before any code** (lane s2-plpgsql, 2026-09-13, branch `plpgsql` from `2177e244`).
[ADR 0113](../adr/0113-plpgsql-is-the-subset-the-suite-sends.md) is the decision, **accepted by
the user on 2026-09-13** together with option (b) of §6; this file is the census it rests on and
the order of work.

**Authority.** The user ruled on 2026-09-13 that the node's remaining capability gaps are to be
closed. That reopens two standing rulings this file would otherwise have to obey:

* **"DO minimal"** (2026-09-04, [ADR 0058](../adr/0058-a-do-block-is-two-templates-not-a-language.md),
  the closed question in [`do-blocks.md`](do-blocks.md) §2), which kept `check_all_foreign_keys_valid!`
  refused;
* **"trigger bodies are status quo"** (`docs/acceptance/v1.1.md` §3), which kept
  `persistence_test.rb`'s trigger unfired.

Both are pinned by tests, and §9 lists every assertion that changes. **A third ruling is not
reopened by this file**: a write to a system catalog is `42501`
(`tests/user_decided_divergences.rs::a_write_to_a_system_catalog_is_refused`), and the suite's
`DO` body writes one. §6 says why that decides how far this unit can move the suite.

## 1. Why `persistence_test.rb` never saw a `CREATE TRIGGER`

**It did. The log could not show it.**

* The statement log keeps the first **120 characters** of the source
  (`crates/esker-sql/src/stmt_stats.rs:252`), and the source it is given is `Parsed::source()`
  (`exec/mod.rs:4739` → `:1503`) — **the whole message**, where `Parsed::text()` is the statement.
* Rails sends statement 790 as one message: `CREATE OR REPLACE FUNCTION populate_column() … ;
  CREATE TRIGGER before_insert_trigger …` (`postgresql_specific_schema.rb:215-231`). Both statements
  carry the message's first 120 characters as their label, and those end inside the function body
  at `SELECT MAX(id)`.
* Counted in `results/node-logs/node-attempt6-98958255.log`: the 790 label appears
  **738 = 2 × 369** times — a function and a trigger per schema load — and the 762 label
  (`CREATE TABLE postgresql_partitioned_table_parent (`) **1,476 = 4 × 369** times: two tables, a
  function and a trigger. Each line has its own prewrite and commit counts, so each is a statement
  that ran and committed.
* So both triggers exist after every load (`tests/trigger_function.rs::statement_790_defines_a_function_and_a_trigger`)
  and **neither fires** (`::a_stored_trigger_does_not_fire`). `PkAutopopulatedByATriggerRecord.create`
  sends `INSERT INTO "pk_autopopulated_by_a_trigger_records" DEFAULT VALUES RETURNING "id"` (once in
  the log), nothing fills `id`, and `NOT NULL` answers `23502` — correctly, for a row that really is
  `(null)`. `CREATE FUNCTION … LANGUAGE plpgsql` is not refused: it is stored verbatim
  (`exec/ddl.rs::create_function`).

"`CREATE TRIGGER` appears zero times" (`r1-a6-13e.txt`) is a true count of a truncated label.

## 2. The census

Sources: the gem the suite runs, `vendor/bundle/ruby/4.0.0/gems/activerecord-8.1.3.1/lib/`
(byte-identical to the checkout's `lib/` for `referential_integrity.rb`, `postgresql_adapter.rb`
and `fixtures.rb`), the checkout's `activerecord/test/`, and the attempt 6 node log. A refused
`DO` leaves no line in that log (no line carries `0A000`, and the only lowercase `do $$` lines are
the seven `RAISE WARNING`s), so the refused calls are counted from their call sites.

### 2.1 `DO` bodies

| # | shape | sent by | attempt 6 | constructs |
|---|---|---|---:|---|
| D1 | `DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_type t JOIN pg_namespace n ON t.typnamespace = n.oid WHERE t.typname = … AND n.nspname = …) THEN CREATE TYPE … AS ENUM (…); END IF; END $$;` | `create_enum`, `postgresql_adapter.rb:556` | 40 statements | block · `IF … THEN … END IF` · SQL condition · embedded DDL |
| D2 | `do $$ BEGIN RAISE WARNING '<text>'; END; $$` | `postgresql_adapter_test.rb:697, 709, 717, 731, 748, 757, 766` | 7 statements | block · `RAISE WARNING` with a literal |
| D3 | `do $$ declare r record; BEGIN FOR r IN (SELECT FORMAT('UPDATE pg_catalog.pg_constraint SET convalidated=false WHERE conname = ''%1$I'' AND connamespace::regnamespace = ''%2$I''::regnamespace; ALTER TABLE %2$I.%3$I VALIDATE CONSTRAINT %1$I;', constraint_name, table_schema, table_name) AS constraint_check FROM information_schema.table_constraints WHERE constraint_type = 'FOREIGN KEY') LOOP EXECUTE (r.constraint_check); END LOOP; END; $$;` | `check_all_foreign_keys_valid!`, `referential_integrity.rb:41` | 3 calls, all `0A000 DO is not supported` | `DECLARE … record` · `FOR r IN (query) LOOP … END LOOP` · a record's field · `EXECUTE (expr)` of a **two-statement** string |

D1 and D2 answer today, through ADR 0058's two templates. D3 is the whole of three failing tests:

* `fixtures_test.rb:900` `test_raises_fk_violations` (the F) and `:926`
  `test_does_not_raise_if_no_fk_violations` (the E), through `fixtures.rb:686` →
  `:696 check_all_foreign_keys_valid!(conn)` with `verify_foreign_keys_for_fixtures = true`;
* `referential_integrity_test.rb:106`
  `test_all_foreign_keys_valid_having_foreign_keys_in_multiple_schemas` (the E).

### 2.2 `CREATE FUNCTION … LANGUAGE plpgsql`

| # | function | sent by | attempt 6 | body | constructs |
|---|---|---|---:|---|---|
| F1 | `partitioned_insert_trigger()` `RETURNS TRIGGER` | statement 762, `postgresql_specific_schema.rb:108`, one message with two `CREATE TABLE`s and T1 | 369 | `BEGIN INSERT INTO postgresql_partitioned_table VALUES (NEW.*); RETURN NULL; END;` | embedded `INSERT` · `NEW.*` · `RETURN NULL` |
| F2 | `populate_column()` `RETURNS TRIGGER` | statement 790, `:216` | 369 | `DECLARE max_value INTEGER; BEGIN SELECT MAX(id) INTO max_value FROM pk_autopopulated_by_a_trigger_records; NEW.id = COALESCE(max_value, 0) + 1; RETURN NEW; END;` | `DECLARE` of a scalar · `SELECT … INTO` · `NEW.<column> = <expr>` · `RETURN NEW` |

### 2.3 Triggers

| # | statement | sent by | attempt 6 | fired by |
|---|---|---|---:|---|
| T1 | `CREATE TRIGGER insert_partitioning_trigger BEFORE INSERT ON postgresql_partitioned_table_parent FOR EACH ROW EXECUTE PROCEDURE partitioned_insert_trigger();` | 762, `:117` | 369 | `postgresql_adapter_test.rb:209, 216, 223, 230` — four `exec_insert`s into the parent (4 in the log) |
| T2 | `CREATE TRIGGER before_insert_trigger BEFORE INSERT ON pk_autopopulated_by_a_trigger_records FOR EACH ROW EXECUTE FUNCTION populate_column();` | 790, `:227` | 369 | `persistence_test.rb:1707` (1 in the log) |
| — | `ALTER TABLE … DISABLE TRIGGER ALL` / `… ENABLE TRIGGER ALL` | `referential_integrity.rb`, around every fixture load | 593,609 / 593,346 statements | — |

**Nowhere in the suite**: a statement-level trigger, an `AFTER` trigger, an `UPDATE` or `DELETE`
trigger, `WHEN`, trigger arguments, `DISABLE TRIGGER USER`, or a trigger disabled by name.

### 2.4 The union — what the subset is

| construct | D1 | D2 | D3 | F1 | F2 | brief / capture |
|---|:-:|:-:|:-:|:-:|:-:|---|
| block `[DECLARE …] BEGIN … END` | ✓ | ✓ | ✓ | ✓ | ✓ | |
| `DECLARE <name> <sql type>;` | | | | | ✓ | |
| `DECLARE <name> record;` | | | ✓ | | | |
| `IF <condition> THEN … END IF;` | ✓ | | | | | |
| `RAISE NOTICE \| WARNING \| EXCEPTION '<literal>';` | | ✓ | | | | `NOTICE`, `EXCEPTION`: `pg19_do_block.txt` |
| `NULL;` | | | | | | `pg19_do_block.txt` |
| `SELECT <expr> INTO <variable> …;` | | | | | ✓ | |
| `<variable> = <expr>;` / `NEW.<column> = <expr>;` (and `:=`) | | | | | ✓ | |
| `FOR <record> IN <query> LOOP … END LOOP;` | | | ✓ | | | |
| `EXECUTE <expr>;` — no `INTO`, no `USING` | | | ✓ | | | |
| any other SQL statement, variables substituted | ✓ | | | ✓ | | |
| `RETURN NEW;` / `RETURN NULL;` | | | | ✓ | ✓ | `RETURN OLD`: brief (§7) |
| the `NEW` record; `NEW.*` in a `VALUES` list | | | | ✓ | ✓ | `OLD`: brief (§7) |
| `BEFORE … INSERT … FOR EACH ROW` | | | | ✓ | ✓ | `AFTER`, `UPDATE`, `DELETE`: brief (§7) |

`DO LANGUAGE plpgsql $$ … $$`, a trailing `LANGUAGE plpgsql`, a named dollar tag and
`LANGUAGE nosuchlang` → `42704` answer today and keep answering (`tests/do_block.rs`).

## 3. What the oracle said

One `psql` session against `esker-pg19` (PostgreSQL 19beta1, as `esker`, a superuser), inside
`BEGIN … ROLLBACK` with `ON_ERROR_ROLLBACK`, 2026-09-13. The facts that decide scope:

1. **D3 over a clean foreign key** → `DO`, and `pg_constraint.convalidated` is `t` afterwards (the
   loop sets it `false` and `VALIDATE CONSTRAINT` sets it back).
2. **D3 after a row slipped in under `DISABLE TRIGGER ALL`** →
   `23503 insert or update on table "s2_child" violates foreign key constraint "s2_fk"`,
   `DETAIL: Key (parent_id)=(99) is not present in table "s2_parent".`, and a `CONTEXT` naming the
   executed string and `PL/pgSQL function inline_code_block line 14 at EXECUTE`.
3. **`ALTER TABLE public.s2_child VALIDATE CONSTRAINT s2_fk` on a constraint already validated is a
   no-op** — `ALTER TABLE`, with the violating row still in the table. So D3's
   `UPDATE pg_catalog.pg_constraint SET convalidated=false` is load-bearing: without it
   `test_raises_fk_violations` fails on PostgreSQL too. This node's `validate_constraint`
   (`exec/ddl.rs:1551`) returns early on a validated constraint the same way.
4. **`EXECUTE 'CREATE TABLE s2_x (a int); CREATE TABLE s2_y (a int)'`** → `DO`, and both tables
   exist. One string, two statements, both run.
5. **F2's shape**: `INSERT … DEFAULT VALUES RETURNING id` answers `1`, then `2`; an explicit
   `VALUES (NULL)` answers `3` — the trigger overwrites it. (`pg19_trigger_function.txt:84-119`
   captured the same function earlier, including `99` becoming `4`.)
6. **F1's shape**: `insert into parent (number) VALUES (1)` → **`INSERT 0 0`**; with
   `RETURNING id` → **no row** and `INSERT 0 0`; `select max(id) from parent` → `3` (the child's rows,
   through inheritance); `select count(*) from only parent` → `0`; `currval` → `3`.
7. `FORMAT('%1$I|%2$I|%3$I|%s|%L', 'Ab c', 'public', 'x', 'y''z', 'q''r')` →
   `"Ab c"|public|x|y'z|'q''r'`.
8. `'public'::regnamespace` → `public`; `::oid` → `2200`;
   `'pg_catalog'::regnamespace = 11::oid::regnamespace` → `t`.
9. `do $$ BEGIN RAISE WARNING 'foo'; END; $$` → `WARNING 01000 foo`, tag `DO`.

**Fact 6 is the one a reader would get wrong**: the moment triggers fire, T1 fires too, and the four
`postgresql_adapter_test.rb` tests that pass today — because the row lands in the parent — must keep
passing with the row in the child. That is part of C's acceptance, not an afterthought.

## 4. The subset, construct by construct

* **Reading a body.** A PL/pgSQL tokenizer (`'…'` with `''`, `E'…'`, dollar quotes, quoted
  identifiers, `--` and nested `/* */` comments) and a recursive-descent reader of §2.4. An SQL
  fragment runs to the `;` at bracket depth zero outside any literal — the rule PostgreSQL's own
  grammar uses for an SQL statement inside a body.
* **A malformed body** is `42601` with PostgreSQL's sentence where a capture has one
  (`BEGIN SELECT 1 END` → `unexpected end of function definition at end of input`,
  `pg19_do_block.txt:62`). **A construct PostgreSQL has and the subset does not** is `0A000` naming it
  (ADR 0113 decision 7; §11 is the list).
* **Variables.** `DECLARE x <type>` starts `NULL` with that type, and assigning to it casts to it
  the way an `INSERT` into a column of that type does. A `record` has no shape until a `FOR`
  assigns a row; a field is the column of that name in the query's result.
* **`SELECT <expr> INTO <variable> …`**: the `INTO <variable>` is cut out of the fragment, the
  `SELECT` runs, and the first row's first column is assigned. **No row assigns `NULL`** (not
  `STRICT`, PostgreSQL's default), and **columns and rows past the first are ignored** —
  `SELECT 1, 2 INTO n` and a three-row `SELECT … INTO n` both answer `DO`, measured. One target;
  a list of targets or a record target is §11.
* **A `SELECT` with no `INTO`** is PostgreSQL's `42601 query has no destination for result data`
  (captured in B before it is asserted).
* **`IF`**: the condition runs as a `SELECT` of it; `NULL` is false.
* **`FOR r IN <query> LOOP`**: the query runs once and its rows are read before the body runs,
  which is what PostgreSQL's cursor snapshot gives a loop body that writes. The cost is memory for a
  large result; the suite's query is `information_schema.table_constraints`.
* **`EXECUTE <expr>`**: the expression is evaluated to text, parsed as one or more statements, and
  each runs in turn; rows are discarded (fact 4).
* **`RAISE`**: `NOTICE` and `WARNING` through `Executor::notice`, the path ADR 0058's template uses;
  `EXCEPTION` — and a `RAISE` with no level, which means it — is `P0001` with the literal as the
  whole message. `%%` in the literal is `%`, and a lone `%` with no argument is PostgreSQL's
  `42601 too few parameters specified for RAISE` (`'100%'` and `'%%%'`, measured). A bare
  `RAISE;` is PostgreSQL's own `0Z002 RAISE without parameters cannot be used outside an exception
  handler`, which is every place it can appear in the subset.
* **`RETURN`**: in a trigger function `RETURN NEW | OLD | NULL` — a `BEFORE` trigger's returned row
  replaces the row being written, `NULL` skips it, an `AFTER` trigger's is ignored. In a `DO`,
  `RETURN;` ends the block and `RETURN <expr>` is PostgreSQL's
  `42804 RETURN cannot have a parameter in function returning void`.
* **A malformed body answers PostgreSQL's sentence**, all measured: an expression cut off by `;`
  is `missing "THEN" | "LOOP" at end of SQL expression`, one cut off by the end of the body is
  `syntax error at end of input`, an SQL statement cut off by it is `unexpected end of function
  definition at end of input`, an empty one is `missing expression at or near "<token>"`, an
  unknown target is `"x" is not a known variable`, and a declaration's own are
  `incomplete data type declaration at end of input` and `duplicate declaration at or near "n"`.

## 5. Execution model

* **Where.** A new module, `crates/esker-sql/src/plpgsql/`, synchronous, no dependency.
* **One statement, one transaction.** A `DO` lowers to `plan::Statement::Do { body }`, and
  `Executor::run_recording` hands it to the interpreter with the statement's own `txn` and
  `Written`. Every SQL fragment inside is parsed, lowered, bound and run through
  `Executor::run_recording` — the function a top-level statement reaches — so its writes are in the
  statement's buffer, a catalog write sets `catalog_written` exactly as top-level DDL does
  (ADR 0106), and an error anywhere is the `DO`'s error, undone by the implicit savepoint every
  statement already has (ADR 0057). A trigger's body runs inside the `INSERT`, `UPDATE` or `DELETE`
  that fired it, in that statement's transaction: there is no second transaction and no autonomous
  write.
* **Variables reach SQL as typed values, never as text.** A reference in a fragment — `max_value`,
  `r.constraint_check`, `NEW.id`, and `NEW.*` in a `VALUES` list — is rewritten to `$n` and bound as
  a value of the variable's type or the column's, through the binder's substitution step. Printing
  a value into SQL and parsing it back is the round trip that loses a type or a precision;
  PostgreSQL binds a variable as a parameter too. `exec/bind.rs::substitute` reads wire bytes today,
  so this is one function beside it that takes values.
* **Query results come back as values.** `SELECT … INTO`, an `IF` and a `FOR` need `Datum`s, and
  `Outcome::Rows` carries rendered bytes; the interpreter reads through the query path before
  rendering (risk 4).
* **Nesting is bounded.** A trigger whose body writes its own table fires itself; PostgreSQL stops
  at `max_stack_depth` with `54001 stack depth limit exceeded`. The executor counts depth and answers
  that code past a fixed bound (captured in C).
* **A restart re-runs the body.** A fragment that waited on a row lock returns
  `StatementMustRestart`, which leaves the block and re-runs the whole top-level statement from its
  savepoint (ADR 0057). Writes are undone by that machinery; a `RAISE NOTICE` queued before the wait
  is sent again. Named, not fixed.
* **Guards belong to the top-level statement.** The statement timeout, cancellation,
  `pg_stat_activity.query` and the statement stats are taken once, by the `DO` or the `INSERT`; a
  fragment takes none of its own.
* **Errors pass through unchanged** — code, message, `DETAIL`. PostgreSQL adds a `CONTEXT`
  (fact 2); this node's errors carry no `CONTEXT` on the wire today, and adding one is §11.
* **ADR 0058's templates go.** D1 and D2 become ordinary bodies: `parse::strip_do_create_enum`,
  `parse::strip_do_raise`, `Parsed::is_do_guarded`, `plan::Statement::Raise` and
  `CreateType::if_not_exists` are deleted. One grammar, one reader — D1's 40 statements a pass are
  the regression check that the interpreter reads what the template read.

## 6. The SQL around D3, and the ruling it meets

D3 needs five things. One of them is PL/pgSQL.

1. **`format()`** — `%s`, `%I`, `%L`, `%%` and the positional `%n$` form (fact 7). Not implemented:
   `plan/expr.rs` has `format_type` and no `format`. Width and `-` flags are not in the census,
   and they are built anyway: the capture of `format()` pins them (`pg19_format.txt`) and they are
   a few lines, where a declared divergence would have been a sentence per row. Only a positional
   width, `%*2$s`, is refused by name.
2. **`regnamespace`** — text → `regnamespace`, `oid` → `regnamespace`, and `=` between two
   (fact 8). Not implemented; built the way `regclass` and `regproc` are
   ([ADR 0098](../adr/0098-regproc-is-an-oid-that-prints-as-a-function.md)).
3. **`EXECUTE` of a two-statement string** — §4.
4. **`ALTER TABLE <schema>.<table> VALIDATE CONSTRAINT <name>`** — exists.
5. **`UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE conname = … AND
   connamespace::regnamespace = …` — refused `42501` by the user's ruling**, pinned by
   `user_decided_divergences.rs::a_write_to_a_system_catalog_is_refused` (whose list holds this very
   statement), and fact 3 shows it cannot be dropped from the body.

**So the interpreter alone moves D3's three tests from `0A000` to `42501`, and no further.**
`esker-coord/QUESTION-s2.md` puts it to the user:

* **(a)** keep `42501`, declared — the three stay failing, now on the catalog-write ruling rather
  than on `DO`;
* **(b)** accept exactly `UPDATE pg_catalog.pg_constraint SET convalidated = <boolean constant>
  WHERE <predicate>`. The predicate runs over the `pg_constraint` view's rows; every row it selects
  must be a foreign key or a `CHECK`, whose flag is stored today in its table's record
  (`ForeignKeyDef::validated`, `CheckDef::validated` — the `NOT VALID` state). The flag is written
  through `catalog::replace_table` with the schema version bumped, as `VALIDATE CONSTRAINT` writes
  it, and the tag is `UPDATE n`. Another column, another catalog, or a selected row with no flag to
  write stays `42501`. No record kind, no field, no format change.

Recommended: **(b)** — it is the last obstacle for those three tests, and what it writes is a state
the catalog already represents.

**Ruled (b) by the user, 2026-09-13.** Only `convalidated`, only on the flag a foreign key or a
`CHECK` already stores; every other write to a system catalog stays `42501`; no format change. B's
last slice builds it.

## 7. Triggers

### Where a row trigger fires

PostgreSQL, for one row: defaults and sequences → **`BEFORE ROW`** → generated columns →
`NOT NULL`, `CHECK`, the partition constraint → unique indexes → **`AFTER ROW`**, queued to the end
of the statement (a foreign key's own checks are `AFTER` triggers too). Several triggers on one
event fire in name order.

`exec/dml.rs`, mapped onto that:

* **`insert`**, per row: `row_at_defaults` → values → sequences → row id → `fit_typmods` →
  **`BEFORE ROW INSERT`** → `fill_generated` → `check_not_null` → domain and `CHECK` → partition
  route → `ON CONFLICT` → `write_row` (unique, foreign key) → `RETURNING`. **`AFTER ROW INSERT`** is
  queued and fired after the loop. `RETURN NULL` writes nothing, returns nothing and counts nothing:
  the tag is `INSERT 0 <rows written>` (fact 6), where today it is `insert.rows.len()`. A returned
  row's values are fitted to their columns again.
* **`update`**, per row of each target: assignments → `fit_typmods` → **`BEFORE ROW UPDATE`**
  (`OLD`, `NEW`) → generated, `NOT NULL`, `CHECK` → foreign key refusal and cascade → move or write →
  **`AFTER ROW UPDATE`** queued. `RETURN NULL` skips the row and its count.
* **`delete`**, per row: **`BEFORE ROW DELETE`** (`OLD`) → `on_parent_removed` → `remove_row` →
  **`AFTER ROW DELETE`** queued. `RETURN NULL` skips the row; the tag counts rows removed, where
  today it is `rows.len()`.
* **`AFTER` triggers fire once the statement's rows are written**, in name order, and each sees
  every row the statement wrote.
* **Inheritance.** An `UPDATE` or `DELETE` on a parent reaches its children's rows
  (`inheritance_targets`), and each row fires the triggers of the table it is in — PostgreSQL's rule
  per result relation. An `INSERT` fires the triggers of the table it names. T1 is exactly this: the
  insert into the parent fires T1, and T1's `INSERT INTO postgresql_partitioned_table` fires the
  child's (none).
* **A foreign key's `CASCADE`, `SET NULL` and `SET DEFAULT` fire the child's row triggers**, because
  PostgreSQL's referential actions are statements on the child. Decided by reading
  `foreign_key::cascade_*`, which reach the same hooks for the cost of a call, and measured
  (`pg19_plpgsql_trigger.txt`, third session): `ON UPDATE CASCADE` and `SET NULL` fire `UPDATE`
  triggers, a child's `BEFORE DELETE` answering `RETURN NULL` keeps the child with no error, and a
  child's `BEFORE UPDATE` that points the key elsewhere is `23503`.
* **What a body sees and what it may answer**, all measured: `OLD` in an `INSERT` trigger and `NEW`
  in a `DELETE` one read `NULL`, and returned as they are they are `RETURN NULL`; assigning one of
  their fields makes a row. A generated column reads `NULL` in a `BEFORE` trigger, on `UPDATE` too.
  `RETURN` of a `NULL` value is `RETURN NULL`, of any other value that is not a row `42804`, and a
  body that ends without `RETURN` is `2F005`, an `AFTER` trigger's included.

### Refused rather than half-fired (`0A000`, naming it)

* **`CREATE TRIGGER … FOR EACH STATEMENT`.** Accepted and stored today — `lower_create_trigger`
  reads it as `for_each_row = false` — which becomes a trigger that silently never fires the moment
  row triggers do.
* **Trigger arguments**, `EXECUTE FUNCTION f('x')`: `lower_create_trigger` does not read them today,
  so they are dropped in silence. Refused for the same reason.
* **`CREATE TRIGGER` on a partitioned table or on a partition.** PostgreSQL clones a partitioned
  table's row triggers onto its partitions and fires a partition's after routing; this node routes
  after `CHECK`, so the order would be a new one to build.
* **`INSERT … ON CONFLICT` into a table with an enabled row trigger.** PostgreSQL fires
  `BEFORE INSERT` and then, on the conflict path, `BEFORE UPDATE` — a second ordering.
* Already refused where the statement is lowered, unchanged: `WHEN`, `UPDATE OF <columns>`,
  `TRUNCATE`, `INSTEAD OF`, `CREATE CONSTRAINT TRIGGER`, `CREATE OR REPLACE TRIGGER`.

### Enabling and disabling

* **`ALL`** sets `TableDef::triggers_disabled`, which suspends the foreign-key checks
  (`exec/foreign_key.rs::enforcing`), and flips every user trigger's `TriggerDef::enabled` —
  PostgreSQL's `ALL` covers both, and `pg_trigger.tgenabled` shows `D`. **This is the statement
  Rails wraps every fixture load in**, so a disabled trigger does not fire.
* **`USER`** flips the user triggers and leaves the foreign key's alone — measured both ways:
  `ENABLE TRIGGER USER` after `DISABLE TRIGGER ALL` still lets a row with no parent in.
* **A trigger's name** flips that trigger, and a name the table does not have is `42704` from the
  executor, naming the relation bare — `pg19_trigger_function.txt` captured `23502` while the
  trigger is disabled and `4` once it is enabled again.
* A trigger's own flag moves the table's schema version, as `CREATE TRIGGER` does; `ALL`'s
  referential flag alone does not, as before.
* `DROP TRIGGER`, `pg_trigger`, `pg_get_triggerdef` and `DROP FUNCTION`'s `2BP01` exist
  (`tests/trigger_function.rs`) and do not change.

### A trigger function that is not one

`CREATE FUNCTION f() RETURNS integer … LANGUAGE plpgsql` is stored without its return type —
`FunctionDef` has none — so `CREATE TRIGGER … EXECUTE FUNCTION f()` cannot give PostgreSQL's `42P17`
for a function that does not return `trigger`. Storing the return type is a field in the function
record, which is a format change, and no suite statement needs it: declared, not built.
`LANGUAGE sql` returning `trigger` is refused where it is lowered, with PostgreSQL's `42P13`; no
other language is stored (`42704`), so every function a trigger can name is PL/pgSQL.

## 8. Catalog records

* **A function's body**: the `KIND_FUNCTION` record, `version ++ id ++ language ++ body`
  (`catalog/record.rs:1585`), the body verbatim.
* **A table's triggers**: in the table record since version 18 (`record.rs:2226`) — `name`,
  `before`, `events`, `for_each_row`, `function`, `enabled`; `triggers_disabled` since version 12.
* **The `pg_constraint` write, if ruled (b)**: `ForeignKeyDef::validated` and `CheckDef::validated`,
  stored today.

**No new record kind, no new field, no format version.** A body is read each time its trigger fires,
and nothing parsed is stored or cached.

## 9. Assertions that change, and the ruling that changes each

| test | today | after | by |
|---|---|---|---|
| `user_decided_divergences.rs::a_do_block_with_a_loop_is_refused_by_name` | `!0A000 DO is not supported` | PostgreSQL's `DO` | 2026-09-13 |
| `user_decided_divergences.rs::the_create_enum_guard_is_still_read` | the template | the same answer, through the interpreter | — |
| `user_decided_divergences.rs::a_write_to_a_system_catalog_is_refused` | three statements → `42501` | the `pg_constraint` row answers PostgreSQL's `UPDATE 0`; the `pg_depend` and `pg_class` rows stay `42501` | §6, ruled (b) |
| `do_block.rs::a_body_that_is_not_the_template_is_refused_by_name` | five bodies → `0A000` | each as `pg19_do_block.txt` answers it; the by-name half moves to constructs outside the subset | 2026-09-13 |
| `do_block.rs::the_forms_around_the_templates_answer_as_postgresql_does` | three bodies → `0A000 DO` | the capture's answers | 2026-09-13 |
| `do_block.rs::raise_notice_and_warning_reach_the_client` | `RAISE INFO`, `LOG`, `WARNING 'a', 'b'` → `0A000` | unchanged: outside the subset | — |
| `trigger_function.rs::a_stored_trigger_does_not_fire`, now `::a_stored_trigger_fires` | `23502` | the trigger's value is written | 2026-09-13 |
| `trigger_function.rs::every_trigger_function_answer_is_postgresql_19_s` | the replay aborts at line 80 | that line re-captured in a savepoint of its own, so the replay reaches the firing rows | 2026-09-13 |
| `ddl_cascade_fk_trigger.rs::a_named_trigger_does_not_exist` | `42704` from the lowering, for every name | `42704` from the executor, which knows the table's triggers — the same answers | — |

None is deleted or skipped; each moves to the oracle's answer. `docs/acceptance/v1.1.md` is the
record of a tag and is not edited; the new state lives here and in ADR 0113.

## 10. Order of work — one commit per slice

**A. Docs.** This plan, ADR 0113 (Proposed), `QUESTION-s2.md`. Handover.

**B. `DO`.**

1. `plpgsql` tokenizer, reader and syntax tree, with unit tests: every construct in §2.4 reads, every
   construct in §11 is `0A000` naming it, the captured malformed body is `42601`. Not wired.
2. The interpreter and `Statement::Do`; the templates deleted. Red first: `DECLARE … SELECT INTO`,
   `IF`, `CREATE TABLE` inside a block, `NULL;`. Green through the interpreter: D1
   (`pg19_do_create_enum.txt`'s replay), D2, `RAISE EXCEPTION`.
3. `FOR … IN <query> LOOP`, record fields, `EXECUTE` of one and of two statements.
4. `format()`.
5. `regnamespace`.
6. D3 end to end, as ruled (b): the narrow `pg_constraint` write and the three suite shapes —
   clean → `DO`, a violation → `23503` naming the table, two schemas → `DO`.

Capture `tests/corpus/pg19_plpgsql_do.txt` in one session, `BEGIN … ROLLBACK`, **a savepoint around
every statement** — ADR 0058's amendment is what a capture without them costs. Handover.

**C. Triggers.**

1. `BEFORE ROW INSERT`, `RETURN NEW`, `RETURN NULL`, `NEW.*`. Red first: `a_stored_trigger_does_not_fire`
   inverted, the persistence statement answering `1` then `2`, and T1's four-statement sequence from
   fact 6.
2. `BEFORE` and `AFTER` × `UPDATE` and `DELETE`, `AFTER INSERT`, `OLD`.
3. `DISABLE` / `ENABLE TRIGGER ALL | USER | <name>`; the refusals of §7; the depth bound.
4. `pg19_trigger_function.txt` replayed through firing; `tests/corpus/pg19_plpgsql_trigger.txt`
   captured for 2 and 3. Handover.

`docs/DESIGN.md` §13 gains the interpreter with B and the triggers with C.

## 11. Not doing

Refused by name with `0A000`, or left as it is:

* **Statements and forms**: `ELSIF` and `ELSE`, `CASE`, `LOOP`, `WHILE`, `EXIT`, `CONTINUE`, an
  integer `FOR i IN 1..n`, `FOREACH`, labels, a nested `BEGIN … END`, `EXCEPTION WHEN` (a
  subtransaction), `GET DIAGNOSTICS`, `PERFORM`, `ASSERT`, `CALL`, `COMMIT` or `ROLLBACK` in a body,
  `RETURN NEXT`, `RETURN QUERY`, `RETURN <expr>` outside a trigger, cursors (`OPEN`, `FETCH`,
  `MOVE`, `CLOSE`, `refcursor`, a `CURSOR` declaration), `EXECUTE … INTO` and `… USING`,
  `SELECT … INTO STRICT`, `SELECT` into more than one target or into a record, `%TYPE`,
  `%ROWTYPE`, `ALIAS`, `CONSTANT`, `NOT NULL` or `:= <default>` in a declaration, `RAISE` with
  format arguments, with `USING`, or at `INFO`, `LOG` or `DEBUG`, the `TG_*` variables.
* **Functions**: a function called from SQL (`SELECT f()` stays `0A000`), arguments, a return type
  other than `trigger` taking effect, procedures.
* **Validation at `CREATE FUNCTION`**: PostgreSQL parses the body there; this node stores it, as it
  has since the define-only unit (`pg19_trigger_function.txt`'s `tf_badbody` row).
* **`plpgsql.variable_conflict`'s other settings.** Its default, `error`, **is** built — a name that
  is both a variable and a column of the statement's relation is `42702` with PostgreSQL's `DETAIL`,
  measured — so this line is only `use_variable` and `use_column`.
* **A `CONTEXT` line** on an error raised inside a body.
* **Triggers**: statement-level, `WHEN`, `UPDATE OF`, arguments, `TRUNCATE`, `INSTEAD OF`,
  constraint triggers, transition tables (`REFERENCING`), on partitioned tables or partitions, event
  triggers, `ENABLE REPLICA | ALWAYS`, `session_replication_role`.
* **A function's stored return type** — a format change (§7).

`ELSIF` and `ELSE` are a few lines once `IF` exists. They are listed rather than built because the
suite does not send them, and the brief's scope is the census: no more, no less.

## 12. Risks

1. **T1 fires the moment triggers do** (fact 6). Four passing tests depend on `NEW.*`, an insert into
   an inheritance child from inside a trigger, `INSERT 0 0`, a `RETURNING` with no row, and
   `max(id)` over the parent reaching the child. C1 tests that sequence before a pass can see it.
   Built, and pinned by `plpgsql_trigger.rs::statement_762s_trigger_moves_each_row_into_the_child`.
2. **D1 runs 40 times a pass** and moves from a template to the interpreter; a regression there is
   `enum_test.rb`. `pg19_do_create_enum.txt`'s replay is the check.
3. **The catalog-write ruling** (§6): under (a), B moves no suite test.
4. **Values out of a query inside the executor.** `Outcome::Rows` is rendered bytes; if the query
   path cannot hand back `Datum`s without a new seam, B2 grows by that seam.
5. **The hot path.** Every row an `INSERT`, `UPDATE` or `DELETE` writes asks whether its table has an
   enabled row trigger — a walk of `TableDef::triggers`, empty for a table with none — and nothing is
   copied for `AFTER` triggers a table does not have. A body is read, and its SQL parsed, each time a
   trigger fires: a table with a trigger pays that per row, which the suite's two tables do not notice.
6. **A restart re-runs a body** (§5), so a notice can repeat.
7. **Wire: none. Format: none (§8). Dependencies: none.**

## 13. Progress

| slice | commit | what |
|---|---|---|
| A | `bd0f9c83`, `4a2d70e2` | this plan and ADR 0113; the ADR accepted and §6 ruled (b) |
| B1 | `ea72350d` | the reader: tokenizer, grammar, PostgreSQL's sentences for a malformed body |
| B2 + B3 | `83815c76` | the interpreter and `Statement::Do` — `FOR` and `EXECUTE` landed with it rather than after; the templates removed; `pg19_plpgsql_do.txt` |
| B4 | `5cae182d` | `format()`, widths included; `pg19_format.txt` |
| B6 | `b5f47544` | the `pg_constraint.convalidated` write, and the census block with its schema predicate left out; B landed in main as `fc8333c8` |
| C | — | row triggers: `BEFORE` and `AFTER` on every event, `NEW.*`, `ENABLE`/`DISABLE TRIGGER ALL`, `USER` or a name, a foreign key's actions firing the child's triggers, the §7 refusals; `pg19_plpgsql_trigger.txt`, and `pg19_trigger_function.txt`'s abort contained |
| B5 | — | `regnamespace`: ruled (a), in an ADR of its own, 0115 — after C |
