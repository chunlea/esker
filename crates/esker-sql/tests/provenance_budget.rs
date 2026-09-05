//! **How many declared divergences have never been measured**, held to a number that may fall.
//!
//! A divergence carrying [`UNMEASURED`] is honest — it says nobody has captured PostgreSQL's answer
//! — and it is still a hole. The list is allowed to shrink whenever a lane measures one on its way
//! past, and it may not grow without somebody deciding it should, which is what this asserts. The
//! same shape `deny.toml`'s crate budget uses, and for the same reason: an invisible number grows.
//!
//! Counted from the source rather than from the harness, because each test binary sees only its own
//! divergences and the budget is a property of all of them together.
//!
//! [ADR 0075](../../../docs/adr/0075-the-oracle-captures-live-in-the-repository.md) and
//! `docs/plans/divergence-provenance.md`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

/// What the sweep found when the rule landed. **Lower this when you measure one; do not raise it.**
const BUDGET: usize = 207;

#[test]
fn unmeasured_divergences_do_not_grow() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut found = 0;
    let mut per_file: Vec<(String, usize)> = Vec::new();
    for entry in std::fs::read_dir(&tests).expect("the tests directory is readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a readable test file");
        // This file names the literal in its own prose, and counting itself would be a budget that
        // spends itself.
        if path
            .file_name()
            .is_some_and(|name| name == "provenance_budget.rs")
        {
            continue;
        }
        let count = text.matches("\"UNMEASURED\"").count();
        if count > 0 {
            found += count;
            per_file.push((
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into(),
                count,
            ));
        }
    }
    per_file.sort_by_key(|(name, count)| (std::cmp::Reverse(*count), name.clone()));
    let worst: Vec<String> = per_file
        .iter()
        .take(8)
        .map(|(name, count)| format!("  {count:3}  {name}"))
        .collect();
    assert!(
        found <= BUDGET,
        "{found} declared divergences are UNMEASURED, and the budget is {BUDGET}.\n\nA new \
         divergence gets a capture with it — a corpus row whose PostgreSQL answer nobody \
         recorded is the one row nothing can check (ADR 0075). The heaviest files:\n{}",
        worst.join("\n")
    );
    // And the other direction: a budget nobody lowers is a budget nobody reads.
    assert!(
        found + 16 > BUDGET,
        "only {found} divergences are UNMEASURED but the budget is still {BUDGET} — lower it to \
         {found}, so the next one that creeps in is visible"
    );
}
