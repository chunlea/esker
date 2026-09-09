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
        // **`void` is the one thing left, and it is a type this node does not have.** Every
        // advisory function that returns `void` on a real server answers an **empty string** here:
        // the value prints the same, and `pg_advisory_unlock_all() IS NULL` is `f` on both, which
        // a NULL would have got wrong. What still differs is the name of the type, and closing it
        // means a `ColumnType::Void` — a type-surface change, which is not this unit's to make.
        (
            "SELECT 'r', pg_typeof(pg_advisory_unlock_all())::text",
            "`text`, because this node has no `void` type. The function runs and releases the \
             locks — the two lines around this one measure that — and only the type name differs.",
            "UNMEASURED",
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

/// **A session advisory lock is a row in `pg_locks`**, and `connection_test.rb`'s
/// *get and release advisory lock* reads it back by reassembling the key from two halves.
///
/// Measured on PostgreSQL 19, `pg_advisory_lock(5295901941258979200)`:
///
/// ```text
/// locktype | database | relation | page | tuple | virtualxid | transactionid |  classid   |   objid    | objsubid |     mode      | granted | fastpath
/// advisory |   132527 |   (null) |(null)|(null) |   (null)   |    (null)     | 1233048257 | 3054176128 |        1 | ExclusiveLock |    t    |    f
/// ```
///
/// `classid` is the key's high 32 bits and `objid` its low 32, which is what makes
/// `(classid::bigint << 32) | objid::bigint` the id the client passed. `objsubid` is `1` for the
/// single-argument form — the two-argument `pg_advisory_lock(int, int)` is `2`, which is the whole
/// reason the column is there.
#[test]
fn a_session_advisory_lock_is_a_row_in_pg_locks() {
    let mut node = parity::Node::new(&[]);
    node.run("SELECT pg_advisory_lock(5295901941258979200)")
        .unwrap();

    // The client's own query, verbatim from `connection_test.rb`.
    assert_eq!(
        node.rows(
            "SELECT locktype, (classid::bigint << 32) | objid::bigint AS lock_id \
             FROM pg_locks WHERE locktype = 'advisory'"
        ),
        vec![vec![
            "advisory".to_string(),
            "5295901941258979200".to_string()
        ]]
    );

    // And the rest of the row, which is what says the two halves were split rather than invented.
    assert_eq!(
        node.rows(
            "SELECT classid, objid, objsubid, mode, granted, fastpath, relation, page, tuple, \
             virtualxid, transactionid FROM pg_locks WHERE locktype = 'advisory'"
        ),
        vec![vec![
            "1233048257".to_string(),
            "3054176128".to_string(),
            "1".to_string(),
            "ExclusiveLock".to_string(),
            "t".to_string(),
            "f".to_string(),
            // The harness prints a NULL as `\\N`, and every one of these is NULL on a real
            // server too: an advisory lock has no relation, no page, no tuple and no transaction.
            "\\N".to_string(),
            "\\N".to_string(),
            "\\N".to_string(),
            "\\N".to_string(),
            "\\N".to_string(),
        ]]
    );

    // **Re-entrant, and still one row.** A session never conflicts with itself, and a second hold
    // of the same key in the same mode is not a second row — measured.
    node.run("SELECT pg_advisory_lock(5295901941258979200)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_locks WHERE locktype = 'advisory'"),
        vec![vec!["1".to_string()]]
    );

    // Two holds, two unlocks, both `t`; and then the row is gone.
    assert_eq!(
        node.rows("SELECT pg_advisory_unlock(5295901941258979200)"),
        vec![vec!["t".to_string()]]
    );
    assert_eq!(
        node.rows("SELECT pg_advisory_unlock(5295901941258979200)"),
        vec![vec!["t".to_string()]]
    );
    assert!(
        node.rows("SELECT * FROM pg_locks WHERE locktype = 'advisory'")
            .is_empty(),
        "released, so the row goes with it"
    );

    // Releasing one nobody holds is `f`, not an error.
    assert_eq!(
        node.rows("SELECT pg_advisory_unlock(5295901941258979200)"),
        vec![vec!["f".to_string()]]
    );
}
