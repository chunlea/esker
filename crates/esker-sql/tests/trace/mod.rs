//! One tracing subscriber for this crate's integration tests.
//!
//! # Why this exists
//!
//! `esker-store`'s tests have had a `trace()` of their own since 4c; this crate's had none, and
//! the cost was paid on 2026-09-11: `joint_gate`'s columnar differential produced a disagreement
//! that only a log could explain, and six runs with `RUST_LOG=esker_store::columnar=debug`
//! produced **not one line** — there was nothing installed to print them. An hour went into
//! reading code for an answer a log line already knew.
//!
//! # What it does, and what it deliberately does not
//!
//! It reads `RUST_LOG` and nothing else, so a test run says nothing until someone asks. `try_init`
//! rather than `init`: a subscriber is global and per **process**, integration tests share one
//! process across the binary's tests, and a second `init` panics — so this is safe to call from
//! every harness constructor, which is exactly how it is called.
//!
//! It writes through `with_test_writer`, so the output belongs to the test that produced it and a
//! passing test stays quiet under `cargo test` even when the filter is on.

/// Installs the subscriber, once per process. Cheap and idempotent after the first call.
///
/// `pub(crate)` and not `pub`: this is a private module of a test binary, so a `pub` here is
/// unreachable and `-D warnings` says so.
pub(crate) fn on() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}
