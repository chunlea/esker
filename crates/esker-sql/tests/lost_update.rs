//! Run 66's probe, in process: **three writers, one row, and every increment has to land.**
//!
//! The harness measured 234–240 of 240 against `329250f0` with **zero errors reported** — a lost
//! update wearing a successful commit, which is the one failure in ADR 0057's design that destroys
//! data rather than answering wrongly. This file is that probe with the network taken out, so the
//! race can be run a thousand times a second instead of once a round.
//!
//! The transaction is the probe's, statement for statement, and the second `UPDATE` is not
//! decoration: a key written by an **earlier statement of the same transaction** is the case
//! ADR 0057 §4 singles out, and it is the one a single-`UPDATE` probe cannot reach.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use parity::Pair;

/// Three writers, forty rounds each, two increments per round.
#[test]
fn every_increment_lands_under_three_concurrent_writers() {
    const WRITERS: usize = 6;
    const ROUNDS: usize = 200;

    let pair = Pair::new(&[
        "CREATE TABLE rc_conflict (id bigint primary key, n bigint)",
        "INSERT INTO rc_conflict (id, n) VALUES (1, 0)",
    ]);

    let mut serialization = 0_usize;
    let mut other: Vec<String> = Vec::new();
    let threads: Vec<_> = (0..WRITERS)
        .map(|_| {
            let sessions = pair.sessions();
            std::thread::spawn(move || {
                let mut failures = (0_usize, Vec::new());
                for _ in 0..ROUNDS {
                    // **A new session per round**, which is what the probe does: it connects
                    // inside the loop, so every round is a fresh executor and a fresh transaction.
                    let mut session = sessions.session();
                    // The probe's own transaction, statement for statement.
                    let outcome = session
                        .run("BEGIN")
                        .and_then(|_| session.run("SELECT n FROM rc_conflict WHERE id = 1"))
                        .and_then(|_| session.run("UPDATE rc_conflict SET n = n + 1 WHERE id = 1"))
                        .and_then(|_| session.run("UPDATE rc_conflict SET n = n + 1 WHERE id = 1"))
                        .and_then(|_| session.run("COMMIT"));
                    if let Err(error) = outcome {
                        if error.sqlstate() == "40001" {
                            failures.0 += 1;
                        } else {
                            failures.1.push(format!("{} {error}", error.sqlstate()));
                        }
                        let _ = session.run("ROLLBACK");
                    }
                }
                failures
            })
        })
        .collect();
    for thread in threads {
        let (serialized, others) = thread.join().unwrap();
        serialization += serialized;
        other.extend(others);
    }

    let mut admin = pair.session();
    let landed: i64 = admin.rows("SELECT n FROM rc_conflict WHERE id = 1")[0][0]
        .parse()
        .unwrap();
    let expected = (WRITERS * ROUNDS * 2) as i64;
    assert!(
        other.is_empty(),
        "errors that are not a serialization failure: {other:?}"
    );
    assert_eq!(
        serialization, 0,
        "READ COMMITTED waits for the writer in front of it; it does not tell the caller to retry"
    );
    assert_eq!(
        landed, expected,
        "{landed} of {expected} increments landed — the rest were committed over, silently"
    );
}
