//! Contract C3 for values: what we print and read must be what PostgreSQL 19 prints and reads.
//!
//! `tests/corpus/pg19_values.txt` is 184 lines of what a real PostgreSQL 19beta1 did with a value.
//! This replays every one of them through [`Datum::from_text`] and [`Datum::to_text`] and requires
//! the same answer — the same characters for a value, the same SQLSTATE *and* the same message for
//! a refusal.
//!
//! Both directions are covered by one pass because the corpus holds a round trip: the input column
//! is what PostgreSQL's input function was given and the output column is what its output function
//! then printed, so reading the first and printing it has to produce the second.
//!
//! # Divergences are held from both sides
//!
//! A handful of inputs PostgreSQL accepts are answered here with contract C2's `0A000` instead —
//! hexadecimal floats, and the parts of PostgreSQL's datetime grammar outside ISO 8601. Each is
//! listed in [`DIVERGENCES`] with the reason, and the list is checked in both directions: an
//! unlisted divergence fails the build, and so does a listed one that has started agreeing, so
//! closing a gap cannot be absorbed silently.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::value::{ColumnType, Datum};
use esker_sql::value::{PgDatum};

/// Inputs a real PostgreSQL reads and this crate deliberately does not, each answered with
/// `0A000 feature_not_supported` naming the construct.
///
/// The datetime entries are one decision: PostgreSQL's `DecodeDateTime` is a large parser with a
/// `DateStyle`-dependent grammar, a time zone database and a set of special values, and
/// implementing a *part* of it is how a server ends up returning a confidently wrong instant.
/// Reading ISO 8601 and naming everything else is the honest half.
const DIVERGENCES: &[(&str, &str, &str)] = &[
    ("float8", "0x10", "hexadecimal float input"),
    ("float8", "0x1p3", "hexadecimal float input"),
    ("timestamptz", "01/02/2024", "a non-ISO date"),
    (
        "timestamptz",
        "2024-01-01 10:00:00 America/New_York",
        "a named time zone",
    ),
    (
        "timestamptz",
        "2024-01-01 10:00:00 UTC",
        "a named time zone",
    ),
    (
        "timestamptz",
        "epoch",
        "one of PostgreSQL's special datetime inputs",
    ),
];

#[test]
fn every_value_reads_and_prints_the_way_postgresql_19_does() {
    let mut checked = 0;
    let mut diverged = Vec::new();

    for (line_number, line) in corpus() {
        let (ty, input, expected) = line;
        let column_type = column_type(&ty);
        let outcome = Datum::from_text(column_type, &input).map(|datum| datum.to_text());
        let listed = DIVERGENCES
            .iter()
            .find(|(dty, dinput, _)| *dty == ty && *dinput == input);

        if let Some((_, _, reason)) = listed {
            let error = outcome.as_ref().err().unwrap_or_else(|| {
                panic!(
                    "line {line_number}: {ty} {input:?} is listed as a divergence ({reason}) and \
                     now agrees with PostgreSQL -- delete the entry"
                )
            });
            assert_eq!(
                error.sqlstate(),
                esker_sql::sqlstate::FEATURE_NOT_SUPPORTED,
                "line {line_number}: a divergence must be reported as 0A000, not as a failure to \
                 read valid input"
            );
            assert!(
                error.to_string().contains(reason),
                "line {line_number}: `{error}` does not name the construct ({reason})"
            );
            diverged.push((ty, input));
            continue;
        }

        match expected.strip_prefix('!') {
            None => {
                let printed = outcome.unwrap_or_else(|error| {
                    panic!("line {line_number}: {ty} {input:?} -> {error} ({}), but PostgreSQL read it as {expected:?}", error.sqlstate())
                });
                assert_eq!(
                    printed.as_deref(),
                    Some(expected.as_str()),
                    "line {line_number}: {ty} {input:?} printed differently"
                );
            }
            Some(refusal) => {
                let (state, message) = refusal.split_once(' ').expect("SQLSTATE then message");
                let error = match outcome {
                    Err(error) => error,
                    Ok(value) => panic!(
                        "line {line_number}: {ty} {input:?} was read as {value:?}, but PostgreSQL \
                         refused it with {state}"
                    ),
                };
                assert_eq!(
                    error.sqlstate(),
                    state,
                    "line {line_number}: {ty} {input:?} -> `{error}`, wrong SQLSTATE"
                );
                assert_eq!(
                    error.to_string(),
                    message,
                    "line {line_number}: {ty} {input:?} -> wrong message"
                );
            }
        }
        checked += 1;
    }

    assert!(checked > 150, "the corpus did not load: {checked} cases");
    assert_eq!(
        diverged.len(),
        DIVERGENCES.len(),
        "a listed divergence is not in the corpus any more: {diverged:?}"
    );
}

/// A value that PostgreSQL prints must read back as itself, or a client that copied a value out of
/// one query could not put it into the next.
#[test]
fn printing_a_value_and_reading_it_back_is_the_identity() {
    for (line_number, (ty, input, expected)) in corpus() {
        if expected.starts_with('!') {
            continue;
        }
        let column_type = column_type(&ty);
        let Ok(datum) = Datum::from_text(column_type, &input) else {
            continue; // a divergence; the test above is what holds those
        };
        let printed = datum.to_text().expect("no NULL in the corpus");
        let reread = Datum::from_text(column_type, &printed).unwrap_or_else(|error| {
            panic!("line {line_number}: {printed:?} did not read back: {error}")
        });
        assert_eq!(
            reread, datum,
            "line {line_number}: {ty} {input:?} printed as {printed:?} and came back different"
        );
    }
}

fn column_type(name: &str) -> ColumnType {
    match name {
        "int8" => ColumnType::Int8,
        "text" => ColumnType::Text,
        "bool" => ColumnType::Bool,
        "bytea" => ColumnType::Bytea,
        "timestamptz" => ColumnType::TimestampTz,
        "float8" => ColumnType::Double,
        other => panic!("the corpus names a type this crate does not have: {other}"),
    }
}

/// The corpus, unescaped: `\\` is one backslash, `\t` a tab, `\n` a newline.
fn corpus() -> impl Iterator<Item = (usize, (String, String, String))> {
    include_str!("corpus/pg19_values.txt")
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|(index, line)| {
            let mut fields = line.split('\t');
            let mut next =
                || {
                    unescape(fields.next().unwrap_or_else(|| {
                        panic!("line {}: three tab-separated fields", index + 1)
                    }))
                };
            (index + 1, (next(), next(), next()))
        })
}

fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => {
                out.push('\\');
                if other != '\\' {
                    out.push(other);
                }
            }
            None => out.push('\\'),
        }
    }
    out
}
