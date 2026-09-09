//! `varchar(n)`, `character(n)` and `timestamp(p)` against PostgreSQL 19beta1's own answers.
//!
//! One mechanism and three types, which is why they are one file. A length or a precision is a
//! property of the **column** — `pg_attribute.atttypmod`, where a real server keeps it — and not
//! of the type, so `pg_type` still has one `varchar` row and the number rides on the column.
//!
//! The three do different things with their number, and that is the whole of the unit:
//! `varchar(n)` **refuses** a longer value, `character(n)` **pads** a shorter one, and
//! `timestamp(p)` **rounds**. `tests/corpus/pg19_typmod.txt` carries the capture, including the
//! two rounding rules nobody would guess and the two vocabularies one type uses for its errors.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT id, length(c), c || '|' FROM tm WHERE id = 1",
        "**A `character(n)`'s trailing blanks, and the two places they are supposed to vanish.** \
         The row reads `1|1|x|` on a real server and `1|3|x  |` here: `length(c)` over a \
         `character(3)` holding `x` is **1** there and 3 here, and `c || '|'` is `x|` there and \
         `x  |` here. A `bpchar` is stored blank-padded and printed padded — `SELECT c` is `x  ` \
         in both — but `length` ignores the padding and a concatenation coerces to `text`, which \
         strips it. This node pads on the way out and keeps the padding through both.\n\
         \n\
         The entry that used to stand here said these were \"two scalar operations this node does \
         not have\", declared `UNMEASURED`, and the row *also* mis-parsed: `x|` ends in a `|`, so \
         the format split it into an extra cell and the report showed two lines that looked the \
         same (`docs/plans/debts-v1.1.md` #20). Escaping the value is what let the real difference \
         be read at all. `docs/plans/debts-v1.1.md` #31.",
        "pg19_typmod.txt:71",
    )],
};

#[test]
fn every_typmod_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_typmod.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `character(n)` pads, and padding is what makes the comparison work.
///
/// Two facts that are one fact: a value stored padded to `n` makes plain byte comparison **be**
/// PostgreSQL's blank-insensitive comparison. That is what lets a `character(n)` sit in an index
/// key without breaking "equal values encode identically" — the invariant `esker-keys`' property
/// tests exist for — and it is why this type could not land before the typmod did.
#[test]
fn a_character_column_pads_and_compares_blind_to_the_padding() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE c (id int8 PRIMARY KEY, v char(3))",
        "INSERT INTO c VALUES (1, 'x')",
        "INSERT INTO c VALUES (2, '')",
        "INSERT INTO c VALUES (3, 'abc')",
    ] {
        node.run(statement).unwrap();
    }

    // Printed padded, which is what a real server sends: `x` becomes `x  `.
    assert_eq!(
        node.rows("SELECT id, v FROM c ORDER BY id"),
        vec![vec!["1", "x  "], vec!["2", "   "], vec!["3", "abc"],]
    );

    // And every spelling of the same value finds it, however many blanks the query wrote.
    for query in [
        "SELECT id FROM c WHERE v = 'x'",
        "SELECT id FROM c WHERE v = 'x  '",
        "SELECT id FROM c WHERE v = 'x    '",
    ] {
        assert_eq!(node.rows(query), vec![vec!["1"]], "{query}");
    }

    // Longer than `n` is `22001`, with the type spelled as `format_type` writes it.
    let error = node.run("INSERT INTO c VALUES (4, 'abcd')").unwrap_err();
    assert_eq!(error.sqlstate(), "22001");
    assert_eq!(error.to_string(), "value too long for type character(3)");
}

/// A bare `character` is `character(1)`, not "unlimited".
///
/// The opposite of `character varying`, whose bare spelling means no limit at all — and the reason
/// `char(n)` could not ride along with the unit that landed bare `varchar`: there is no useful
/// no-typmod form of it to land first.
#[test]
fn a_bare_character_is_character_of_one() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE w (id int8 PRIMARY KEY, a character, b character varying)")
        .unwrap();
    node.run("INSERT INTO w VALUES (1, 'y', 'anything at all')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT a, b FROM w"),
        vec![vec!["y", "anything at all"]]
    );

    // One character is the limit, and it is a limit rather than a default.
    let error = node.run("INSERT INTO w VALUES (2, 'yy', '')").unwrap_err();
    assert_eq!(error.sqlstate(), "22001");
    assert_eq!(error.to_string(), "value too long for type character(1)");
}

/// The string family does not decay uniformly under `min`/`max`.
///
/// `min(character(n))` is **`bpchar`** where `min(character varying)` is `text`. Measured, not
/// reasoned: `bpchar` has a `min` of its own and `varchar` borrows `text`'s.
#[test]
fn min_over_a_character_stays_bpchar_where_varchar_becomes_text() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE m (id int8 PRIMARY KEY, c char(3), v varchar(5))",
        "INSERT INTO m VALUES (1, 'x', 'exact')",
        "INSERT INTO m VALUES (2, 'abc', 'ab')",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT min(c), max(c), min(v), max(v) FROM m"),
        vec![vec!["abc", "x  ", "ab", "exact"]]
    );
}
