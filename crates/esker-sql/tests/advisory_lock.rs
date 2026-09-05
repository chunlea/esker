//! Advisory locks — **99 tests over 8 files, and they are how the suite starts rather than a
//! feature it uses.**
//!
//! `ActiveRecord::Migrator` wraps every migration in `with_advisory_lock` (`migration.rb:1610`),
//! so a node that cannot answer `pg_try_advisory_lock` fails at the first migration of a file and
//! takes the file with it. The adapter sends exactly two shapes and never the blocking form.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a property of the session.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **`pg_locks` is a relation this node does not have**, so it is `42P01` — the same answer
        // it gives for every other relation it has never heard of, and not a special case. The
        // lock *semantics* are all here and tested; what is missing is the view that reports them,
        // and it is missing for a structural reason worth naming: `CatalogView::rows_of` takes a
        // transaction and a tenant, because every other view in this node is a read of the store.
        // These rows are **session state** and are in no transaction at all, so the view machinery
        // has to learn to carry a second source before this one can exist. One of the 99 tests
        // reads it (`connection_test.rb:203`); the other 98 are migrations that need only the two
        // functions above, which is why this is a divergence rather than a blocker.
        (
            "SELECT 'r', locktype, classid, objid, objsubid, mode, granted FROM pg_locks WHERE \
             locktype = 'advisory'",
            "`42P01`: this node has no `pg_locks`. The lock is held — the line before this one \
             says so — and there is nothing here that reports it.",
        ),
        (
            "SELECT 'r', (classid::bigint << 32) | objid::bigint AS lock_id FROM pg_locks WHERE \
             locktype = 'advisory'",
            "The same. `advisory::Key` packs and splits on exactly this expression and its own \
             test pins it against the same literal the suite uses, so what is untested here is \
             the view and not the arithmetic.",
        ),
        (
            "SELECT 'r', count(*) FROM pg_locks WHERE locktype = 'advisory'",
            "The same, four times over — the corpus counts the locks after each step.",
        ),
        (
            "SELECT 'r', classid, objid, objsubid FROM pg_locks WHERE locktype = 'advisory'",
            "The same. `objsubid` 1 against 2 is what tells the two key spaces apart, and \
             `advisory::Space` carries it for when the view exists.",
        ),
        (
            "SELECT 'r', objsubid FROM pg_locks WHERE locktype = 'advisory' ORDER BY objsubid",
            "The same.",
        ),
        (
            "SELECT 'r', mode FROM pg_locks WHERE locktype = 'advisory'",
            "The same.",
        ),
        (
            "SELECT 'r', objid FROM pg_locks WHERE locktype = 'advisory' ORDER BY objid",
            "The same — and this is the line that proves a session lock survives `ROLLBACK`, \
             which `a_session_lock_outlives_the_transaction_that_took_it` asserts instead.",
        ),
        // **`pg_advisory_unlock_all()` needs a `void`**, which this node has no type for: every
        // function here answers a value. `ActiveRecord` never calls it — the migrator unlocks the
        // one key it took — so it is refused by name rather than given a type it would be the only
        // user of. `Executor::release_advisory_locks` does the same job where it actually matters,
        // at the end of a session.
        (
            "SELECT 'r', pg_advisory_unlock_all() IS NULL",
            "`0A000` naming the function: it returns `void` and this node has no such type. The \
             session-end release it exists for is done by the connection instead.",
        ),
        (
            "SELECT 'r', pg_typeof(pg_advisory_unlock_all())::text",
            "The same refusal, and the line that says why: the answer is `void`.",
        ),
    ],
};

#[test]
fn every_advisory_lock_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_advisory_lock.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **A session lock outlives the transaction that took it**, which is the rule the corpus can only
/// show through `pg_locks` and this node has to show directly.
///
/// Measured on the oracle: `pg_try_advisory_lock(301)` issued inside `BEGIN` is still held after
/// `ROLLBACK`. Nothing else in this node behaves that way — every other thing a statement changes
/// is in the transaction's write set — so an implementation that put the lock there would look
/// right until the first migration that rolled back.
#[test]
fn a_session_lock_outlives_the_transaction_that_took_it() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    assert_eq!(
        node.rows("SELECT pg_try_advisory_lock(301)"),
        [["t"]],
        "taken inside the block"
    );
    node.run("ROLLBACK").unwrap();

    // Still held: releasing it answers `t` **once**, and the second release is the `f` that says
    // the rollback did not take it.
    assert_eq!(node.rows("SELECT pg_advisory_unlock(301)"), [["t"]]);
    assert_eq!(node.rows("SELECT pg_advisory_unlock(301)"), [["f"]]);
}

/// **Two sessions on one node**, which is the whole point of the feature and what a corpus of one
/// session cannot reach.
///
/// The migrator takes the lock so that a second migrator waits; a node where each session had its
/// own table would answer `true` to both and let two migrations run at once — the exact failure
/// the lock exists to prevent, and one that no single-session test can see.
#[test]
fn a_second_session_cannot_take_a_lock_the_first_holds() {
    use std::sync::Arc;

    use esker_sql::advisory::Locks;
    use esker_sql::backend::{Backend, MemoryBackend};
    use esker_sql::catalog::Catalog;
    use esker_sql::exec::Executor;
    use esker_sql::parse::parse_statements;
    use esker_sql::pgwire::session::{Execute, Outcome, Params};

    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn Backend>;
    let catalog = Arc::new(Catalog::new());
    let locks = Arc::new(Locks::new());
    let session = || {
        Executor::new(
            Arc::clone(&backend),
            Arc::clone(&catalog),
            1,
            esker_sql::session::register(),
        )
        .sharing_advisory_locks(Arc::clone(&locks))
    };
    let (mut a, mut b) = (session(), session());

    let answer = |executor: &mut Executor, sql: &str| -> String {
        let parsed = parse_statements(sql).unwrap().pop().unwrap();
        match executor.execute(&parsed, &Params::NONE).unwrap() {
            Outcome::Rows { rows, .. } => String::from_utf8(rows[0][0].clone().unwrap()).unwrap(),
            Outcome::Done { .. } => "-".to_owned(),
        }
    };

    assert_eq!(answer(&mut a, "SELECT pg_try_advisory_lock(7)"), "t");
    assert_eq!(
        answer(&mut b, "SELECT pg_try_advisory_lock(7)"),
        "f",
        "a second session must not get a lock the first holds"
    );
    // Nor may it release one it does not hold, which would be worse than failing to take it.
    assert_eq!(answer(&mut b, "SELECT pg_advisory_unlock(7)"), "f");
    assert_eq!(
        answer(&mut a, "SELECT pg_try_advisory_lock(7)"),
        "t",
        "and the holder is not blocked by itself"
    );

    // Shared locks agree with each other and not with an exclusive one.
    assert_eq!(answer(&mut a, "SELECT pg_try_advisory_lock_shared(9)"), "t");
    assert_eq!(answer(&mut b, "SELECT pg_try_advisory_lock_shared(9)"), "t");
    assert_eq!(answer(&mut b, "SELECT pg_try_advisory_lock(9)"), "f");

    // The session ending releases what it held, which is the other half of the lifetime.
    a.release_advisory_locks();
    assert_eq!(
        answer(&mut b, "SELECT pg_try_advisory_lock(7)"),
        "t",
        "the first session's locks go when it does"
    );
}
