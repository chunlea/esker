//! **The acceptance tests for `debts-v1.1.md` #49 option (b), written red and left red.**
//!
//! `docs/plans/debt-49-catalog-cache.md` is the plan and
//! [ADR 0106](../../../docs/adr/0106-what-a-statement-reads-below-the-sql.md) is the decision it
//! waits on. These are the two assertions that have to turn green when it lands, written now so
//! that the target exists before the work does and cannot be chosen to fit it.
//!
//! **Both are `#[ignore]`d and both genuinely fail.** An ignored test that would have passed is
//! worse than no test, so each was run and read before it was committed:
//!
//! ```text
//! one_statement_reads_no_key_twice
//!   7 of this statement's 60 reads are repeats of a key it had already read at the same snapshot
//!   (16 x schema(t1,"esker"), and five each of the four tenant-wide reads), bound is 2
//!
//! a_repeated_statement_stops_tracking_the_catalog
//!   20 relations 4.09 ms, 100 relations 25.33 ms — **6.2x the time for 5x the catalog**, bound
//!   is 2x. Super-linear, and the control over the same two catalogs went 0.57 ms -> 3.38 ms.
//! ```
//!
//! **The second one passed on its first draft** and had to be tightened: a 50 ms absolute slack,
//! copied from an older test written at a different scale, swamped a 4.4x curve on numbers of two
//! and eight milliseconds. That is the failure this file's own header warns about, met while
//! writing it.
//!
//! **Delete the `#[ignore]` when option (b) lands.** Nothing else about them should need changing;
//! if one does, the change is not option (b).
//!
//! # Why these two and not a slope alone
//!
//! The obvious acceptance test — *"the same statement over a catalog of n and of 5n, and the ratio
//! must stop tracking n"* — is half of it, and on its own it is a **timing** assertion on a shared
//! box. The other half is deterministic and is the one that actually names the defect:
//! `pk_and_sequence_for` reads **one key sixteen times** and repeats **one five-read hydration
//! bundle five times** inside a single statement, at one snapshot, where the answer cannot have
//! changed. That is countable, it does not flake, and no catalog size is needed to see it.
//!
//! **And a read count alone would be green for the wrong reason.** A tenant-wide *scan* is one
//! read whatever it walks, so the number of reads barely moves with the catalog while the cost
//! moves linearly with it — measured on run 117's tap, `pk_and_sequence_for` went 1,432 ms to
//! 3,377 ms across one file as its catalog grew, 2.36x, which is the same 2.3x r1 measured between
//! two files. So the count catches the repetition and the clock catches the size, and neither
//! catches the other.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// `ActiveRecord`'s `pk_and_sequence_for`, verbatim from `postgresql/schema_statements.rb:382`.
const PK_AND_SEQUENCE_FOR: &str = "SELECT attr.attname, nsp.nspname, seq.relname \
     FROM pg_class seq, pg_attribute attr, pg_depend dep, pg_constraint cons, pg_namespace nsp \
     WHERE seq.oid = dep.objid AND seq.relkind = 'S' \
       AND attr.attrelid = dep.refobjid AND attr.attnum = dep.refobjsubid \
       AND attr.attrelid = cons.conrelid AND attr.attnum = cons.conkey[1] \
       AND seq.relnamespace = nsp.oid AND cons.contype = 'p' \
       AND dep.classid = 'pg_class'::regclass \
       AND dep.refobjid = '\"pk0\"'::regclass";

/// A statement whose cost really is the catalog's size: the control, so that a slow box moves both
/// numbers and the ratio still means what it says. The idiom `pk_and_sequence_cost.rs` established.
const CONTROL: &str = "SELECT count(*) FROM pg_class";

fn grow_to(node: &mut parity::Node, target: usize, made: &mut usize) {
    while *made < target {
        node.run(&format!(
            "CREATE TABLE pk{made} (id bigserial primary key, a int8, b text)"
        ))
        .unwrap();
        *made += 1;
    }
}

fn elapsed(node: &mut parity::Node, sql: &str) -> Duration {
    let start = Instant::now();
    node.rows(sql);
    start.elapsed()
}

/// **No key is read more than twice in one statement**, two being the number of catalog views a
/// statement opens ([ADR 0105](../../../docs/adr/0105-a-catalog-read-never-waits.md) counts the
/// same two from the other side).
///
/// **Red today at 16**: `schema(t1,"esker")` is read sixteen times inside one
/// `pk_and_sequence_for`, at one snapshot, and the tenant-wide scans repeat five times each.
/// `catalog::Catalog` already holds a version-validated cache and
/// `catalog::schema_exists`/`pg_relations::Relations::read` go around it, which is the whole of
/// option (b).
///
/// **Needs `ESKER_STMT_STATS=1 ESKER_STMT_STATS_TRACE=1`**, and says so by failing rather than by
/// passing on an empty trace — a test whose instrument is off must not be a green tick.
#[test]
#[ignore = "red until debts-v1.1.md #49 option (b) lands; see docs/plans/debt-49-catalog-cache.md"]
fn one_statement_reads_no_key_twice() {
    /// One per catalog view a statement opens, and no more.
    const BOUND: usize = 2;

    assert!(
        esker_sql::stmt_stats::tracing_reads(),
        "this test reads the instrument's trace: run it with ESKER_STMT_STATS=1 \
         ESKER_STMT_STATS_TRACE=1, or it cannot say anything"
    );
    let mut node = parity::Node::new(&[
        "CREATE TABLE pk0 (id bigserial primary key, a int8, b text)",
        "INSERT INTO pk0 (a, b) VALUES (1, 'x')",
    ]);
    esker_sql::stmt_stats::clear_trace();
    assert_eq!(
        node.rows(PK_AND_SEQUENCE_FOR),
        [["id", "public", "pk0_id_seq"]],
        "the answer first, so a statement that got cheap by getting wrong fails here"
    );
    let trace = esker_sql::stmt_stats::last_trace();
    assert!(
        !trace.is_empty(),
        "the trace is empty, so the statement was never instrumented and the bound below would \
         pass by not looking"
    );
    let mut times: BTreeMap<&str, usize> = BTreeMap::new();
    for line in &trace {
        *times.entry(line.as_str()).or_default() += 1;
    }
    let worst: Vec<_> = times
        .iter()
        .filter(|(_, times)| **times > BOUND)
        .map(|(line, times)| format!("{times} x {line}"))
        .collect();
    assert!(
        worst.is_empty(),
        "{} of this statement's {} reads are repeats of a key it had already read at the same \
         snapshot, where the answer cannot have changed:\n    {}",
        worst.len(),
        trace.len(),
        worst.join("\n    ")
    );
}

/// **A statement repeated at an unchanged catalog version stops tracking the catalog's size.**
///
/// The slope acceptance test the plan asks for, and it is deliberately about the **second** run:
/// the first fills whatever cache option (b) routes to, and what has to become flat is every run
/// after it. Today both runs pay five whole-catalog loads, so the second is as linear as the
/// first.
///
/// **Red today at 6.2x for 5x the catalog** — 4.09 ms at twenty relations, 25.33 ms at a hundred,
/// against a bound of 2x. Slightly *super*-linear, which the census explains: five whole-catalog
/// loads each walking a catalog that is itself five times longer. The bound is loose on
/// purpose — this is a clock on a shared box — and it is still far under a linear curve, which is
/// the only thing it has to separate. `pk_and_sequence_cost.rs` asserts a *different and looser*
/// property (`big < small * 4` for **twice** the catalog, which permits quadratic); it was written
/// to catch a cross-product blow-up and it passes today. This one is about the slope that survived
/// it.
///
/// **The control is a statement that must grow with the catalog**, so a slow container moves both
/// numbers and the comparison still says what it says.
#[test]
#[ignore = "red until debts-v1.1.md #49 option (b) lands; see docs/plans/debt-49-catalog-cache.md"]
fn a_repeated_statement_stops_tracking_the_catalog() {
    /// What five times the catalog may cost, once the statement has been asked before.
    const BOUND: u32 = 2;
    /// Absolute room for a noisy box, kept **small against the numbers this actually measures**
    /// (4 ms and 25 ms). A 50 ms slack was the first attempt, copied from an older test written at
    /// a different scale, and it made this pass on a 4.4x curve — an ignored test that would have
    /// passed, which is worse than no test.
    const SLACK: Duration = Duration::from_millis(2);

    let mut node = parity::Node::new(&[]);
    let mut made = 0;
    grow_to(&mut node, 20, &mut made);
    assert_eq!(
        node.rows(PK_AND_SEQUENCE_FOR),
        [["id", "public", "pk0_id_seq"]]
    );
    let small_control = elapsed(&mut node, CONTROL);
    // The **second** run, at a version nothing has moved: this is the one that has to be flat.
    let small = elapsed(&mut node, PK_AND_SEQUENCE_FOR);

    grow_to(&mut node, 100, &mut made);
    assert_eq!(
        node.rows(PK_AND_SEQUENCE_FOR),
        [["id", "public", "pk0_id_seq"]],
        "the same one row over five times the catalog"
    );
    let big_control = elapsed(&mut node, CONTROL);
    let big = elapsed(&mut node, PK_AND_SEQUENCE_FOR);

    // **Printed whether it passes or fails.** A ratio assertion that only speaks when it breaks
    // leaves the next reader guessing how much room there was.
    println!(
        "20 relations {small:?}, 100 relations {big:?}; control {small_control:?} -> {big_control:?}"
    );
    assert!(
        big < small * BOUND + SLACK,
        "five times the catalog took {big:?} where a fifth of it took {small:?}, on the second \
         run at an unchanged version — the statement is still reading the whole catalog once per \
         relation in its FROM list. The control over the same two catalogs went {small_control:?} \
         -> {big_control:?}, so the machine is not what changed."
    );
}
