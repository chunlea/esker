//! Contract C3 for the six `SET`s and two `SHOW`s `ActiveRecord` sends at connect.
//!
//! Sixty-eight statements put to a real PostgreSQL 19beta1 in one session and replayed against one
//! node (`corpus/pg19_set.txt`). Eight of `ActiveRecord`'s thirty-six are these
//! (`docs/plans/phase-9-rails.md` §2, unit 5) and they were the cheapest on the board — which is
//! exactly why they needed measuring rather than assuming: three of the eight are **not** inert,
//! and a `SET` accepted and ignored is a setting a client asked for and did not get.
//!
//! The corpus also pins the one that is easiest to get wrong by storing a parameter in the wrong
//! place: **a `SET` is transactional.** `ROLLBACK` puts the old value back and a `ROLLBACK TO`
//! undoes a `SET` made inside the savepoint, exactly as it undoes a write — a session parameter is
//! block state, not connection state.
//!
//! The bespoke tests below are the ones a corpus cannot hold: whether a notice is actually
//! suppressed is a fact about the wire, not about a result set.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: a session parameter needs no table.
const FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SET no_such_parameter = 1",
        "PostgreSQL answers 42704 because an un-namespaced name it does not know cannot be a \
             custom GUC. This node cannot tell `work_mem` — a parameter a real server has and \
             this one does not — from a name nobody has, without carrying PostgreSQL's whole GUC \
             table; so a SET it does not run is 0A000 naming it, which is contract C2 and does \
             not claim the parameter is absent. `SHOW` and `RESET` answer 42704 for both, which \
             is the same trade made the other way and is older than this unit.",
    )],
};

#[test]
fn every_set_and_show_answers_the_way_postgresql_19_does() {
    let checked = parity::replay(include_str!("corpus/pg19_set.txt"), FIXTURE, &DIVERGENCES);
    assert!(
        checked > 66,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `client_min_messages` is **honoured**, which is the whole reason `ActiveRecord` sends it: the
/// first thing its migration does is `DROP TABLE IF EXISTS`, and a real server says
/// `NOTICE: table … does not exist, skipping` unless told not to.
///
/// A corpus cannot hold this — `psql` prints a notice where the capture records rows — so it is a
/// test, and it asserts in **both** directions: silenced at `warning`, and back at `notice`.
#[test]
fn client_min_messages_actually_suppresses_a_notice() {
    let mut node = parity::Node::new(FIXTURE);

    node.run("DROP TABLE IF EXISTS nothing_here").unwrap();
    assert_eq!(
        node.executor_notices().len(),
        1,
        "the skipped DROP said nothing"
    );

    node.run("SET client_min_messages TO 'warning'").unwrap();
    node.run("DROP TABLE IF EXISTS nothing_here").unwrap();
    assert!(
        node.executor_notices().is_empty(),
        "a notice went out under client_min_messages = warning"
    );

    // And back: a threshold is not a one-way switch, and `RESET` is how a pooled connection
    // returns to what the next user expects.
    node.run("RESET client_min_messages").unwrap();
    node.run("DROP TABLE IF EXISTS nothing_here").unwrap();
    assert_eq!(node.executor_notices().len(), 1);
}

/// A value this node cannot mean is refused **by name**, never accepted and ignored.
///
/// A real server takes the zone; taking it here would honour the setting in `SHOW` and nowhere
/// else, because an instant still prints in UTC. **`search_path` left this test** with the
/// namespace unit: it is now honoured for real, so the value is no longer one this node cannot
/// mean — an entry naming no schema is *skipped*, which is what a real server does with it too.
#[test]
fn a_value_this_node_cannot_mean_is_refused_by_name() {
    let mut node = parity::Node::new(FIXTURE);

    let error = node.run("SET timezone TO 'America/New_York'").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        error.to_string(),
        "the time zone \"America/New_York\" is not supported"
    );

    // A path naming a schema that is not there is **accepted** and resolves to nothing, which is
    // what a real server does — `SHOW` gives it back as written and `current_schemas` drops it.
    node.run("SET search_path TO other").unwrap();
    assert_eq!(node.rows("SHOW search_path"), vec![vec!["other"]]);
    assert_eq!(node.rows("SELECT current_schemas(false)"), vec![vec!["{}"]]);

    // The two spellings ActiveRecord sends are both `public`, and both run.
    node.run("SET search_path TO public").unwrap();
    node.run("SET search_path TO \"$user\", public").unwrap();
    assert_eq!(
        node.rows("SHOW search_path"),
        vec![vec!["\"$user\", public"]]
    );

    // And the zone it sends is the one this node prints in.
    node.run("SET SESSION timezone TO 'UTC'").unwrap();
    assert_eq!(node.rows("SHOW timezone"), vec![vec!["UTC"]]);
}

/// `SET LOCAL` is refused by name. It is undone when the transaction ends, whichever way it ends,
/// and promoting it to a session-wide `SET` would leave the value behind after the block — which
/// is the one outcome the user asking for `LOCAL` was avoiding.
#[test]
fn set_local_is_refused_by_name() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node
        .run("SET LOCAL client_min_messages TO 'error'")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        error.to_string(),
        "SET LOCAL client_min_messages is not supported"
    );
}

/// `SHOW ALL` stays refused by name: its answer is every parameter of the server that answers it,
/// which is a fact about a deployment rather than about SQL, and this node has six.
#[test]
fn show_all_is_refused_by_name() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node.run("SHOW ALL").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(error.to_string(), "SHOW ALL is not supported");
}

/// A read-only parameter is `55P02` and not `42704`, which is the difference between "you may not
/// change this" and "there is no such thing" — and `max_identifier_length` is one a client reads
/// to decide how long a name it may generate.
#[test]
fn a_read_only_parameter_says_so() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(node.rows("SHOW max_identifier_length"), vec![vec!["63"]]);

    for statement in [
        "SET max_identifier_length = 100",
        "RESET max_identifier_length",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::CANT_CHANGE_RUNTIME_PARAM,
            "{statement}"
        );
        assert_eq!(
            error.to_string(),
            "parameter \"max_identifier_length\" cannot be changed",
            "{statement}"
        );
    }
}
