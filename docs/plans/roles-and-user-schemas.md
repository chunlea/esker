# Row 1 — roles, and `$user` on top of the schema namespace

One unit (coordinator, 2026-09-04). Closes `schema_authorization_test.rb`'s **6 errors**, the whole
of run 70's `role "…" does not exist` row. Written before any code; red-first when the gates reopen.

## Why it is one unit

The file's six tests never mention privileges. They create a user, create a schema **authorized to**
that user, `SET SESSION AUTHORIZATION` to it, and then create an unqualified table that must land in
that user's schema and be **invisible** from outside it. Roles are the missing noun; `$user` is the
one line of resolution that makes the schema they own reachable. Neither half is worth a round trip
on its own.

**Schemas are already a namespace** — `resolve_unqualified` stores `schema.name`, `creation_schema`
picks the schema an unqualified `CREATE` lands in. So `$user` is genuinely small: one more entry
kind in the resolution path. That was ADR 0071's withdrawal and it is what makes this unit sized the
way it is.

## Scope

1. **A role record** — a new catalog record kind: name, oid, and the seven flags `pg_roles` shows.
2. **`CREATE USER` / `CREATE ROLE`, `DROP USER` / `DROP ROLE`** — both spellings, one implementation.
3. **`pg_roles` and `pg_authid`** as catalog views.
4. **`CREATE SCHEMA AUTHORIZATION u`** — unblock `lower.rs:467`; the schema is **named `u`** and
   owned by `u`.
5. **`SET SESSION AUTHORIZATION u`** — validate against the catalog instead of always refusing
   (`lower.rs:1155`), and move `current_user` / `session_user`.
6. **`$user` in `search_path`** resolves to the session authorization.

## The oracle, already captured

One rolled-back PG19 session, nothing left behind. Full table in
`h1-row1-roles-measurement.md`; the three that a reasonable implementation gets wrong:

* `CREATE USER` tags **`CREATE ROLE`**, and **implies `LOGIN`** — `rolcanlogin=t`. `CREATE ROLE`
  does not. Otherwise the two statements are identical, so symmetry is the trap.
* `CREATE SCHEMA AUTHORIZATION u` with **no schema name** creates a schema *named* `u`.
* After `RESET SESSION AUTHORIZATION` the table created under `u` is
  `relation "…" does not exist` — invisible, not merely unowned. That is `test_schema_invisible`,
  and it is the assertion that proves `$user` resolved rather than being skipped.

## Test list

| test | asserts |
|---|---|
| `create_user_records_a_role_that_pg_roles_shows` | the row, all seven flags, `rolcanlogin` **true** |
| `create_role_does_not_imply_login` | the same, `rolcanlogin` **false** — the pair is the point |
| `a_duplicate_role_is_refused_by_name` | `role "u" already exists` |
| `drop_user_removes_it_and_a_missing_one_is_refused` | both directions |
| `create_schema_authorization_names_the_schema_after_the_role` | schema `u`, owner `u` |
| `set_session_authorization_moves_current_user` | `current_user` and `session_user` both |
| `set_session_authorization_to_a_missing_role_is_22023` | unchanged from today; a guard against regressing it |
| `an_unqualified_create_lands_in_the_session_users_schema` | `$user` resolved |
| `a_table_in_a_user_schema_is_invisible_once_authorization_is_reset` | the `test_schema_invisible` shape |
| real-store sibling of the last two | when the gates reopen |

## Risks

* **`$user` is currently *skipped*** in path resolution (`parameter.rs`), which is what makes the
  boot default `"$user", public` behave. Making it resolve changes the default path's meaning for
  every session, not just these tests — so the boot behaviour needs a test of its own, and a session
  whose authorization names a schema that does not exist must still resolve `public` and not fail.
* **`pg_roles` is a view over a record kind that does not exist yet**, so the reserved relation id
  needs claiming out loud — the last one collided with another lane.
* The oracle capture was taken as a superuser. `SET SESSION AUTHORIZATION` to another role is a
  superuser-only operation on PG; this node has no privilege model, which is fine and is the
  divergence below.

## What I will NOT do

* **No privilege enforcement, and no `GRANT`.** The six tests need neither, and a catalog that
  records grants nobody honours is a security-shaped feature that is not one. Stated in the code as
  a declared divergence, not left for a reader to discover.
* **No `OWNER TO`, no `ALTER ROLE`, no role membership.** The brief named the first; the file does
  not send it.
* **No password storage.** `rolpassword` reads NULL, which is what the oracle shows for a role
  created without one, and is the only value this node can honestly report.
