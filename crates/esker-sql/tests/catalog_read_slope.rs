//! **The acceptance tests for `debts-v1.1.md` #49 option (b)** — written red before the work and
//! read again after it.
//!
//! `docs/plans/debt-49-catalog-cache.md` is the plan and
//! [ADR 0106](../../../docs/adr/0106-what-a-statement-reads-below-the-sql.md) is the decision.
//! Both tests below were run and read while they were still `#[ignore]`d, so that the target
//! existed before the work did and could not be chosen to fit it:
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
//! # What landing option (b) did to each of them
//!
//! **The first is green**, and so is [`a_repeated_statement_reads_only_the_version_keys`], which
//! was added when the second turned out to be measuring something else.
//!
//! **The second is still red, and it is `debts-v1.1.md` #54's test now, not #49's** — the user's
//! ruling of 2026-09-10: keep it, `#[ignore]`d, with the reason below, because it was not deleted
//! or weakened and what it measures is a real debt. Its scenario is right and its
//! instrument is wrong: it is a *clock* on the in-process node, where a KV read costs nothing, so
//! what it times is the work that is left after the reads are gone. Measured after option (b)
//! landed, on the second run of `pk_and_sequence_for` at an unchanged version:
//!
//! ```text
//!  20 relations   4 reads   1.96 ms          <- the four version counters, and nothing else
//! 100 relations   4 reads  14.38 ms
//!
//! and where those milliseconds are, per statement, at 20 -> 100 relations:
//!     SELECT count(*) FROM pg_class                            164 us ->   639 us   3.9x
//!     SELECT count(*) FROM pg_attribute                        316 us ->   737 us   2.3x
//!     SELECT count(*) FROM pg_class, pg_namespace              371 us ->  1355 us   3.7x
//!     SELECT count(*) FROM pg_class seq, pg_depend dep
//!                    WHERE seq.oid = dep.objid               1552 us -> 24646 us  15.9x
//! ```
//!
//! **The read count is flat and the clock is not**, which is the mirror image of the warning in
//! the plan: a join of two *computed* catalog views is a cross product, and five times the catalog
//! is twenty-five times the pairs. That is a planner defect, it is not in ADR 0106's option space,
//! and no amount of caching reads touches it. So this test waits on **#54** rather than on #49 —
//! and it is the one place the in-process node measures *better* than the real topology, because a
//! KV read here is a `BTreeMap` lookup and what the clock sees is the planner and nothing else.
//!
//! # Why a count is the acceptance test and a clock is not
//!
//! On the real topology a KV read is **232 µs** ([ADR 0102](../../../docs/adr/0102-the-catalogs-read-path.md)),
//! so the read count *is* the cost — run 117 put `pk_and_sequence_for` at 35 round trips and p50
//! 2,382 ms. On the in-process node the same read is a `BTreeMap` lookup, so the clock here cannot
//! see the change at all and times the executor instead. The count is also deterministic, which
//! the clock on a shared box is not.
//!
//! What a count alone would miss is repetition *within* one statement, which is what the first
//! test bounds, and the size of a scan — which is why the third test asserts an absolute number
//! at two catalog sizes rather than a ratio: **a tenant-wide scan is one read whatever it walks**,
//! so a ratio over sizes was green before the work and says nothing.

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

/// **The two computed catalog views #54 was about**, joined the way `pk_and_sequence_for` joins
/// its five: a comma in the `FROM` and the equality in the `WHERE`. The shortest statement whose
/// `EXPLAIN` says whether that equality became a join condition.
const JOIN_OVER_TWO_VIEWS: &str =
    "SELECT count(*) FROM pg_class seq, pg_depend dep WHERE seq.oid = dep.objid";

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
/// **Turns the instrument on itself** (`stmt_stats::trace_every_read`) and still asserts the trace
/// is not empty — a test whose instrument is off must not be a green tick.
#[test]
fn one_statement_reads_no_key_twice() {
    /// One per catalog view a statement opens, and no more.
    const BOUND: usize = 2;

    // **Turned on here rather than by the environment**, because this is the acceptance test and
    // one that needs two variables set is one the gate never runs (`stmt_stats::trace_every_read`).
    esker_sql::stmt_stats::trace_every_read();
    assert!(
        esker_sql::stmt_stats::tracing_reads(),
        "this test reads the instrument's trace and could not turn it on"
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
/// **It asserts read counts and a plan; the clock is printed.** It used to assert
/// `big < small * 2 + 2ms` over the wall clock, and on 2026-09-11 that went red on a gate whose
/// own diff did not touch this crate — 3.1x under a load of 8 to 11 (#59). A gate cannot carry a
/// wall-clock assertion, so the two properties it was really about are asserted directly:
///
/// * **the reads are four at either catalog size**, which is #49 (b)'s acceptance and is a count;
/// * **the comma join's equality reaches the join**, which is #54's, and is a *plan*.
///
/// The second is the one that needed saying out loud. A cross product reads no more than a lookup
/// does — a tenant-wide scan is one read whatever it walks — so the read count was flat *before*
/// #54 was fixed, and a count alone is a test that cannot fail for the debt this is named after.
/// The plan can: before the fix the equality sat in a `Filter` above the loop and every pair was
/// built, and the bare product's plan (no condition at all) is asserted beside it so that the
/// string being looked for is known to discriminate.
///
/// The clock stays in the output because it is what a reader wants when the counts look right and
/// the statement still feels slow, and `tests/catalog_join_slope.rs` is the measurement at three
/// sizes that keeps the numbers.
///
/// # Green since 2026-09-11, and it spent two debts red
///
/// It was `#[ignore]`d twice over: first as #49's, then retargeted at **#54** by the user's ruling
/// of 2026-09-10 — *kept red rather than rewritten*, which is the ruling that made it the thing
/// #54 had to satisfy instead of a number #54 could be fitted to.
///
/// What closed it was not what its own `#[ignore]` predicted. The residue really was a cross
/// product, and the fix was not a hash on the key: a comma join's `WHERE` never became a join
/// condition, so `outer x inner` joined rows were built and then filtered. Measured at
/// **1.19 ms -> 2.74 ms** for five times the catalog, where it had been 4.09 -> 25.33.
/// `tests/catalog_join_slope.rs` carries the measurement at three sizes.
#[test]
fn a_repeated_statement_stops_tracking_the_catalog() {
    /// What the second run may read at either catalog size: two catalog views, two counters each.
    const BOUND: usize = 4;

    esker_sql::stmt_stats::trace_every_read();
    assert!(
        esker_sql::stmt_stats::tracing_reads(),
        "this test reads the instrument's trace and could not turn it on"
    );
    let mut node = parity::Node::new(&[]);
    let mut made = 0;
    let mut measured = Vec::new();
    for size in [20usize, 100] {
        grow_to(&mut node, size, &mut made);
        assert_eq!(
            node.rows(PK_AND_SEQUENCE_FOR),
            [["id", "public", "pk0_id_seq"]],
            "the same one row over a catalog of {size}"
        );
        let control = elapsed(&mut node, CONTROL);
        esker_sql::stmt_stats::clear_trace();
        // The **second** run, at a version nothing has moved: this is the one that has to be flat.
        let took = elapsed(&mut node, PK_AND_SEQUENCE_FOR);
        let reads = esker_sql::stmt_stats::last_trace();
        assert!(
            !reads.is_empty(),
            "the trace is empty over {size} relations, so the statement was never instrumented \
             and the bound below would pass by not looking"
        );
        assert!(
            reads.len() <= BOUND,
            "over {size} relations the second run read {} keys, not {BOUND}:\n    {}",
            reads.len(),
            reads.join("\n    ")
        );
        measured.push((size, reads.len(), took, control));
    }

    // **The plan is what pins #54, and the count above cannot.** A cross product reads no more
    // than a lookup does — a tenant-wide scan is one read whatever it walks — so the read count
    // was flat *before* the fix as well, and a count ratio alone would be a test that cannot fail
    // for the debt it is named after. What the fix moved is where the equality goes: a comma
    // join's `WHERE` becomes a **join condition** instead of a filter over the pairs, and
    // `EXPLAIN` is where this node writes that down (`tests/catalog_join_slope.rs` has the three
    // sizes and both plans).
    let plan = node
        .rows(&format!("EXPLAIN {JOIN_OVER_TWO_VIEWS}"))
        .into_iter()
        .map(|row| row.join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        plan.contains("Join Filter: (oid = objid)"),
        "the comma join's equality never reached the join, so the loop builds every pair and \
         filters afterwards — which is #54, and it is quadratic in the catalog:\n{plan}"
    );
    // **The control for the string**, so this cannot pass by looking for something every plan
    // says: the same two views with **no** condition are an honest cross product, and that plan
    // has no `Join Filter` in it at all.
    let product = node
        .rows("EXPLAIN SELECT count(*) FROM pg_class seq, pg_depend dep")
        .into_iter()
        .map(|row| row.join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !product.contains("Join Filter"),
        "a product with no condition named a join filter, so the assertion above proves \
         nothing:\n{product}"
    );

    // **Printed, not asserted.** A wall clock on a shared box is not a gate: this assertion was
    // `big < small * 2 + 2ms` and it went red on a gate whose own diff did not touch this crate,
    // at 3.1x under a load of 8-11 (#59, 2026-09-11). The numbers stay because they are what a
    // reader wants when the counts look right and the statement still feels slow.
    for (size, reads, took, control) in &measured {
        println!("{size:4} relations: {reads} reads, {took:?}; control {control:?}");
    }
}

/// **A statement asked a second time at an unchanged version reads only the version counters** —
/// whatever the catalog holds.
///
/// The acceptance test for option (b), and the one that translates: on the real topology a read is
/// a round trip, so this is `pk_and_sequence_for`'s **35 round trips** going to four.
///
/// **Red before option (b) at 60 reads** (`docs/bench/statement-reads.md`, the committed census),
/// green after at 4 — and 4 at both catalog sizes, which is the half a ratio cannot state: a
/// tenant-wide scan is one read whatever it walks, so "the count does not grow with the catalog"
/// was true before the work as well. What was not true is the number.
///
/// The four are two catalog views' two counters each ([ADR 0105](../../../docs/adr/0105-a-catalog-read-never-waits.md)
/// counts the same two, and `debts-v1.1.md` #50 is the one about making them one).
///
/// *The counterfactual*: take the memo off `View::relations` — the whole of the bundle's cache —
/// and this is **red at 9 reads over twenty relations**, with the five whole-catalog loads back
/// (`scan name(t1)…` and the four beside it). `one_statement_reads_no_key_twice` goes red with
/// it, which is the two of them agreeing about one line.
#[test]
fn a_repeated_statement_reads_only_the_version_keys() {
    /// Two catalog views, two counters each, and nothing else.
    const BOUND: usize = 4;

    // **Turned on here rather than by the environment**, because this is the acceptance test and
    // one that needs two variables set is one the gate never runs (`stmt_stats::trace_every_read`).
    esker_sql::stmt_stats::trace_every_read();
    assert!(
        esker_sql::stmt_stats::tracing_reads(),
        "this test reads the instrument's trace and could not turn it on"
    );
    let mut node = parity::Node::new(&[]);
    let mut made = 0;
    for size in [20usize, 100] {
        grow_to(&mut node, size, &mut made);
        // The **first** run fills; what has to be flat is every run after it.
        assert_eq!(
            node.rows(PK_AND_SEQUENCE_FOR),
            [["id", "public", "pk0_id_seq"]],
            "the answer first, so a statement that got cheap by getting wrong fails here"
        );
        esker_sql::stmt_stats::clear_trace();
        assert_eq!(
            node.rows(PK_AND_SEQUENCE_FOR),
            [["id", "public", "pk0_id_seq"]],
            "the same one row over a catalog of {size}"
        );
        let trace = esker_sql::stmt_stats::last_trace();
        assert!(
            !trace.is_empty(),
            "the trace is empty, so the statement was never instrumented and the bound below \
             would pass by not looking"
        );
        assert!(
            trace.len() <= BOUND,
            "over {size} relations the second run of this statement read {} keys, not {BOUND}:\n    {}",
            trace.len(),
            trace.join("\n    ")
        );
    }
}
