# ADR 0071 — A relation name is keyed by its schema — **withdrawn**

* Status: **withdrawn**, the same day it was proposed. The change it proposed is not needed, because
  the thing it said was missing is already here. Kept rather than deleted so the next reader who
  looks at `name_key` and sees no schema in it finds this instead of re-proposing it.
* Date: 2026-09-04
* Numbered 0071 at HEAD `c37c1c58`; 0070 is b4's operator-class ADR.
* `CATALOG_FORMAT_VERSION` **34 is released** — this ADR claimed it and no longer needs it. Main
  stays at 33.

## What was proposed, and why it looked right

`name_key(tenant, name)` is `'m' ++ "sql" ++ 'n' ++ tenant ++ name`, with the name as the whole tail
and **no schema segment anywhere**, across twenty-five call sites. Reading only that function, two
relations of the same name in different schemas are plainly the same key — which would explain
`relation "defaults" already exists` in `DefaultsUsingMultipleSchemasAndDomainTest`, and would sit
under `schema_authorization_test.rb`'s per-user schemas as one shared cause. So this ADR proposed
putting the schema into the key, length-prefixed, with a read-time fallback for `v33` clusters.

## Why it is wrong

**The schema is already in the stored name, as a dotted string, and the key needs no segment for
it.** Two functions in `exec/mod.rs` say so, and both are documented as measured against PG19:

* `resolve_unqualified` walks the resolution path and returns `qualify(schema, name)` for the first
  schema that has the relation, so a non-`public` relation is stored as `schema.name`.
* `creation_schema` picks where an unqualified `CREATE` lands: "with `sp_a, sp_b` a `CREATE TABLE
  made_here` lands in `sp_a`, and with `nosuchschema, sp_b` it still succeeds — the first entry that
  *resolves*".

`public` is stored bare, which is why `require_sequence` has to say "the name as written, not as it
would be stored". So `public.defaults` is the key `defaults`, `schema_1.defaults` is the key
`schema_1.defaults`, and they do not collide. The premise was an artefact of reading the key
function and not the layer above it.

## What this leaves

**Row 1 (`role "…" does not exist`, 6 tests) stands and is unaffected** — it is measured, its oracle
is captured, and two refusal sites in this tree already name the gap (`lower.rs:467` "there are no
roles here", `lower.rs:1155` "the file still needs `CREATE USER` to pass"). It is a roles catalog,
not a schema change, and it was never blocked on this.

**Row 2A (`relation "defaults" already exists`, 3 tests) is unexplained again**, and the merged
"one unit" routing that came from this ADR should be undone. What is *not* the cause: a shared key
between schemas. Candidates worth a capture rather than a guess — the file's setup drops and
recreates `schema_1` per test and creates three **domains that shadow built-in type names**
(`schema_1.text`, `schema_1.varchar`, `schema_1.bpchar`) before the table, and `drop_schema` here
already handles types-as-dependents with a comment about a domain that "survived with a record key
naming a schema that was gone". That is a near neighbour of this failure and the first place to
look, but it is a hypothesis and this lane has paid for those before.

## The lesson worth keeping

A storage key is not the whole of a namespace. The evidence for "schemas are not namespaced here"
came from the key function; the evidence against it was two doc comments one layer up, both saying
*measured*. **Read the layer that calls the thing before concluding the thing is missing.**
