//! Our own `.slt` runner. The files, the format and why there are two runners are all documented
//! in [`crate::harness`]; this file is the test that drives ours over the corpus.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "slt_harness/mod.rs"]
mod harness;

#[test]
fn every_slt_file_passes() {
    let mut checked = 0;
    for (name, body) in harness::FILES {
        checked += harness::run_file(name, body);
    }
    assert!(
        checked > 150,
        "only {checked} directives ran; the files did not load"
    );
}
