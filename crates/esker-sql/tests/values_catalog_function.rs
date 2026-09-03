//! A catalog function inside a `VALUES` list — `insert_all`'s inlined `CURRENT_TIMESTAMP`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus creates its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// `time with time zone` is not one of the stored types, and both of these are about that.
const NO_TIMETZ: &str = "**`CURRENT_TIME` is `time with time zone`, which is not one of the \
                         stored types** (ADR 0033). A real server refuses this `INSERT` with the \
                         `42804` that names both types; this node refuses it a step earlier, by \
                         name, because it has no type to name. Its unzoned twin `LOCALTIME` is \
                         here and answers that `42804` word for word — the line below it — so \
                         what is missing is one type and not the rule. Adding it is a row-codec \
                         change (`esker-keys`, ADR 0030) and belongs with the rest of the type \
                         surface rather than with this shape.";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "INSERT INTO vl_posts (title, created_at, updated_at) VALUES ('c', CURRENT_DATE, CURRENT_TIME) RETURNING id",
            NO_TIMETZ,
        ),
        (
            "SELECT 'r', pg_typeof(CURRENT_TIMESTAMP), pg_typeof(now()), pg_typeof(LOCALTIMESTAMP), pg_typeof(CURRENT_DATE), pg_typeof(CURRENT_TIME)",
            NO_TIMETZ,
        ),
        (
            "INSERT INTO vl_posts (title, created_at, updated_at) VALUES ('bad', nosuchfunction(), CURRENT_TIMESTAMP)",
            "`0A000` here against `42883` there, the same contract-C2 divergence \
             `tests/default_expression.rs` already declares for `nosuchfunc()`: on a real server \
             the function genuinely does not exist, and on this one it is a function nobody has \
             implemented yet, and nothing here can tell the two apart without PostgreSQL's whole \
             function catalogue. **Both refuse, and both leave the row unwritten** — which is \
             what the `count(*)` two lines down checks.",
        ),
    ],
};

#[test]
fn every_values_catalog_function_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_values_catalog_function.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
