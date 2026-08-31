//! The whole `.slt` corpus, unchanged, against a real three-store cluster.
//!
//! `tests/slt.rs` runs these same files over `MemoryBackend`. Running them again over real stores
//! is not duplication: it is the only thing that says the corpus records *SQL* behaviour rather
//! than the fake's behaviour. Every file that passes here and there is one whose answers do not
//! depend on what is underneath the executor, which is what a storage-independent SQL layer is
//! supposed to mean.
//!
//! The files are used **as they are**. One written differently for the real store would be testing
//! the file rather than the store.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;
#[path = "slt_harness/mod.rs"]
mod harness;

use cluster::Cluster;

#[test]
fn every_slt_file_passes_against_real_stores() {
    let mut checked = 0;
    for (name, body) in harness::FILES {
        // A cluster per file, for the same reason the fake is per file: a file is a self-contained
        // story and must not depend on another having run first.
        let cluster = Cluster::start();
        checked += harness::run_file_on(name, body, std::sync::Arc::clone(&cluster.backend));
    }
    assert!(
        checked > 150,
        "only {checked} directives ran; the files did not load"
    );
}
