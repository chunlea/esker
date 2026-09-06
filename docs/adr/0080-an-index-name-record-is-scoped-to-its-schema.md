# 0080 — An index's name record is scoped to its schema

*Status: accepted (2026-09-05, by the user). Superseded parts: none.*

## Context

A relation's name record is the catalog's uniqueness index: `name_key(tenant, name)` holds what the
name resolves to, and a `CREATE` that finds one is `42P07`. A **table's** key is its qualified
name — `schema ++ NUL ++ name` outside `public` ([ADR 0071](0071-a-relation-name-is-keyed-by-its-schema.md)).
An **index's** was not: `write_table`, `replace_table` and `drop_table` all keyed an index's and a
primary key's record on `IndexDef::name` alone.

So index names shared one namespace across every schema in a tenant, and a name a user gave —
`CREATE INDEX shared_ix ON s.t (a)` where `public.shared_ix` exists — was refused as a duplicate
where a real server creates it.

`SchemaWithDotsTest` is where the suite meets it. With `search_path` set to a schema called
`my.schema`, `ALTER INDEX "posts_pkey" RENAME TO "articles_pkey"` collided with
`public.articles_pkey`, a fixture that is always present, and the failure cascaded into every later
test of the file as `25P02`. The harness lane's side-by-side against PostgreSQL 19 named the
statement; run as a minimal pair, the dot turned out to be irrelevant — the same collision happens
in an ordinary schema, and the dotted name is only how the suite reached it.

## Decision

**Index name records are keyed by schema, and old databases are disposable.**

* Every writer builds the key with `catalog::owned_name(table, name)` — the index's own bare name in
  its table's schema. Seven sites: two in `write_table`, three in `replace_table`, two in
  `drop_table`.
* Every reader resolves through the `search_path` exactly as a relation name does. Most already did,
  through `Executor::resolve_unqualified`; `ALTER INDEX … RENAME TO`'s duplicate check did not, and
  asked `public` for a bare target.
* **`IndexDef::name` stays bare-or-as-derived and no renderer changes.** `pg_class.relname` comes
  from the *key*, split by the reader, so scoping the key is what puts an index in a schema.
* **No fallback read of the flat key, and no migration.** A database written before this is refused
  rather than misread — see *Consequences*, where that half is still owed.

The alternatives, and why not: a fallback read (a qualified miss falls back to the flat key) is
permanent, and a flat key is indistinguishable from a legitimate `public` one, so the fallback would
answer for the wrong index for ever. A migration is code that runs once, against data nobody needs.

## Consequences

* Two indexes of one name may exist in two schemas, which is what PostgreSQL does.
* `SchemaWithDotsTest#test_rename_table`'s head error is gone.
* **A derived name is stored qualified and a user's is stored bare.** `plan::make_object_name`
  spends its 63-byte budget on the identifier and re-qualifies, so `s.t`'s primary key is stored
  `s\0t_pkey`. `owned_name` is therefore idempotent — a name that already carries a separator is
  used as it is — and `ALTER INDEX … RENAME` compares **bare to bare**. Making `IndexDef::name`
  uniformly bare would be tidier and is four derivation sites' worth of change; it is not done here.
* **The format bump this decision asked for is NOT in the commit that carries the rest**, and the
  reason is a finding rather than a shortfall. Raising `OLDEST_TABLE_VERSION` is the mechanism that
  refuses old bytes with a clear sentence — `catalog format version 36 is not 37..=37` — but that
  constant is the floor for `Reader::new`, which **every record kind without a floor of its own
  uses**. Raising it to 37 also refuses version-14 *sequence* records, whose layout this decision
  does not touch, and inverts the claim of nine `a_version_N_table_record_still_decodes` goldens.
  Refusing an old database is right; doing it by raising a shared record floor is not obviously the
  way, and the alternative — a database-level marker checked once at open — is a design question
  rather than a lane's improvisation. Sized and handed back: nine goldens whose claim inverts, one
  sequence test that should not be affected at all, and the choice of mechanism.
* Until that lands, **an old database is misread rather than refused**: its flat index keys are
  invisible to a qualified reader, so an index would not be found by `DROP INDEX` and its name
  record would outlive its object.
