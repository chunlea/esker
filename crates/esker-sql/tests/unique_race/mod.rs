//! **A lost race on a unique key, and the code it is refused with**
//! ([ADR 0114](../../../../docs/adr/0114-a-unique-key-being-written-waits-at-read-committed.md) §3).
//!
//! One sequence, shared by the binary that runs it against three real stores
//! (`concurrent_unique_insert.rs`) and the one that runs it against `MemoryBackend`
//! (`serializable.rs`), so that the two sets are the same shapes by construction rather than by
//! copying. The table is Rails' `subscribers`: no primary key, and a unique index on `nick`, so the
//! index is the only thing two rows can collide on.
//!
//! B opens first and runs one statement, which fixes its snapshot: a read of `bob`, or `SELECT 1`,
//! which reads no table. A then inserts `bob`. B inserts `bob` — while A is still open, or after A
//! has committed — and both commit. What PostgreSQL 19 refuses B with, measured with two `psql`
//! sessions interleaved by `pg_sleep` (`esker-coord/s1-oracle-2026-09-13/`, `d/` and `h/`):
//!
//! ```text
//!   case  level            B read bob   B's INSERT               A          PostgreSQL 19
//!   04    REPEATABLE READ  count(*)     plain                    live       waits, 23505
//!   10    REPEATABLE READ  count(*)     plain                    committed  23505
//!   07    SERIALIZABLE     no           plain                    live       waits, 23505
//!   09    SERIALIZABLE     yes          plain                    committed  40001 (read/write dependencies)
//!   13    SERIALIZABLE     count(*)     ON CONFLICT DO NOTHING   live       waits, 40001 (concurrent update)
//!   14    SERIALIZABLE     no           ON CONFLICT DO NOTHING   live       waits, 40001 (concurrent update)
//!   15    REPEATABLE READ  count(*)     ON CONFLICT DO NOTHING   live       waits, 40001 (concurrent update)
//!   16    REPEATABLE READ  no           ON CONFLICT DO NOTHING   live       waits, 40001 (concurrent update)
//! ```
//!
//! Case 09 reads with Rails' `find_by`, as the real-store test that was §3's red half does, and
//! **13b** is case 13 with `find_by`: PostgreSQL's `40001` for `ON CONFLICT` comes from
//! `ExecCheckTupleVisible` — the arbiter found a row the snapshot cannot see — and 14 against 13
//! shows the read is not what decides it.
//!
//! **This node answers the same code, one statement later.** Neither snapshot level waits for a live
//! holder here, so B's `INSERT` answers at once and the race is lost at B's `COMMIT`. That is why the
//! sequence can run on one thread, and why B sets `lock_timeout`: a wait nobody expected is a `55P03`
//! in five seconds rather than a hang.

#![allow(
    dead_code,
    reason = "shared by two test binaries, and the real-store one keeps its own case 09"
)]

use esker_sql::pgwire::session::Outcome;

/// Rails' `subscribers`, as `activerecord/test/schema/schema.rb` declares it: `id: false`, so the
/// row key is an internal row id and the only thing two rows can collide on is the index.
pub(crate) const SUBSCRIBERS: [&str; 2] = [
    "CREATE TABLE subscribers (nick character varying NOT NULL, name character varying, \
     id integer, books_count integer NOT NULL DEFAULT 0, update_count integer NOT NULL DEFAULT 0)",
    "CREATE UNIQUE INDEX index_subscribers_on_nick ON subscribers (nick)",
];

/// How PostgreSQL's capture reads the key.
const COUNT: &str = "SELECT count(*) FROM subscribers WHERE nick = 'bob'";
/// How Rails reads it: `find_by(nick: "bob")`.
const FIND_BY: &str = "SELECT nick FROM subscribers WHERE nick = 'bob' LIMIT 1";
/// A first statement that reads no table, and still fixes B's snapshot before A commits.
const NO_TABLE: &str = "SELECT 1";
const PLAIN: &str = "INSERT INTO subscribers (nick) VALUES ('bob')";
const DO_NOTHING: &str =
    "INSERT INTO subscribers (nick) VALUES ('bob') ON CONFLICT (nick) DO NOTHING";
/// B's `INSERT … SELECT` reads the key itself: ADR 0114 §3's plan named this as the risk, and
/// `esker-coord/s1-oracle-2026-09-13/f/` measured it — PostgreSQL 19 counts a read in the statement that
/// writes the key exactly as a read before it.
const READS_IN_ITS_QUERY: &str = "INSERT INTO subscribers (nick) SELECT 'bob' WHERE NOT EXISTS \
                                  (SELECT 1 FROM subscribers WHERE nick = 'bob')";
/// The same read as a `count(*)` in a derived table.
const COUNTS_IN_ITS_QUERY: &str = "INSERT INTO subscribers (nick) SELECT 'bob' FROM (SELECT count(*) \
                                   AS n FROM subscribers WHERE nick = 'bob') c WHERE c.n = 0";
/// The same read at the top of the `INSERT`'s own query, over the table it writes.
const READS_AT_THE_TOP: &str = "INSERT INTO subscribers (nick) SELECT 'bob' FROM subscribers WHERE \
                                nick = 'bob' HAVING count(*) = 0";
/// An `INSERT … SELECT` whose query reads no table: the control for the three above.
const SELECTS_NO_TABLE: &str = "INSERT INTO subscribers (nick) SELECT 'bob'";

/// A session of whichever node the including binary runs.
pub(crate) trait Sql {
    fn sql(&mut self, sql: &str) -> esker_sql::Result<Outcome>;
}

/// One row of the table above.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Race {
    name: &'static str,
    level: &'static str,
    /// B's first statement.
    first: &'static str,
    /// B's `INSERT`.
    insert: &'static str,
    /// Whether A commits before B's `INSERT`, rather than after it.
    committed_first: bool,
    /// What PostgreSQL 19 refuses B with.
    postgres: &'static str,
}

pub(crate) const CASE_04: Race = Race {
    name: "case 04",
    level: "REPEATABLE READ",
    first: COUNT,
    insert: PLAIN,
    committed_first: false,
    postgres: "23505",
};

pub(crate) const CASE_10: Race = Race {
    name: "case 10",
    level: "REPEATABLE READ",
    first: COUNT,
    insert: PLAIN,
    committed_first: true,
    postgres: "23505",
};

pub(crate) const CASE_07: Race = Race {
    name: "case 07",
    level: "SERIALIZABLE",
    first: NO_TABLE,
    insert: PLAIN,
    committed_first: false,
    postgres: "23505",
};

pub(crate) const CASE_09: Race = Race {
    name: "case 09",
    level: "SERIALIZABLE",
    first: FIND_BY,
    insert: PLAIN,
    committed_first: true,
    postgres: "40001",
};

pub(crate) const CASE_13: Race = Race {
    name: "case 13",
    level: "SERIALIZABLE",
    first: COUNT,
    insert: DO_NOTHING,
    committed_first: false,
    postgres: "40001",
};

pub(crate) const CASE_13B: Race = Race {
    name: "case 13b",
    level: "SERIALIZABLE",
    first: FIND_BY,
    insert: DO_NOTHING,
    committed_first: false,
    postgres: "40001",
};

pub(crate) const CASE_14: Race = Race {
    name: "case 14",
    level: "SERIALIZABLE",
    first: NO_TABLE,
    insert: DO_NOTHING,
    committed_first: false,
    postgres: "40001",
};

pub(crate) const CASE_15: Race = Race {
    name: "case 15",
    level: "REPEATABLE READ",
    first: COUNT,
    insert: DO_NOTHING,
    committed_first: false,
    postgres: "40001",
};

pub(crate) const CASE_16: Race = Race {
    name: "case 16",
    level: "REPEATABLE READ",
    first: NO_TABLE,
    insert: DO_NOTHING,
    committed_first: false,
    postgres: "40001",
};

// Unit L: one statement that reads the key and inserts it (`esker-coord/s1-oracle-2026-09-13/f/`). B's
// first statement reads no table, so the only read of `bob` is inside B's `INSERT`.
pub(crate) const CASE_F1: Race = Race {
    name: "case f1",
    level: "SERIALIZABLE",
    first: NO_TABLE,
    insert: READS_IN_ITS_QUERY,
    committed_first: true,
    postgres: "40001",
};

pub(crate) const CASE_F2: Race = Race {
    name: "case f2",
    level: "SERIALIZABLE",
    first: NO_TABLE,
    insert: READS_IN_ITS_QUERY,
    committed_first: false,
    postgres: "40001",
};

pub(crate) const CASE_F3: Race = Race {
    name: "case f3",
    level: "SERIALIZABLE",
    first: NO_TABLE,
    insert: COUNTS_IN_ITS_QUERY,
    committed_first: true,
    postgres: "40001",
};

pub(crate) const CASE_F4: Race = Race {
    name: "case f4",
    level: "SERIALIZABLE",
    first: NO_TABLE,
    insert: SELECTS_NO_TABLE,
    committed_first: true,
    postgres: "23505",
};

pub(crate) const CASE_F5: Race = Race {
    name: "case f5",
    level: "REPEATABLE READ",
    first: NO_TABLE,
    insert: READS_IN_ITS_QUERY,
    committed_first: true,
    postgres: "23505",
};

pub(crate) const CASE_F6: Race = Race {
    name: "case f6",
    level: "REPEATABLE READ",
    first: NO_TABLE,
    insert: READS_IN_ITS_QUERY,
    committed_first: false,
    postgres: "23505",
};

pub(crate) const CASE_F8: Race = Race {
    name: "case f8",
    level: "SERIALIZABLE",
    first: NO_TABLE,
    insert: READS_AT_THE_TOP,
    committed_first: true,
    postgres: "40001",
};

pub(crate) const CASE_F9: Race = Race {
    name: "case f9",
    level: "SERIALIZABLE",
    first: NO_TABLE,
    insert: READS_AT_THE_TOP,
    committed_first: false,
    postgres: "40001",
};

pub(crate) const CASE_F10: Race = Race {
    name: "case f10",
    level: "REPEATABLE READ",
    first: NO_TABLE,
    insert: READS_AT_THE_TOP,
    committed_first: true,
    postgres: "23505",
};

/// Runs `race` with three sessions of one node, and asserts that B is refused with PostgreSQL's
/// code and that A's `bob` is the only one.
pub(crate) fn assert_refused_as_postgres<S: Sql>(a: &mut S, b: &mut S, fresh: &mut S, race: Race) {
    b.sql("SET lock_timeout = '5s'").unwrap();
    b.sql("BEGIN").unwrap();
    b.sql(&format!("SET TRANSACTION ISOLATION LEVEL {}", race.level))
        .unwrap();
    b.sql(race.first).unwrap();

    a.sql("BEGIN").unwrap();
    a.sql(&format!("SET TRANSACTION ISOLATION LEVEL {}", race.level))
        .unwrap();
    a.sql(PLAIN).unwrap();
    if race.committed_first {
        a.sql("COMMIT").unwrap();
    }

    let inserted = b.sql(race.insert);
    if !race.committed_first {
        a.sql("COMMIT").unwrap();
    }
    let answer = inserted.and_then(|_| b.sql("COMMIT"));
    let _ = b.sql("ROLLBACK");

    let Err(refused) = answer else {
        panic!("{}: B committed a second bob", race.name);
    };
    assert_eq!(
        refused.sqlstate(),
        race.postgres,
        "{}: B was told {refused}",
        race.name
    );
    let bobs = fresh.sql(COUNT).unwrap();
    assert_eq!(count(&bobs), "1", "{}: A's bob is the only one", race.name);
}

/// The first cell of an answer, as text.
fn count(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Rows { rows, .. } => rows
            .first()
            .and_then(|row| row.first())
            .and_then(Option::as_ref)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default(),
        Outcome::Done { tag } => tag.clone(),
    }
}
