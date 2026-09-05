# ADR 0071 — A relation name is keyed by its schema

* Status: **proposed** — needs the human's ruling, because it changes an on-disk key shape.
* Date: 2026-09-04
* Numbered 0071 at HEAD `c37c1c58`; 0070 is b4's operator-class ADR.

## Context

A relation's name lives at `name_key(tenant, name)` = `'m' ++ "sql" ++ 'n' ++ tenant ++ name`, with
the name as the whole tail of the key. There is no schema in it. Twenty-five call sites read or
write that key.

So **two relations with the same name in different schemas are the same key**, and the second
`CREATE TABLE` answers `relation "…" already exists`. That is one missing thing behind two rows of
run 70's ranking and 9 of their 13 tests:

* `schema_authorization_test.rb` (6) — `CREATE SCHEMA AUTHORIZATION u`, then `SET SESSION
  AUTHORIZATION u`, then an unqualified `CREATE TABLE` that must land in `u`'s schema and be
  **invisible** from outside it. `test_schema_invisible` asserts the `SELECT` raises.
* `schema_test.rb`'s `DefaultsUsingMultipleSchemasAndDomainTest` (3) — a table `defaults` created in
  `schema_1` while `public.defaults` already exists.

Neither is about privileges. Both are about a schema being a **namespace**.

## Decision

### 1. The key gains the schema, and the schema is length-prefixed

`name_key(tenant, schema, name)` = `'m' ++ "sql" ++ 'n' ++ tenant ++ varint(len schema) ++ schema ++ name`.

The **schema** is length-prefixed and the **name** stays the whole tail. That asymmetry is the
point: the existing key relies on the name being last so that no name can be a prefix of another,
and inserting an unprefixed segment in front of it would break exactly that property — `s` + `chema.t`
and `sc` + `hema.t` would be one key.

### 2. Old keys are read, never rewritten in place

A `v33` cluster's name records are at `(tenant, name)` and mean `(tenant, "public", name)`, because
`public` is the only schema those clusters could put a relation in. A lookup for schema `public`
that misses the new key **falls back to the old one**; a write always uses the new shape, so a
relation rewrites itself into the new key space the first time it is altered.

No migration step, no downtime, and no scan of a catalog that may be large. The cost is one extra
`get` per miss on `public`, which is the path a `CREATE TABLE` takes anyway.

### 3. This is a key change, not a record-format change

The version byte (2 … 33) is on the record **value**, and this ADR does not change any value: the
schema is recoverable from the key, so `name_of` returns `(schema, name)` and nothing in the body
moves. **`CATALOG_FORMAT_VERSION` therefore stays at 33** unless §4 forces a value change.

*Claimed anyway, out loud, so no other lane takes it while this is open: if a value change turns out
to be needed, this ADR takes **34**.*

### 4. What is deliberately not decided here

**Whether `pg_class` needs a stored namespace oid.** Today the name scan *is* `pg_class`; with the
schema in the key it still is, and the oid can be derived from the schema name. If a stored oid is
needed for `pg_namespace` joins to be stable across renames, that is a value change and takes
version 34. Measured before it is written, not assumed.

**Search-path resolution** rides on top and is a separate, smaller change: resolving an unqualified
name means trying each schema in `search_path` in order. `$user` resolves to the session
authorization, which needs roles — the subject of this lane's other row, and the reason these two
were merged into one unit.

**No privilege enforcement.** A schema records an owner; nothing checks it. `GRANT` would be
accepted and readable back and would restrain nothing. That is a declared divergence and must be
said out loud, because a catalog that records grants nobody enforces is a security-shaped feature
that is not one.

## Consequences

* Twenty-five call sites gain a schema argument; every one of them has to decide *which* schema,
  which is where the bugs will be. The compiler finds the sites; it does not find the wrong answer.
* A name record now sorts by schema first, so "every relation in a tenant" becomes "every relation
  in a schema" plus a scan per schema — `pg_class` over all schemas is a scan of the whole `'n'`
  range, which is what it already is.
* The fallback in §2 is a permanent read path, not a temporary one, until a cluster has rewritten
  every relation. It is one `get`, and it is only taken on a miss in `public`.
* Rolling back to a `v33` binary after this lands leaves relations created in `public` under the new
  key invisible to the old code. Stated rather than solved: this project has no downgrade story and
  should not grow one by accident.
