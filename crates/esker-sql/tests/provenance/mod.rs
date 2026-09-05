//! Where a declared divergence's PostgreSQL answer was measured.
//!
//! A corpus row carries what PostgreSQL 19 answered, and the harness compares this node against it
//! on every run — so a wrong expected value fails loudly. **Except for a declared divergence**,
//! where the harness checks only that the two still *differ*. That expected value is the one number
//! in this repository nothing verifies, and it is the only kind that can survive a green suite
//! indefinitely.
//!
//! It did. A corpus row claimed `'{"a":1}'::json || '{"b":2}'::json` answered `{"a":1}{"b":2}`;
//! PostgreSQL has no `||` for `json` at all and says `42883`. It was written from memory in the
//! same edit that declared it a divergence, so from that moment nothing could have caught it.
//!
//! So every divergence cites the capture line its answer was read from, and this module
//! **resolves** the citation against `tests/captures/` ([ADR
//! 0075](../../../../docs/adr/0075-the-oracle-captures-live-in-the-repository.md)). Filling the
//! citations in once would be bookkeeping; re-reading them every run is what keeps them true when
//! a capture is re-taken or a row moves.
//!
//! Shared by both harnesses rather than written twice, because it is a rule and not a helper.

/// One declared divergence: `(statement, reason, provenance)`.
pub(crate) type Divergence = (&'static str, &'static str, &'static str);

/// A divergence whose PostgreSQL answer nobody has captured, said out loud.
///
/// A declaration, not an omission. The count of these is asserted, so the list can shrink and
/// cannot grow without somebody deciding that it should.
pub(crate) const UNMEASURED: &str = "UNMEASURED";

/// Every divergence cites a capture line that still says what it claims, or declares it has none.
///
/// # Panics
///
/// When a provenance is missing, malformed, names a capture that is not there, points past the end
/// of one, or points at a line carrying a different statement.
pub(crate) fn check(answers: &[Divergence]) {
    let captures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("captures");
    let mut wrong = Vec::new();
    for (sql, _, provenance) in answers {
        if *provenance == UNMEASURED {
            continue;
        }
        let Some((file, line)) = provenance.rsplit_once(':') else {
            wrong.push(format!(
                "{sql}\n    {provenance:?} is neither {UNMEASURED} nor <capture file>:<line>"
            ));
            continue;
        };
        let Ok(at) = line.parse::<usize>() else {
            wrong.push(format!("{sql}\n    {provenance:?} has no line number"));
            continue;
        };
        let Ok(text) = std::fs::read_to_string(captures.join(file)) else {
            wrong.push(format!("{sql}\n    there is no tests/captures/{file}"));
            continue;
        };
        let Some(cited) = text.lines().nth(at.saturating_sub(1)) else {
            wrong.push(format!("{sql}\n    {file} has no line {at}"));
            continue;
        };
        // A capture line is `statement TAB types TAB answer`; the statement is what must still be
        // there. Compared with whitespace squeezed, because a Rust literal wraps where a capture
        // line does not.
        let cited = squeeze(cited.split('\t').next().unwrap_or_default());
        if cited != squeeze(sql) {
            wrong.push(format!(
                "{sql}\n    {provenance} now reads {cited:?}, which is a different statement"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} declared divergence(s) cannot be traced to a measurement (ADR 0075):\n\n{}",
        wrong.len(),
        wrong.join("\n\n")
    );
}

/// One line's worth of whitespace, so a wrapped Rust literal and a capture line compare as
/// statements rather than as layout.
fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
