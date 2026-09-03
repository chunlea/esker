//! `~`, `~*`, `!~`, `!~*` — POSIX regular-expression matching, written in-house.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is over literals or over a table the corpus builds.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog trade: `pg_typeof` answers a `regtype` on a real server and `text`
    // here, with the same characters in it. The row agrees — `boolean`, both.
    types: &["SELECT 'r', pg_typeof('abc' ~ 'b')"],
    answers: &[
        (
            "SELECT 'r', E'a\\nb' ~ 'a.b', E'a\\nb' ~ '^a.b$'",
            "**Not the operator**: `E''` is PostgreSQL\u{2019}s escape-string literal, a *lexer* \
             feature this node does not have, and it is only here because it is the shortest way \
             to write a newline in a probe. The fact it pins — that `.` matches a newline and that \
             `^`/`$` are string anchors rather than line anchors — is asserted directly in \
             `dot_matches_a_newline_and_the_anchors_are_the_strings`, with the newline written \
             into an ordinary literal instead",
        ),
        (
            "SELECT 'r', 'abc' ~ '(?i)ABC'",
            "`(?i)` is PostgreSQL\u{2019}s **advanced** regular expression, not POSIX ERE, and the \
             grant for this unit is the ERE subset. Refused by name (`0A000`) rather than \
             approximated: a node that ignored the flag would answer `f` where a real server \
             answers `t`, which is a wrong answer rather than a gap",
        ),
        (
            "SELECT 'r', 'aab' ~ '^(a)\\1b$'",
            "A **back-reference**, which no finite automaton can express — it is what separates an \
             ERE from a backtracking engine, and taking it would mean giving up the linear-time \
             matcher that keeps a user\u{2019}s pattern from being a denial of service. Refused by \
             name, for the same reason `(?i)` is",
        ),
    ],
};

#[test]
fn every_regex_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_regex_match.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The four patterns `ActiveRecord` actually sends**, which are the whole reason this exists.
#[test]
fn the_four_patterns_the_adapter_sends() {
    let mut node = parity::Node::new(&[]);
    // `schema_names`, whose absence cost `pg19_schema.txt` sixty-two statements.
    assert_eq!(
        node.rows("SELECT 'pg_catalog' !~ '^pg_.*', 'public' !~ '^pg_.*'"),
        [["f", "t"]]
    );
    // `pk_and_sequence_for`, on the path every model takes at boot.
    assert_eq!(
        node.rows(
            "SELECT 'nextval(''s''::regclass)' ~* 'nextval|uuid_generate|gen_random_uuid', \
             'now()' ~* 'nextval|uuid_generate|gen_random_uuid'"
        ),
        [["t", "f"]]
    );
    assert_eq!(node.rows("SELECT 'now()' !~* 'nextval'"), [["t"]]);
    assert_eq!(node.rows("SELECT 's' ~ '.', '' ~ '.'"), [["t", "f"]]);
}

/// **`.` matches a newline**, which POSIX says and most engines do not — and `^`/`$` are string
/// anchors, not line anchors, which is the same fact from the other side.
#[test]
fn dot_matches_a_newline_and_the_anchors_are_the_strings() {
    let mut node = parity::Node::new(&[]);
    // A real newline inside an ordinary literal, because the `E''` spelling the capture uses is a
    // *lexer* feature this node does not have — declared in the corpus above, and nothing to do
    // with the operator.
    assert_eq!(
        node.rows("SELECT 'a\nb' ~ 'a.b', 'a\nb' ~ '^a.b$'"),
        [["t", "t"]]
    );
}

/// **The empty pattern matches everything**, and a pattern is a search until it is anchored.
#[test]
fn the_empty_pattern_matches_and_a_pattern_is_a_search() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("SELECT 'abc' ~ '', 'abc' !~ ''"), [["t", "f"]]);
    assert_eq!(
        node.rows("SELECT 'abc' ~ 'b', 'abc' ~ '^b', 'abc' ~ '^abc$'"),
        [["t", "f", "t"]]
    );
}

/// A NULL on either side is NULL, and the negated spelling does not rescue it.
#[test]
fn a_null_operand_is_null_for_all_four() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT NULL::text ~ 'b', 'abc' ~ NULL::text, NULL::text !~ 'b', 'abc' !~ NULL::text"
        ),
        [["\\N", "\\N", "\\N", "\\N"]]
    );
}

/// The subset, one probe each: classes, ranges, named classes, grouping, alternation, the three
/// quantifiers, bounds, and the escapes.
#[test]
fn the_subset_the_captures_need() {
    let mut node = parity::Node::new(&[]);
    for (sql, expected) in [
        (
            "SELECT 'aaa' ~ '^a*$', '' ~ '^a*$', 'b' ~ '^a*$'",
            vec!["t", "t", "f"],
        ),
        ("SELECT 'aaa' ~ '^a+$', '' ~ '^a+$'", vec!["t", "f"]),
        (
            "SELECT 'a' ~ '^ab?$', 'ab' ~ '^ab?$', 'abb' ~ '^ab?$'",
            vec!["t", "t", "f"],
        ),
        (
            "SELECT 'cat' ~ '^(cat|dog)$', 'dog' ~ '^(cat|dog)$', 'cow' ~ '^(cat|dog)$'",
            vec!["t", "t", "f"],
        ),
        (
            "SELECT 'ab' ~ '^(ab)*$', 'abab' ~ '^(ab)*$', 'aba' ~ '^(ab)*$'",
            vec!["t", "t", "f"],
        ),
        (
            "SELECT 'a' ~ '^[abc]$', 'd' ~ '^[abc]$', 'a' ~ '^[^abc]$', 'd' ~ '^[^abc]$'",
            vec!["t", "f", "f", "t"],
        ),
        (
            "SELECT 'f' ~ '^[a-z]$', 'F' ~ '^[a-z]$', '5' ~ '^[0-9]+$', '5a' ~ '^[0-9]+$'",
            vec!["t", "f", "t", "f"],
        ),
        (
            "SELECT 'a' ~ '^[[:alpha:]]$', '1' ~ '^[[:alpha:]]$', '1' ~ '^[[:digit:]]$'",
            vec!["t", "f", "t"],
        ),
        (
            "SELECT 'foo' ~ 'o{2}', 'fo' ~ 'o{2}', 'foo' ~ '^fo{1,2}$'",
            vec!["t", "f", "t"],
        ),
        // **A leading `]` inside a class is a literal.**
        ("SELECT 'a]b' ~ '^a[]]b$'", vec!["t"]),
        // **`\\a` is not `a`**: escaping is not "drop the backslash".
        (
            "SELECT 'a.c' ~ '^a\\.c$', 'abc' ~ '^a\\.c$', 'a*c' ~ '^a\\*c$'",
            vec!["t", "f", "t"],
        ),
        ("SELECT 'aXbXc' ~ 'X.*X', 'abc' ~ 'X.*X'", vec!["t", "f"]),
    ] {
        assert_eq!(node.rows(sql), [expected], "{sql}");
    }
}

/// **Three `2201B` reasons under one code**, and PostgreSQL's own sentences.
#[test]
fn a_malformed_pattern_names_what_is_wrong() {
    let mut node = parity::Node::new(&[]);
    for (pattern, message) in [
        ("[", "brackets [] not balanced"),
        ("(a", "parentheses () not balanced"),
        ("*", "quantifier operand invalid"),
    ] {
        let error = node
            .run(&format!("SELECT 'abc' ~ '{pattern}'"))
            .unwrap_err();
        assert_eq!(error.sqlstate(), "2201B", "{pattern}");
        assert_eq!(
            error.to_string(),
            format!("invalid regular expression: {message}"),
            "{pattern}"
        );
    }
}

/// **A non-text operand is `42883`, not a cast**, and the other side is reported as `unknown`
/// whichever one is at fault.
#[test]
fn a_non_text_operand_is_an_operator_that_does_not_exist() {
    let mut node = parity::Node::new(&[]);
    let error = node.run("SELECT 1 ~ 'a'").unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    assert_eq!(
        error.to_string(),
        "operator does not exist: bigint ~ unknown"
    );
}

/// **Outside the subset is refused by name**, not approximated — the two constructs the capture
/// records PostgreSQL accepting and this node does not build.
#[test]
fn an_advanced_regular_expression_is_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    let error = node.run("SELECT 'abc' ~ '(?i)ABC'").unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    let error = node.run("SELECT 'aab' ~ '^(a)\\1b$'").unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
}

/// The operator over a **column**, which is how every statement that matters uses it.
#[test]
fn the_operator_filters_a_scan() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE rx (id bigserial primary key, s character varying)",
        "INSERT INTO rx (s) VALUES ('pg_catalog'), ('public'), ('test_schema'), (NULL)",
    ]);
    assert_eq!(
        node.rows("SELECT s FROM rx WHERE s !~ '^pg_.*' ORDER BY id"),
        vec![vec!["public"], vec!["test_schema"]]
    );
    assert_eq!(
        node.rows("SELECT s FROM rx WHERE s ~ '^pg_.*' ORDER BY id"),
        [["pg_catalog"]]
    );
    // A NULL row is admitted by neither, which is the three-valued rule doing its work.
    assert_eq!(
        node.rows("SELECT count(*) FROM rx WHERE s ~* 'PUBLIC'"),
        [["1"]]
    );
}
