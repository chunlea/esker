//! Roles, and `$user` on top of the schema namespace.
//!
//! `schema_authorization_test.rb`'s six errors, whose row in the ranking reads `role "…" does not
//! exist`. The row names the first domino, not the work: the file creates a user, creates a schema
//! **authorized to** that user, sets session authorization to it, and then creates an *unqualified*
//! table that must land in that user's schema and be **invisible** from outside it. `CREATE USER`
//! is `rescue nil`'d there, so its refusal is silent and `SET SESSION AUTHORIZATION` reports the
//! missing role.
//!
//! **No privileges.** Nothing here enforces ownership and `GRANT` is not implemented; the six tests
//! need neither, and a catalog that recorded grants nobody honours would be a security-shaped
//! feature that is not one (`docs/plans/roles-and-user-schemas.md`).
//!
//! Every expectation was measured against PG19 in one rolled-back session; the three a reasonable
//! implementation gets wrong are marked below.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use parity::Pair;

/// **`CREATE USER` implies `LOGIN`.** The trap: `CREATE ROLE` is otherwise the same statement and
/// does not, so an implementation that treats them as synonyms passes one of these and fails the
/// other. Measured.
#[test]
fn create_user_records_a_role_that_pg_roles_shows() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER alice").unwrap();
    assert_eq!(
        session.rows(
            "SELECT rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, \
             rolconnlimit FROM pg_roles WHERE rolname = 'alice'"
        ),
        [[
            "alice".to_owned(),
            "f".to_owned(),
            "t".to_owned(),
            "f".to_owned(),
            "f".to_owned(),
            "t".to_owned(),
            "-1".to_owned()
        ]]
    );
}

/// **`SUPERUSER` is recorded**, which it was not: the statement was accepted and the attribute
/// dropped, so a client that asked for it was told yes and then shown `f` (run 87).
///
/// Recorded, not enforced — nothing here consults it. That is a declared divergence; reporting the
/// opposite of what was asked is not.
#[test]
fn create_user_records_the_attributes_it_was_given() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session
        .run("CREATE USER root SUPERUSER CREATEDB CREATEROLE")
        .unwrap();
    assert_eq!(
        session.rows(
            "SELECT rolsuper, rolcreatedb, rolcreaterole, rolcanlogin FROM pg_roles \
             WHERE rolname = 'root'"
        ),
        [[
            "t".to_owned(),
            "t".to_owned(),
            "t".to_owned(),
            "t".to_owned()
        ]]
    );
}

/// The other half of the pair, and the reason the pair exists.
#[test]
fn create_role_does_not_imply_login() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE ROLE bob").unwrap();
    assert_eq!(
        session.rows("SELECT rolcanlogin FROM pg_roles WHERE rolname = 'bob'"),
        [["f".to_owned()]],
        "CREATE ROLE does not imply LOGIN and CREATE USER does"
    );
}

/// `pg_authid` holds the same role with an oid and no password.
#[test]
fn pg_authid_shows_the_role_with_an_oid() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER carol").unwrap();
    assert_eq!(
        session.rows(
            "SELECT oid > 0, rolname, rolpassword IS NULL FROM pg_authid WHERE rolname = 'carol'"
        ),
        [["t".to_owned(), "carol".to_owned(), "t".to_owned()]]
    );
}

/// `role "x" already exists`, and the same sentence for both spellings.
#[test]
fn a_duplicate_role_is_refused_by_name() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER dave").unwrap();
    let refused = session
        .run("CREATE USER dave")
        .expect_err("the name is taken");
    assert_eq!(refused.to_string(), "role \"dave\" already exists");
}

/// `DROP USER` removes it; a name that is not there is refused with PostgreSQL's sentence.
#[test]
fn drop_user_removes_it_and_a_missing_one_is_refused() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER erin").unwrap();
    session.run("DROP USER erin").unwrap();
    assert!(
        session
            .rows("SELECT rolname FROM pg_roles WHERE rolname = 'erin'")
            .is_empty()
    );

    let refused = session
        .run("DROP USER nosuchrole")
        .expect_err("there is no such role");
    assert_eq!(refused.to_string(), "role \"nosuchrole\" does not exist");
}

/// **`CREATE SCHEMA AUTHORIZATION u` with no schema name creates a schema *named* `u`.** Measured
/// — the name is not optional, it is derived from the role.
///
/// **What this does not assert, and deliberately: the owner.** PG19 records `nspowner` and this
/// node does not — `pg_namespace` has `oid` and `nspname` and no owner column, and giving it one
/// would mean a schema-record format change to store an owner that nothing here reads and nothing
/// here enforces. That is the declared divergence in `docs/plans/roles-and-user-schemas.md`, and
/// the six tests this unit exists for never read `nspowner`. Recorded here rather than quietly
/// omitted, so the gap is a decision someone can reverse and not an oversight.
#[test]
fn create_schema_authorization_names_the_schema_after_the_role() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER frank").unwrap();
    session.run("CREATE SCHEMA AUTHORIZATION frank").unwrap();
    assert_eq!(
        session.rows("SELECT nspname FROM pg_namespace WHERE nspname = 'frank'"),
        [["frank".to_owned()]],
        "the schema takes the role's name"
    );
}

/// `SET SESSION AUTHORIZATION` to a role that exists moves both `current_user` and `session_user`.
#[test]
fn set_session_authorization_moves_current_user() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER grace").unwrap();
    session.run("SET SESSION AUTHORIZATION grace").unwrap();
    assert_eq!(
        session.rows("SELECT current_user, session_user"),
        [["grace".to_owned(), "grace".to_owned()]]
    );
}

/// **The guard, which passes today and must keep passing**: a name that is not a role is `22023`,
/// not `42704`. PostgreSQL reads an authorization name as a parameter value rather than an object,
/// and this lane measured that before roles existed at all.
#[test]
fn set_session_authorization_to_a_missing_role_is_still_22023() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    let refused = session
        .run("SET SESSION AUTHORIZATION nobody")
        .expect_err("there is no such role");
    assert_eq!(refused.sqlstate(), "22023", "{refused}");
    assert_eq!(refused.to_string(), "role \"nobody\" does not exist");
}

/// **`$user` resolves to the session authorization**, so an unqualified `CREATE` lands in that
/// user's schema rather than in `public`. This is the half the six tests are actually about.
#[test]
fn an_unqualified_create_lands_in_the_session_users_schema() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER heidi").unwrap();
    session.run("CREATE SCHEMA AUTHORIZATION heidi").unwrap();
    session.run("SET search_path TO '$user',public").unwrap();
    session.run("SET SESSION AUTHORIZATION heidi").unwrap();
    session.run("CREATE TABLE things (id bigint)").unwrap();
    assert_eq!(
        session.rows(
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'things'"
        ),
        [["heidi".to_owned()]],
        "$user resolved to heidi, so the table is in heidi's schema"
    );
}

/// **And it is invisible once the authorization is reset** — `test_schema_invisible`, the assertion
/// the whole file is built around. Measured: the `SELECT` *raises* rather than returning no rows.
#[test]
fn a_table_in_a_user_schema_is_invisible_once_authorization_is_reset() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER ivan").unwrap();
    session.run("CREATE SCHEMA AUTHORIZATION ivan").unwrap();
    session.run("SET search_path TO '$user',public").unwrap();
    session.run("SET SESSION AUTHORIZATION ivan").unwrap();
    session.run("CREATE TABLE hidden (id bigint)").unwrap();

    session.run("RESET SESSION AUTHORIZATION").unwrap();
    let gone = session
        .run("SELECT * FROM hidden")
        .expect_err("outside ivan's authorization the name does not resolve");
    assert_eq!(gone.to_string(), "relation \"hidden\" does not exist");
}

/// **`RESET SESSION AUTHORIZATION` puts the connection's own role back.**
///
/// PostgreSQL documents it as exactly `SET SESSION AUTHORIZATION DEFAULT`, and that is how it is
/// implemented — `sqlparser` reads the second spelling and not the first, so the source is
/// rewritten before the parse. The rewrite loses nothing, which is the test of whether that
/// mechanism was the right one to use.
#[test]
fn reset_session_authorization_restores_the_login_role() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    let before = session.rows("SELECT current_user")[0][0].clone();

    session.run("CREATE USER judy").unwrap();
    session.run("SET SESSION AUTHORIZATION judy").unwrap();
    assert_eq!(session.rows("SELECT current_user"), [["judy".to_owned()]]);

    session.run("RESET SESSION AUTHORIZATION").unwrap();
    assert_eq!(session.rows("SELECT current_user"), [[before]]);
}

/// **`SET LOCAL SESSION AUTHORIZATION` does not outlive its transaction — `COMMIT`.**
///
/// A `SET LOCAL` that survived the block would be a *wrong answer* rather than a missing feature,
/// which is what ADR 0031 ranks a refusal above. It is carried on the transaction-scoped restore
/// `esker.read_as_of` already used, so this is one more field put back and not a new mechanism.
#[test]
fn set_local_session_authorization_ends_with_a_commit() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER kim").unwrap();
    let before = session.rows("SELECT current_user")[0][0].clone();

    session.run("BEGIN").unwrap();
    session.run("SET LOCAL SESSION AUTHORIZATION kim").unwrap();
    assert_eq!(session.rows("SELECT current_user"), [["kim".to_owned()]]);
    session.run("COMMIT").unwrap();

    assert_eq!(
        session.rows("SELECT current_user"),
        [[before]],
        "the block ended, so its LOCAL authorization did"
    );
}

/// The same for `ROLLBACK`, which is the other way a block ends.
#[test]
fn set_local_session_authorization_ends_with_a_rollback() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER liam").unwrap();
    let before = session.rows("SELECT current_user")[0][0].clone();

    session.run("BEGIN").unwrap();
    session.run("SET LOCAL SESSION AUTHORIZATION liam").unwrap();
    session.run("ROLLBACK").unwrap();

    assert_eq!(session.rows("SELECT current_user"), [[before]]);
}

/// **And a `ROLLBACK TO SAVEPOINT` undoes it**, as it undoes every other `SET` inside the mark.
///
/// The savepoint mark carries the authorization for the same reason it carries the parameters and
/// the read set: a rollback that put some of the session back and not the rest would be a subtler
/// wrong answer than the one this feature exists to avoid.
#[test]
fn a_rollback_to_savepoint_undoes_a_local_authorization() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER mona").unwrap();

    session.run("BEGIN").unwrap();
    let inside = session.rows("SELECT current_user")[0][0].clone();
    session.run("SAVEPOINT s1").unwrap();
    session.run("SET LOCAL SESSION AUTHORIZATION mona").unwrap();
    assert_eq!(session.rows("SELECT current_user"), [["mona".to_owned()]]);

    session.run("ROLLBACK TO SAVEPOINT s1").unwrap();
    assert_eq!(
        session.rows("SELECT current_user"),
        [[inside]],
        "the mark was taken before the SET, so the rollback undoes it"
    );
    session.run("ROLLBACK").unwrap();
}

/// **`SET LOCAL` outside a block warns and changes nothing**, which is what a real server does —
/// there is no transaction for it to be local to.
#[test]
fn set_local_session_authorization_outside_a_block_does_nothing() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER nina").unwrap();
    let before = session.rows("SELECT current_user")[0][0].clone();

    session.run("SET LOCAL SESSION AUTHORIZATION nina").unwrap();
    assert_eq!(
        session.rows("SELECT current_user"),
        [[before]],
        "there was no block, so there was nothing to be local to"
    );
}

/// **The boot default still answers `{public}`** — the safety property of making `$user` resolve.
///
/// `"$user", public` is what every session starts with, so this change alters what the *default*
/// path means for every connection on the node, not just the six tests that wanted it. A session
/// whose role has no schema of its own must be exactly as it was: `$user` names nothing and is
/// dropped, which is PostgreSQL's rule for any entry naming a missing schema.
#[test]
fn the_boot_default_path_still_resolves_to_public_alone() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    assert_eq!(
        session.rows("SELECT current_schemas(false)"),
        [["{public}".to_owned()]],
        "nobody has made a schema named after this role, so $user names nothing"
    );
}

/// **An authorization naming a schema that does not exist resolves to nothing, not to an error.**
///
/// A `search_path` entry that names no schema is *skipped* on a real server — it is not a
/// misconfiguration and not a failure — and `$user` is an entry like any other. Without this the
/// obvious implementation raises on a role that simply has no schema, which is every role until
/// somebody runs `CREATE SCHEMA AUTHORIZATION`.
#[test]
fn an_authorization_with_no_schema_of_its_own_is_not_an_error() {
    let pair = Pair::new(&[]);
    let mut session = pair.session();
    session.run("CREATE USER olive").unwrap();
    session.run("SET search_path TO '$user',public").unwrap();
    session.run("SET SESSION AUTHORIZATION olive").unwrap();

    assert_eq!(
        session.rows("SELECT current_schemas(false)"),
        [["{public}".to_owned()]],
        "olive has no schema, so $user drops and public carries the path"
    );
    // And the session still works: an unqualified create lands in public.
    session.run("CREATE TABLE still_fine (id bigint)").unwrap();
    assert_eq!(
        session.rows(
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'still_fine'"
        ),
        [["public".to_owned()]]
    );
}
