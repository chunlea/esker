# 0052 — A database is a tenant, and the directory that names them

## Context

`docs/plans/phase-9-rails.md` measures this node against ActiveRecord's own suite (ADR 0031), and
run 46's seventh row is **103 tests across three files** — `counter_cache_test.rb` (56),
`associations_test.rb` (46), `fixtures_test.rb` (1) — every one of them failing with the same
sentence:

    ActiveRecord::Fixture::FixtureError: table "dogs" has no columns named
    "trainer_id", "dog_lover_id".

**There is no server error underneath it.** `ActiveRecord` raises that itself, after reading the
table's columns and finding the fixture's keys missing, and the introspection it read was answered
correctly: the table really does lack the columns. The cause is two lines of the suite's own schema:

* `schema.rb:578` — `create_table :dogs` with `trainer_id, breeder_id, dog_lover_id, alias`, on the
  default connection (`arunit`).
* `schema.rb:1496`, the **last line of the file** — `OtherDog.lease_connection.create_table :dogs,
  force: true`, a bare `id`-only table. `OtherDog < ARUnit2Model`, so on a real server that runs
  against the **`arunit2` database** and creates a second, independent `dogs`.

The harness points `arunit` and `arunit2` at the same listener because this node has one namespace
and ignores the name. So the second `create_table … force: true` does not create a second table —
`force: true` is `DROP TABLE IF EXISTS` followed by `CREATE TABLE`, it does not merge — and it
**drops and recreates the first one**, five live columns down to one. Measured on a real
PostgreSQL, in one database, in `tests/corpus/pg19_two_database_dogs.txt`: `pg_attribute` goes
5 → 1 and the introspection filters `attisdropped`, so there is no trace of the first definition
left for anything to find.

Those 103 tests cannot pass until the node has more than one database, whatever any other lane
does. They are the largest single block on the scoreboard that is not a type.

What exists today is one database and a comment saying so:

* `bin/esker-sql.rs`: `const TENANT: u64 = 1` — *"the tenant every connection is served as, until
  there is a way to say otherwise"*.
* `pgwire::server`'s `complete_startup` reads the startup packet's `user` and **discards its
  `database`**.
* `parse::lower`'s `DATABASE_NAME = "esker"` is what `current_database()` answers and what the
  single hard-coded `pg_database` row carries, deliberately the same constant so that
  `WHERE datname = current_database()` matches by construction.

## Decision

**A database is a tenant id.** There is no new level in the key space, and `esker-keys` does not
change.

Every SQL key already carries a tenant, and it is the *first* field after the namespace byte in
both halves of the layout (`esker_keys::prefix`, `catalog::record`):

```text
't' ++ tenant:u64 ++ table_id:u64 ++ 'r' ++ row_id       rows
't' ++ tenant:u64 ++ table_id:u64 ++ 'i' ++ index_id …   indexes
'm' ++ "sql" ++ kind ++ tenant:u64 ++ …                  every catalog record, the id
                                                         sequence, the name index, a sequence's
                                                         value, a schema-change job, a flashback
```

So the isolation a database needs is already a property of the encoding rather than a check
somebody has to remember to write. Three things follow from that and they are the argument:

1. **No table of one database can be named from another**, because the name index is
   `'n' ++ tenant ++ name` and a lookup in the wrong tenant reads a different key. PostgreSQL's own
   rule is that a connection sees exactly one database and *cannot* query across, so the constraint
   the encoding imposes is the constraint the semantics want. A design where cross-database access
   were possible would have to add the refusal back.
2. **`DROP DATABASE` is a bounded sweep** — the tenant's slice of `'t'`, and its slice under every
   `'m' ++ "sql"` kind byte — and there is no third place a row can hide. It is every byte rather
   than a list of the kinds that take a tenant, because a list is a thing to forget: the next
   record kind added to `catalog::record` would leak its tenant's rows on every drop and nothing
   would say so.
3. **The id sequence is already per tenant** (`'m' ++ "sql" ++ 's' ++ tenant`), so two databases
   cannot collide on a relation id, and each starts counting from its own beginning the way a real
   database does.

### The two designs this rejects, and why

**A database as a name qualification, the way a schema is.** Schemas here are not a key-space
level: `catalog::qualify` folds `schema.name` into one string and stores it in the tenant's name
index. Doing the same for a database would be the cheapest change and it is the wrong one — two
databases would share one id sequence and one key range, so a scan of one database's tables visits
the other's rows, `DROP DATABASE` becomes a walk over names rather than a range delete, and the
isolation would be a convention rather than a property. A schema is qualifiable *because*
PostgreSQL lets one statement name two schemas; a database is not, and encoding it the same way
would make the illegal thing expressible.

**A new level between tenant and table** — `'t' ++ tenant ++ db ++ table_id`. Rejected because it
is an on-disk format change with golden tests behind it (`CLAUDE.md`: ask before changing one), and
it buys a second axis that nothing asks for. A tenant that is not a database has no user today: the
one caller sets it to `1` and the comment beside it says it is waiting for exactly this. Adding a
level to keep the word "tenant" free for a feature nobody has specified is paying a format change
for a name.

### The one thing a tenant cannot hold: the directory

`pg_database` must answer from **every** database — `ActiveRecord`'s adapter opens a connection and
immediately runs `SELECT current_database()` and three joins against `pg_database` for the
encoding, the collation and the ctype (`postgresql/schema_statements.rb:230-258`, measured in
`tests/corpus/pg19_coalesce_current_database.txt`) — and `CREATE DATABASE` has to check a name that
belongs to no database in particular. So the directory is the one piece of state that cannot be
tenant-scoped, and it is **two** new record kinds in `esker-sql`'s own metadata space, which
`catalog::record` owns end to end:

```text
'm' ++ "sql" ++ 'D' ++ name       a database: the name, and the tenant it is
'm' ++ "sql" ++ 'C'               the next database id, one counter for the cluster
```

**The name is the key and the id is the body**, rather than a record keyed by id with an index over
it. Every question asked of the directory goes that way: startup looks a name up, `DROP DATABASE`
looks a name up, and `pg_database` scans — where the key gives the `datname` and the body the
`oid`. A second record keyed by id would be a second thing to keep consistent in exchange for a
lookup nothing performs.

`esker_keys::prefix::meta_key` takes an arbitrary suffix and the `"sql"` kind bytes are constants in
`catalog::record`, so none of this reaches `esker-keys`: the reserved layout in `docs/DESIGN.md` §3
is unchanged, and its four namespace bytes still mean what they meant.

One property is inherited rather than re-argued: the name is the **whole tail** of its key, so it
needs no length and cannot be confused with one it is a prefix of — what `name_key` and the
checkpoint keys already rely on, and what makes `ar`, `arunit` and `arunit2` three databases.

**The database id is the tenant id is the `pg_database` oid.** One number, so there is no mapping
to keep consistent and no way for two of them to disagree.

### A cluster with no directory has the databases it is already serving

An existing cluster has a tenant `1` full of rows and no `'D'` record. The directory is therefore
**seeded rather than migrated**: a read that finds nothing answers with the database this node
reported before it could name a second — id `1`, named `DATABASE_NAME` — so an upgrade needs no
step, and the constant that had been standing in for the directory becomes its default rather than
being deleted. `create_database` writes the seed beside the first row anybody types, because
otherwise the directory would stop being empty and the database the cluster was already serving
would vanish from `pg_database`: a row disappearing because a *different* row was added.

**`template0` and `template1` are seeded with it**, at ids 2 and 3. They arrived with
`CREATE DATABASE`'s option list rather than with this decision, and they belong here because they
are part of what the directory answers: without them `TEMPLATE = template0` — the spelling every
writer of that clause uses, and the one PostgreSQL's own `HINT` points at — would name nothing, and
a node claiming PostgreSQL's shape would be missing the two rows every cluster has. Both are empty,
which is what makes copying one exact on a node that cannot copy a database's contents. Dropping
one is `42809 cannot drop a template database`.

### What the startup packet selects, and what a wrong name costs

The `database` parameter names the tenant the session runs as. A name the directory does not have
is `3D000 database "x" does not exist`, at startup, which is what a real server answers and what
`rake db:create` depends on to know it must create one.

## Consequences

- **103 tests become reachable**, and they are reachable by the harness pointing `arunit2` at a
  second database rather than by any change to the tests. The exclusion list that ADR 0031 requires
  loses its first three candidates instead of gaining them.
- **`CREATE DATABASE` cannot be captured the way everything else in this phase is.** It is
  `25001 CREATE DATABASE cannot run inside a transaction block` on a real server — measured — so
  the corpus convention of `BEGIN … ROLLBACK` around a whole capture cannot cover it, and its
  capture has to create and drop a real database by name and prove it left nothing behind.
- The same `25001` is a rule here and not only a limitation: creating a database writes the
  directory outside the session's transaction, so a `CREATE DATABASE` inside a block would be a
  write that a `ROLLBACK` could not take back.
- **`DROP DATABASE` empties the tenant, and it costs a delete per key.** PostgreSQL unlinks a
  directory and is O(1); here the rows share one key space, so emptying one is a scan. Dropping the
  directory row alone would be cheap and would leave the user's rows on disk for ever — ids never
  repeat, so nothing would read them again and nothing would reclaim them either — and a statement
  that says it deleted a database and did not is the worse of the two. A database too large for one
  transaction fails loudly rather than half-emptying, and reclaiming in the background, as a job of
  the kind `'m' ++ "sql" ++ 'j'` already records, is the follow-on.
- **The refusal `DROP DATABASE` can make is only about *this* session.** PostgreSQL refuses to drop
  a database any session is connected to; that needs a registry this node does not have, so what is
  enforced is `55006` for the one the current session is serving and nothing for the rest.
- **PostgreSQL's option list is a `42601` before it can be a `0A000`**, because `sqlparser`
  0.62.0's `CREATE DATABASE` grammar carries `LOCATION`, `MANAGEDLOCATION`, `CLONE` and `MySQL`'s
  `CHARACTER SET`/`COLLATE` and nothing PostgreSQL spells — `ENCODING`, which is what
  `rake db:create` sends, exists in that crate only as a `COPY` option. That is contract C1's
  shortfall and it is closed the way this module closes one: the list is cut out of the source
  before the parse and carried beside the tree. What sorts the options once they are read is **not
  whether this node implements one but whether a client could tell that it had not** —
  `ENCODING`/`LC_COLLATE`/`LC_CTYPE`/`LOCALE` are checked against the one encoding and the one
  collation there are and **not recorded**, since a record of a constant is a place for two answers
  to disagree; `OWNER` and `TABLESPACE` get PostgreSQL's own `42704` for a name that is not there,
  which here is true of every name; and `CONNECTION LIMIT`, `ALLOW_CONNECTIONS` and `IS_TEMPLATE`
  are refused unless they ask for the default, because each is a promise a client can check.
- Anything that reads the tenant from a constant becomes a session property: the re-driver and the
  columnar asserter in `bin/esker-sql.rs` each take `TENANT` today and each has to say *which*
  database it is working on, or work on all of them.
- **`TEMPLATE` is honoured only where there is nothing to copy.** `template0` and `template1` are
  seeded, so the clause names something; a template holding any relation is `0A000`, because an
  empty copy of a full database is a wrong answer wearing a success. Copying a database's contents
  is a second unit and this one does not pretend to it.
- **`CONNECTION LIMIT` and refusing to drop a database another session holds want the same thing**
  — a registry of live sessions — which is why they are one debt rather than two.
