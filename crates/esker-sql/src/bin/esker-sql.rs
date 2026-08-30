//! The SQL node's entry point.
//!
//! A placeholder until the wire protocol lands in unit 2 of `docs/plans/phase-6a.md`; CLI
//! integration comes later still. It exists now so the binary target is part of the build from the
//! first commit rather than appearing later with its own compilation problems.

fn main() {
    println!(
        "esker-sql {}: the listener lands with the wire protocol (docs/plans/phase-6a.md unit 2)",
        env!("CARGO_PKG_VERSION")
    );
}
