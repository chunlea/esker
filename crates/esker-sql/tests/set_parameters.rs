//! `SET` / `SHOW` / `RESET` of run-time parameters — four rows of run 45's ranking, one shape.
//!
//! 33 tests between them, and the statements are verbatim from the tests that send them:
//! `set lc_monetary = 'C'` (`money_test`), `set idle_in_transaction_session_timeout = '10ms'`
//! (`connection_test`), `SET search_path TO '$user',public` (`schema_authorization_test`), and
//! `SET geqo TO off` read back with `show geqo`.
//!
//! **`SHOW` is half the requirement, not an extra.** The tests read the value back rather than
//! trusting the `SET`, so a node that accepted the statement and reported the old value would fail
//! every one of them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a property of the session.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty.** `current_schemas(false)` was a `text` here against a real server's `name[]`, with
    // the same characters in it; `name[]` is a type this node has now (`tests/name_array.rs`) and
    // the row agrees on its declared type too.
    types: &[],
    answers: &[
        // **A property of the oracle's container, not of PostgreSQL** — the capture's own header
        // says so. `esker-pg19` boots `lc_monetary` at `en_US.utf8`; this node has no locale
        // database at all, so `C` is the honest default: the one locale whose rules are "no
        // rules". What the suite needs is that `SET` changes it and `SHOW` reports the change, and
        // both do.
        (
            "SHOW lc_monetary",
            "The boot value differs: `en_US.utf8` on the oracle's container, `C` here. Every \
             other line about this parameter agrees.",
            "pg19_set_parameters.txt:56",
        ),
        // **Surfaced by ADR 0082, and older than it.** `SET TimeZone` earlier in this corpus was
        // a refusal that aborted the transaction and swallowed the forty-four statements after it;
        // the zone table answers it now, and this is the first of those to disagree. `SET LOCAL`
        // is refused by name here for every parameter (`tests/session_parameters.rs`), which is
        // its own gap and not this one — the value and the parameter are both fine.
        (
            "SET LOCAL lc_monetary = 'C'",
            "`SET LOCAL` is refused by name for every parameter, which is a gap of its own",
            "pg19_set_parameters.txt:85",
        ),
    ],
};

#[test]
fn every_set_parameter_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_set_parameters.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The two parameters the ranking is mostly made of**, set and read back.
#[test]
fn lc_monetary_and_the_idle_timeout_are_set_and_shown() {
    let mut node = parity::Node::new(&[]);
    node.run("set lc_monetary = 'C'").unwrap();
    assert_eq!(node.rows("SHOW lc_monetary"), [["C"]]);
    assert_eq!(node.rows("SELECT current_setting('lc_monetary')"), [["C"]]);

    // **The unit stays in the value.** `SHOW` gives `10ms`; only `pg_settings.unit` separates it.
    assert_eq!(
        node.rows("SHOW idle_in_transaction_session_timeout"),
        [["0"]]
    );
    node.run("set idle_in_transaction_session_timeout = '10ms'")
        .unwrap();
    assert_eq!(
        node.rows("SHOW idle_in_transaction_session_timeout"),
        [["10ms"]]
    );
    // And zero reads back bare, not as `0ms`.
    node.run("set idle_in_transaction_session_timeout = 0")
        .unwrap();
    assert_eq!(
        node.rows("SHOW idle_in_transaction_session_timeout"),
        [["0"]]
    );
}

/// **`SHOW` answers text whatever the parameter's real type is**, and a boolean reads back
/// `on`/`off` rather than `t`/`f`.
#[test]
fn a_boolean_parameter_shows_on_and_off() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("show geqo"), [["on"]]);
    node.run("SET geqo TO off").unwrap();
    assert_eq!(node.rows("show geqo"), [["off"]]);
    node.run("SET debug_print_plan TO on").unwrap();
    assert_eq!(node.rows("SHOW debug_print_plan"), [["on"]]);
}

/// **The value comes back normalised, not as it was sent.**
///
/// `SET search_path TO '$user',public` reads back `"$user", public` — requoted, and with a space
/// after the comma. A node that echoed the input would fail every test that compares the read-back.
#[test]
fn search_path_is_reported_the_way_postgresql_prints_it() {
    let mut node = parity::Node::new(&[]);
    node.run("SET search_path TO '$user',public").unwrap();
    assert_eq!(node.rows("SHOW search_path"), [["\"$user\", public"]]);
}

/// **An unknown name is `42704` from all three entry points — and `current_setting(…, true)` is
/// the one shape that must not raise.**
#[test]
fn an_unknown_parameter_is_42704_except_through_the_escape_hatch() {
    let mut node = parity::Node::new(&[]);
    for written in [
        "SET nosuchparameter = 'x'",
        "SHOW nosuchparameter",
        "SELECT current_setting('nosuchparameter')",
    ] {
        let error = node.run(written).unwrap_err();
        assert_eq!(error.sqlstate(), "42704", "for {written}");
        assert_eq!(
            error.to_string(),
            "unrecognized configuration parameter \"nosuchparameter\"",
            "for {written}"
        );
    }
    // The documented escape hatch: NULL rather than an error.
    assert_eq!(
        node.rows("SELECT current_setting('nosuchparameter', true) IS NULL"),
        [["t"]]
    );
}

/// **A bad value is `22023`, which is a different class from an unknown name.**
#[test]
fn a_bad_value_is_22023_and_names_the_parameter() {
    let mut node = parity::Node::new(&[]);
    let error = node
        .run("set idle_in_transaction_session_timeout = 'banana'")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "22023");
    assert_eq!(
        error.to_string(),
        "invalid value for parameter \"idle_in_transaction_session_timeout\": \"banana\""
    );
}

/// `RESET`, `RESET ALL` and `SET SESSION` — three spellings the suite uses.
#[test]
fn reset_and_reset_all_go_back_to_the_boot_value() {
    let mut node = parity::Node::new(&[]);
    let boot = node.rows("SHOW lc_monetary")[0][0].clone();
    node.run("SET SESSION lc_monetary = 'C'").unwrap();
    assert_eq!(node.rows("SHOW lc_monetary"), [["C"]]);
    node.run("RESET lc_monetary").unwrap();
    assert_eq!(node.rows("SHOW lc_monetary"), [[boot.as_str()]]);

    node.run("set lc_monetary = 'C'").unwrap();
    node.run("RESET ALL").unwrap();
    assert_eq!(node.rows("SHOW lc_monetary"), [[boot.as_str()]]);
}
