//! The routing rule, the session override, and what `EXPLAIN` says about both.
//!
//! `docs/plans/phase-10-routing.md` U2 and U3. Two halves:
//!
//! * **`esker.engine`'s surface**, replayed against `tests/corpus/pg19_routing_engine.txt` — what a
//!   real PostgreSQL 19 answered for every statement in it — with every divergence listed and its
//!   reason, checked in both directions as [ADR 0031](../../docs/adr/0031-rails-compatibility-is-measured.md)
//!   requires. A gap cannot be absorbed silently and neither can closing one.
//! * **the rule itself**, against a fragment source that answers from a script. The decision is a
//!   pure function tested as a table in `plan::routing`'s own unit tests; what is tested here is
//!   the half that needs a plan — which shapes are expressible, what the substitution produces, and
//!   that a refusal is answered by the rows in the same snapshot.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use esker_client::wire::{Epoch, Peer, PeerRole};
use esker_proto::fragment::ScanStats;
use esker_proto::fragment::result::{Body, Group, Partial, Value, ValueType};
use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::fragment::{Answer, FragmentSource, RefusalReason, Shard};
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};

// ---------------------------------------------------------------------------------------------
// The session override, against what PostgreSQL 19 actually did
// ---------------------------------------------------------------------------------------------

/// Statements a real PostgreSQL answers differently, each with its reason.
///
/// All five are in the **safe** direction: this node refuses something PostgreSQL accepts and does
/// nothing with. None of them is this node answering a question PostgreSQL answers, differently.
///
/// There were six. `SET engine = 'row'` left the list when the `SET`-parameters unit made an
/// unknown name `42704` from `SET` as well as from `SHOW` and `RESET`, which is what PostgreSQL
/// answers for it — so that line agrees now and its row is deleted (ADR 0031 rule 2).
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "SET esker.engine = 'sideways'",
        "PostgreSQL validates no custom parameter's value, ever, so it stores `sideways` and \
         nothing reads it. Here the parameter decides which engine a query runs on, and \
         `crate::parameter`'s whole rule is that a value this node will not act on is refused by \
         name rather than accepted and ignored. `22023` with the three spellings as a HINT, which \
         is PostgreSQL's own shape for an enum it does know.",
    ),
    (
        "BEGIN; SET LOCAL esker.engine = 'columnar'; SHOW esker.engine",
        "`SET LOCAL` is undone when the transaction ends, whichever way it ends, and this node \
         keeps that per-block undo only for `esker.read_as_of`. Refused by name rather than \
         silently promoted to a session-wide `SET`, which would outlive the block — the same \
         answer every other parameter in the table gets, and not specific to this one.",
    ),
    (
        "SET nonesker.engine = 'row'",
        "PostgreSQL accepts **any** namespaced name and stores it, validating nothing — a custom \
         GUC is whatever a session says it is. This node has a list of the parameters it means, \
         and a name outside it is `42704` from all three of `SET`, `SHOW` and `RESET`: the \
         `SET`-parameters unit made the three agree, where `SET` alone used to answer `0A000`. \
         The divergence is the refusal, not its code — a namespaced name is one PostgreSQL would \
         have accepted and this node will not act on.",
    ),
    (
        "RESET esker.never_existed",
        "`RESET` validates no name at all on a real server — the asymmetry ADR 0022's phase-8 \
         amendment found for storage parameters, holding here too. This node answers `42704`, \
         which is the same code PostgreSQL uses for the `SET` of the same name and is the more \
         useful answer: a `RESET` of a name nobody ever set is a typo, and swallowing it silently \
         is how a session ends up believing it cleared something.",
    ),
    (
        "SET esker.engine = 'row', 'columnar'",
        "Both servers refuse it and both say `22023`; only the sentence differs. PostgreSQL says \
         `SET esker.engine takes only one argument`, because it knows a custom GUC is scalar. \
         This node has one path for a parameter that takes a list (`search_path` really does), so \
         a list arrives as one joined value and is refused as a value the enum does not admit — \
         which names the three it does.",
    ),
];

#[test]
fn the_session_override_answers_the_way_postgresql_19_does() {
    let mut checked = 0;
    let mut diverged = Vec::new();

    for (line_number, script, expected) in corpus() {
        let ours = answer(&script);
        checked += 1;

        let listed = DIVERGENCES.iter().find(|(sql, _)| script == *sql);
        if ours == expected {
            assert!(
                listed.is_none(),
                "line {line_number}: `{script}` is listed as a divergence and now agrees with \
                 PostgreSQL ({ours}). Delete its row in DIVERGENCES."
            );
            continue;
        }
        match listed {
            Some(_) => diverged.push(script),
            None => panic!(
                "line {line_number}: `{script}`\n  PostgreSQL 19: {expected}\n  here:          \
                 {ours}\nEither this is a bug or it is a divergence; if it is a divergence, it \
                 belongs in DIVERGENCES with its reason."
            ),
        }
    }

    assert!(
        checked >= 16,
        "only {checked} statements ran; the corpus did not load"
    );
    assert_eq!(
        diverged.len(),
        DIVERGENCES.len(),
        "every listed divergence must be exercised by the corpus; these diverged: {diverged:?}"
    );
}

/// Statements about a *plan* that a real PostgreSQL answers differently.
///
/// All four are `0A000` here and accepted there, which is contract C2's shape: a statement this
/// node parses and cannot honour names the feature rather than being approximated. What
/// diverges is never the *answer to a query* — only what a plan is allowed to say about itself.
const EXPLAIN_DIVERGENCES: &[(&str, &str)] = &[
    (
        "EXPLAIN ANALYZE INSERT INTO t VALUES (9, 'i', 90)",
        "`ANALYZE` runs the statement, which is exactly what it means on a real server — the row \
         really is inserted there, inside the transaction the EXPLAIN runs in. This node executes \
         `ANALYZE` for a `SELECT` only, and refuses it by name for anything that writes: the \
         alternative is an `EXPLAIN` that writes, and a user who did not expect that has already \
         written.",
    ),
    (
        "EXPLAIN ANALYZE UPDATE t SET amount = 1",
        "The same rule, and the same reason.",
    ),
    (
        "EXPLAIN ANALYZE DELETE FROM t",
        "The same rule, and the same reason.",
    ),
    // **`EXPLAIN VERBOSE` and `EXPLAIN (FORMAT JSON)` were both here and neither is any more**,
    // and the two entries' own arguments are worth answering rather than deleting in silence.
    //
    // `VERBOSE`'s said that honouring it "would mean printing the same plan and calling it
    // verbose", and preferred a refusal. That weighs the wrong two things: the choice is not
    // between an honest refusal and a dishonest plan — it is between refusing a statement
    // **PostgreSQL runs** and answering it with the detail this node has. `explain_test.rb` sends
    // `VERBOSE` in a list with `ANALYZE`, so the refusal cost three of that file's five tests, and
    // it cost them at the *parser*: nothing downstream ever ran. Printing the same plan is not a
    // wrong answer, because what a plan contains is already a declared divergence on every line;
    // there is simply no extra detail to show, and saying so by showing none is truthful.
    //
    // `(FORMAT JSON)`'s was sharper and was right as far as it went: `FORMAT` changes the *shape* a
    // client parses, so answering it with **text** would be a wrong answer rather than a plainer
    // one — and the entry went further, arguing that "producing JSON with our fields in it would
    // look like PostgreSQL's schema and not be it". What it did not consider is the third option
    // that was taken instead: a JSON document whose keys are the ones this node can honestly fill
    // and whose absent keys — every cost, every buffer count — are absent rather than zeroed, in a
    // column declared `json` (114) as `\gdesc` says. A client that finds no `Total Cost` is being
    // told the truth; one that reads `"Total Cost": 0.00` is not, and one that is handed text
    // through a `json` OID cannot read it at all. That last is what `connection_test.rb`'s
    // `test_statement_key_is_logged` does — `column_types["QUERY PLAN"].deserialize` — which is why
    // the wire type is asserted beside the value in `tests/explain_options.rs`.
];

/// The plan surface, replayed: which `EXPLAIN` spellings both servers accept.
///
/// What each server *prints* is not compared and is not comparable — PostgreSQL has no second
/// engine to name, so there is nothing for the engine line to diverge from. The corpus header
/// carries PostgreSQL's output verbatim so the divergence table has both sides, which is what
/// ADR 0031 asks for where there is no oracle.
#[test]
fn the_explain_surface_answers_the_way_postgresql_19_does() {
    let mut diverged = Vec::new();
    let mut checked = 0;
    for (line_number, script, expected) in explain_corpus() {
        let ours = answer_over_t(&script);
        checked += 1;
        let listed = EXPLAIN_DIVERGENCES.iter().find(|(sql, _)| script == *sql);
        if ours == expected {
            assert!(
                listed.is_none(),
                "line {line_number}: `{script}` is listed as a divergence and now agrees \
                 ({ours}). Delete its row in EXPLAIN_DIVERGENCES."
            );
            continue;
        }
        match listed {
            Some(_) => diverged.push(script),
            None => panic!(
                "line {line_number}: `{script}`\n  PostgreSQL 19: {expected}\n  here:          \
                 {ours}"
            ),
        }
    }
    assert!(checked >= 10, "only {checked} statements ran");
    assert_eq!(
        diverged.len(),
        EXPLAIN_DIVERGENCES.len(),
        "every listed divergence must be exercised; these diverged: {diverged:?}"
    );
}

/// The value is folded here and is not on a real server, which the corpus's `ok`/`!code` shape
/// cannot see — so it is asserted directly rather than left to a format that would miss it.
///
/// `SET ESKER.ENGINE = 'AUTO'` then `SHOW esker.engine` answers `AUTO` on PostgreSQL 19 and `auto`
/// here. The name is case-insensitive on both.
#[test]
fn the_name_is_folded_on_both_sides_and_the_value_only_here() {
    let mut node = node();
    run(&mut node, "SET ESKER.ENGINE = 'AUTO'").unwrap();
    assert_eq!(show(&mut node, "SHOW esker.engine"), "auto");
    run(&mut node, "SET esker.engine = 'CoLuMnAr'").unwrap();
    assert_eq!(show(&mut node, "SHOW Esker.Engine"), "columnar");
}

/// The boot value is `auto` and `RESET` goes back to it.
///
/// The other half of the declared divergence: PostgreSQL answers `42704` for `SHOW esker.engine`
/// until the first `SET` of the session, and the empty string after a `RESET`. This node has a
/// real default, so it has a real value to report at both moments.
#[test]
fn the_override_boots_at_auto_and_resets_to_it() {
    let mut node = node();
    assert_eq!(show(&mut node, "SHOW esker.engine"), "auto");
    run(&mut node, "SET esker.engine = 'row'").unwrap();
    assert_eq!(show(&mut node, "SHOW esker.engine"), "row");
    run(&mut node, "RESET esker.engine").unwrap();
    assert_eq!(show(&mut node, "SHOW esker.engine"), "auto");
}

/// A value outside the three is refused **by name**, with the three in the `HINT` — the shape a
/// real server uses for an enum it knows, which is what makes the refusal readable rather than
/// merely correct.
#[test]
fn a_value_this_node_will_not_act_on_is_refused_by_name() {
    let mut node = node();
    let error = run(&mut node, "SET esker.engine = 'sideways'").unwrap_err();
    assert_eq!(error.sqlstate(), "22023");
    let message = error.to_string();
    assert!(message.contains("esker.engine"), "{message}");
    assert!(message.contains("sideways"), "{message}");
    // And the setting did not move.
    assert_eq!(show(&mut node, "SHOW esker.engine"), "auto");
}

// ---------------------------------------------------------------------------------------------
// The rule, over a plan
// ---------------------------------------------------------------------------------------------

/// `count(*)` over a table with a columnar copy is answered by the fragments.
#[test]
fn a_count_star_is_answered_by_the_fragments() {
    let mut node = node_with(script(&[groups(&[(&[], &[Partial::Count(7)])])]));
    ready(&mut node);
    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["7"]]);
    assert!(explain(&mut node, "SELECT count(*) FROM t").contains("Columnar Aggregate on t"));
}

/// Two regions, two fragments, one answer: the counts add.
///
/// The whole of the two-level aggregate in its smallest form, and the reason the shard list has to
/// be complete — an answer built from one of two regions is a *number*, and a number cannot be
/// recognised as half of one.
#[test]
fn counts_from_two_regions_are_added() {
    let source = script(&[
        groups(&[(&[], &[Partial::Count(4)])]),
        groups(&[(&[], &[Partial::Count(3)])]),
    ]);
    source.split_at(b"t\x7f");
    let mut node = node_with(source);
    ready(&mut node);
    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["7"]]);
}

/// **A source that cannot scope a fragment to one region does not get more than one.**
///
/// The guard that stands until the columnar copy is region-scoped. Measured on a real four-store
/// cluster on 2026-09-05: four regions, a learner on each, every fragment answered, and every
/// aggregate came back four times its true value (`docs/bench/mpp-baseline.md` §10).
///
/// **It asserts the reason, not the answer.** The row engine is correct, so *any* fallback agrees
/// with it — including one that happened for an unrelated reason. What has to be true is that this
/// rule fired, and `EXPLAIN` is the only place that says so.
///
/// The contrast with `counts_from_two_regions_are_added` directly above is the whole point: the
/// same two regions, the same fold, and the only difference is what the source declares about
/// itself. Multi-shard folding is correct; one store's columnar copy is not.
#[test]
fn a_source_that_is_not_region_scoped_keeps_a_split_table_on_the_rows() {
    let source = script(&[
        groups(&[(&[], &[Partial::Count(4)])]),
        groups(&[(&[], &[Partial::Count(3)])]),
    ]);
    source.split_at(b"t\x7f");
    source.is_not_region_scoped();
    let mut node = node_with(source);
    ready(&mut node);
    let plan = explain(&mut node, "SELECT count(*) FROM t");
    assert!(
        plan.contains("Engine: rows"),
        "a split table was routed to a source that reads across regions:\n{plan}"
    );
    assert!(
        plan.contains("not region-scoped"),
        "the plan fell back for some other reason than the guard:\n{plan}"
    );
}

/// A refusal is answered by the rows, silently to the client and visibly in `EXPLAIN`.
///
/// The assertion that matters is the first one: the client's answer is what the row engine would
/// have said, which is what "a routing decision never changes an answer" means at the surface.
#[test]
fn a_refusal_falls_back_to_the_rows_in_the_same_snapshot() {
    for reason in [
        RefusalReason::TooFarBehind,
        RefusalReason::Unsupported,
        RefusalReason::NotColumnar,
    ] {
        let mut node = node_with(refusing(reason));
        ready(&mut node);
        run(&mut node, "INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20)").unwrap();
        assert_eq!(
            rows(&mut node, "SELECT count(*) FROM t"),
            vec![vec!["2"]],
            "{reason:?} changed the answer"
        );
        // A plain `EXPLAIN` runs nothing, so it reports what was *planned* — columnar — and does
        // not pretend to know how the fragments went. `EXPLAIN ANALYZE` runs them, and that is
        // where the refusal appears.
        let planned = explain(&mut node, "SELECT count(*) FROM t");
        assert!(
            planned.contains("Engine: columnar"),
            "{reason:?}: {planned}"
        );

        let ran = explain_analyze(&mut node, "SELECT count(*) FROM t");
        assert!(ran.contains("Engine: rows"), "{reason:?}: {ran}");
        assert!(ran.contains("columnar refused"), "{reason:?}: {ran}");
        // The row plan it fell back to is printed beneath it, because that is the plan that ran.
        assert!(ran.contains("Seq Scan on t"), "{reason:?}: {ran}");
    }
}

/// `SET esker.engine = 'row'` stops the fragments being asked at all.
#[test]
fn the_row_override_asks_nobody() {
    let source = script(&[groups(&[(&[], &[Partial::Count(99)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10)").unwrap();
    run(&mut node, "SET esker.engine = 'row'").unwrap();

    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["1"]]);
    assert_eq!(*asked.lock().unwrap(), 0, "a fragment went out anyway");
    let plan = explain(&mut node, "SELECT count(*) FROM t");
    assert!(plan.contains("Engine: rows"), "{plan}");
    assert!(plan.contains("esker.engine = 'row'"), "{plan}");
}

/// A table nobody asked for a columnar copy of is never routed, whatever the session says.
#[test]
fn a_table_with_no_columnar_copy_is_never_routed() {
    let source = script(&[groups(&[(&[], &[Partial::Count(99)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    run(
        &mut node,
        "CREATE TABLE t (id int8 PRIMARY KEY, name text, amount int8)",
    )
    .unwrap();
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10)").unwrap();
    run(&mut node, "SET esker.engine = 'columnar'").unwrap();

    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["1"]]);
    assert_eq!(*asked.lock().unwrap(), 0);
    // **And the plan says nothing about an engine**, which is the right silence: a table with no
    // columnar copy has one engine, so there was no decision to report. A line on every plan of
    // every ordinary table is noise, and noise is what stops the line being read when it matters.
    let plan = explain(&mut node, "SELECT count(*) FROM t");
    assert!(!plan.contains("Engine:"), "{plan}");
}

/// A transaction that has written reads its own writes, which a learner has never seen.
///
/// **The one rule here that is about a wrong answer rather than a slow one**, so it holds under the
/// `'columnar'` override: the override moves the threshold, never the correctness rules.
#[test]
fn a_transaction_that_has_written_is_never_routed() {
    let source = script(&[groups(&[(&[], &[Partial::Count(99)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    run(&mut node, "SET esker.engine = 'columnar'").unwrap();

    run(&mut node, "BEGIN").unwrap();
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10)").unwrap();
    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["1"]]);
    run(&mut node, "COMMIT").unwrap();

    assert_eq!(
        *asked.lock().unwrap(),
        0,
        "a fragment was sent from a transaction holding uncommitted writes"
    );
}

/// A `GROUP BY` over a column is expressible; one over an expression is not, and says which.
#[test]
fn a_group_by_over_an_expression_is_refused_by_name() {
    let mut node = node_with(script(&[groups(&[(
        &[Value::Text("a".to_owned())],
        &[Partial::Count(1)],
    )])]));
    ready(&mut node);
    let plan = explain(&mut node, "SELECT name, count(*) FROM t GROUP BY name");
    assert!(plan.contains("Columnar Aggregate on t"), "{plan}");
    assert!(plan.contains("Group Key: name"), "{plan}");
}

/// A `DISTINCT` aggregate has no fragment spelling, and the plan says so rather than sending one.
#[test]
fn a_distinct_aggregate_is_refused_by_name() {
    let source = script(&[groups(&[(&[], &[Partial::Count(1)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    // The answer is the row engine's, and no fragment went out to produce it.
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10), (2, 'a', 20)").unwrap();
    assert_eq!(
        rows(&mut node, "SELECT count(DISTINCT name) FROM t"),
        vec![vec!["1"]]
    );
    assert_eq!(*asked.lock().unwrap(), 0);
    let plan = explain(&mut node, "SELECT count(DISTINCT name) FROM t");
    assert!(plan.contains("Engine: rows"), "{plan}");
    assert!(plan.contains("DISTINCT aggregate"), "{plan}");
}

/// A point read stays on rows even when the session asks for columns: a fragment cannot restrict
/// itself to a key range at all, so this is not a preference a session can express.
#[test]
fn a_point_read_stays_on_rows_under_the_override() {
    let source = script(&[groups(&[(&[], &[Partial::Count(99)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20)").unwrap();
    run(&mut node, "SET esker.engine = 'columnar'").unwrap();

    assert_eq!(
        rows(&mut node, "SELECT count(*) FROM t WHERE id = 1"),
        vec![vec!["1"]]
    );
    assert_eq!(*asked.lock().unwrap(), 0);
}

/// The ratio, at the surface: three columns of three is too wide, one of three is not.
#[test]
fn the_ratio_decides_between_two_queries_over_the_same_table() {
    let source = script(&[groups(&[(
        &[Value::Text("a".to_owned())],
        &[Partial::Count(1)],
    )])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10)").unwrap();

    // One column of three: columns win, and the fragment is asked.
    let narrow = explain(&mut node, "SELECT name, count(*) FROM t GROUP BY name");
    assert!(narrow.contains("Columnar Aggregate on t"), "{narrow}");
    assert!(narrow.contains("1 of 3 columns projected"), "{narrow}");

    // Every column of three: rows win, and nothing goes out. Asserted on the wire rather than on
    // the plan text, because the *behaviour* is the claim; U3 adds the sentence.
    let before = *asked.lock().unwrap();
    assert_eq!(
        rows(
            &mut node,
            "SELECT id, name, count(amount) FROM t GROUP BY id, name"
        ),
        vec![vec!["1", "a", "1"]]
    );
    assert_eq!(*asked.lock().unwrap(), before, "a wide query was routed");
    let wide = explain(
        &mut node,
        "SELECT id, name, count(amount) FROM t GROUP BY id, name",
    );
    assert!(wide.contains("Engine: rows"), "{wide}");
    assert!(wide.contains("3 of 3 columns projected"), "{wide}");
}

/// `EXPLAIN ANALYZE` carries what the scan cost, from the `ScanStats` the response has carried
/// since phase 8 — which is why it was carried then rather than added now.
#[test]
fn analyze_reports_what_the_fragments_cost() {
    let mut node = node_with(script(&[groups(&[(&[], &[Partial::Count(7)])])]));
    ready(&mut node);

    // Without `ANALYZE` nothing ran, so there is nothing to report and none is invented.
    let planned = explain(&mut node, "SELECT count(*) FROM t");
    assert!(
        planned.contains("Columnar Aggregate on t  (1 fragments)"),
        "{planned}"
    );
    assert!(!planned.contains("Stripes:"), "{planned}");

    let ran = explain_analyze(&mut node, "SELECT count(*) FROM t");
    assert!(ran.contains("Fragments: 1 asked, 1 answered"), "{ran}");
    assert!(
        ran.contains("Stripes: 2 of 4 read   Chunks: 1   Rows: 100 scanned, 10 matched"),
        "{ran}"
    );
    assert!(ran.contains("Aggregates: count(*)"), "{ran}");
}

/// `ANALYZE` **runs** the statement, so it stays refused for anything that writes: an
/// `EXPLAIN ANALYZE INSERT` that ran would be an insert.
#[test]
fn analyze_of_a_write_is_still_refused_by_name() {
    let mut node = node();
    run(&mut node, "CREATE TABLE t (id int8 PRIMARY KEY)").unwrap();
    let error = run(&mut node, "EXPLAIN ANALYZE INSERT INTO t VALUES (1)").unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert!(error.to_string().contains("ANALYZE"), "{error}");
    // And nothing was inserted.
    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["0"]]);
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// A fragment source that answers from a script, and counts what it was asked.
#[derive(Debug)]
struct Scripted {
    /// One answer per region, in region order.
    answers: Mutex<Vec<Answer>>,
    /// How many fragments actually went out.
    asked: Arc<Mutex<usize>>,
    /// Where the regions are split, or empty for one region over everything.
    splits: Mutex<Vec<Vec<u8>>>,
    /// What this source answers for [`FragmentSource::runs_are_region_scoped`].
    ///
    /// `true` for every test but one: this source really does scope its shards, and saying
    /// otherwise would make the fold tests below prove nothing. The exception exists so the guard
    /// that keys on this can be asserted to fire.
    scoped: Mutex<bool>,
}

impl Scripted {
    /// Splits the key space at `at`, so a table's range covers two regions.
    fn split_at(&self, at: &[u8]) {
        self.splits.lock().unwrap().push(at.to_vec());
    }

    /// Makes this source declare that a fragment reads more than its own region's rows, which is
    /// what the production store does today (`docs/bench/mpp-baseline.md` §10).
    fn is_not_region_scoped(&self) {
        *self.scoped.lock().unwrap() = false;
    }
}

impl FragmentSource for Scripted {
    /// **`true` unless a test says otherwise, and it is not a courtesy.** This source synthesises
    /// one shard per split and answers each from its own scripted result, so a fragment sees
    /// exactly its region's rows — which is what the production store will do once its columnar
    /// copy is region-scoped, and what the fold tests are the proof of.
    fn runs_are_region_scoped(&self) -> bool {
        *self.scoped.lock().unwrap()
    }

    fn shards(&self, _start: &[u8], _end: &[u8]) -> esker_sql::Result<Vec<Shard>> {
        let splits = self.splits.lock().unwrap().clone();
        let mut bounds: Vec<Vec<u8>> = vec![Vec::new()];
        bounds.extend(splits);
        bounds.push(Vec::new());
        Ok((0..bounds.len() - 1)
            .map(|at| Shard {
                region_id: at as u64 + 1,
                epoch: Epoch::INITIAL,
                start: bytes::Bytes::from(bounds[at].clone()),
                end: bytes::Bytes::from(bounds[at + 1].clone()),
                columnar: Some(Peer {
                    store_id: 9,
                    peer_id: 9,
                    role: PeerRole::ColumnarLearner,
                }),
            })
            .collect())
    }

    fn evaluate(
        &self,
        shard: &Shard,
        _fragment: &[u8],
        _ts: u64,
        _min_apply_index: u64,
    ) -> esker_sql::Result<Answer> {
        *self.asked.lock().unwrap() += 1;
        let answers = self.answers.lock().unwrap();
        let at = usize::try_from(shard.region_id - 1)
            .unwrap_or(0)
            .min(answers.len().saturating_sub(1));
        Ok(answers[at].clone())
    }
}

/// A source that answers each region with the group it was given.
fn script(answers: &[Body]) -> Arc<Scripted> {
    Arc::new(Scripted {
        answers: Mutex::new(
            answers
                .iter()
                .map(|body| Answer::Answered {
                    result: bytes::Bytes::from(
                        esker_proto::fragment::result::encode(body).expect("a well-formed body"),
                    ),
                    stats: ScanStats {
                        stripes_considered: 4,
                        stripes_read: 2,
                        chunks_decoded: 1,
                        rows_scanned: 100,
                        rows_matched: 10,
                    },
                })
                .collect(),
        ),
        asked: Arc::new(Mutex::new(0)),
        splits: Mutex::new(Vec::new()),
        scoped: Mutex::new(true),
    })
}

/// A source that refuses everything.
fn refusing(reason: RefusalReason) -> Arc<Scripted> {
    Arc::new(Scripted {
        answers: Mutex::new(vec![Answer::Refused {
            reason,
            detail: "for a human".to_owned(),
        }]),
        asked: Arc::new(Mutex::new(0)),
        splits: Mutex::new(Vec::new()),
        scoped: Mutex::new(true),
    })
}

/// One `Body::Groups` with the given groups; the aggregate shapes are taken from the first.
fn groups(rows: &[(&[Value], &[Partial])]) -> Body {
    let aggregates = rows
        .first()
        .map(|(_, partials)| {
            partials
                .iter()
                .map(|partial| (partial.kind(), aggregate_type(partial)))
                .collect()
        })
        .unwrap_or_default();
    Body::Groups {
        key_types: rows
            .first()
            .map(|(key, _)| key.iter().map(value_type).collect())
            .unwrap_or_default(),
        aggregates,
        groups: rows
            .iter()
            .map(|(key, partials)| Group {
                key: key.to_vec(),
                partials: partials.to_vec(),
            })
            .collect(),
    }
}

fn value_type(value: &Value) -> ValueType {
    value.value_type().unwrap_or(ValueType::Int8)
}

fn aggregate_type(partial: &Partial) -> Option<ValueType> {
    match partial {
        Partial::Count(_) => None,
        Partial::Sum(value) | Partial::Min(value) | Partial::Max(value) => Some(
            value
                .as_ref()
                .and_then(Value::value_type)
                .unwrap_or(ValueType::Int8),
        ),
    }
}

/// One SQL node over an in-memory store, with no fragment source.
fn node() -> Executor {
    Executor::new(
        Arc::new(MemoryBackend::new()) as Arc<dyn Backend>,
        Arc::new(Catalog::new()),
        1,
        esker_sql::session::register(),
    )
}

/// The same, able to ask fragments of `source`.
fn node_with(source: Arc<Scripted>) -> Executor {
    node().asking_fragments_of(source as Arc<dyn FragmentSource>)
}

/// The table every routing case in this file is about: three columns, one columnar replica.
fn ready(node: &mut Executor) {
    run(
        node,
        "CREATE TABLE t (id int8 PRIMARY KEY, name text, amount int8)",
    )
    .unwrap();
    run(node, "ALTER TABLE t SET (columnar_replicas = 1)").unwrap();
}

fn run(node: &mut Executor, sql: &str) -> esker_sql::Result<()> {
    for parsed in parse_statements(sql)? {
        match parsed.class() {
            StatementClass::Begin => {
                node.begin(parsed.begins_read_only())?;
                if let Some(level) = parsed.begins_isolation() {
                    node.set_isolation(level)?;
                }
            }
            StatementClass::Commit => node.commit()?,
            StatementClass::Rollback => node.rollback()?,
            _ => {
                let _: Outcome = node.execute(&parsed, &Params::NONE)?;
            }
        }
    }
    Ok(())
}

/// The rows a query answers, as text.
fn rows(node: &mut Executor, sql: &str) -> Vec<Vec<String>> {
    let parsed = parse_statements(sql).unwrap().pop().expect("one statement");
    match node.execute(&parsed, &Params::NONE).unwrap() {
        Outcome::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| {
                        cell.map_or_else(
                            || "NULL".to_owned(),
                            |bytes| String::from_utf8_lossy(&bytes).into_owned(),
                        )
                    })
                    .collect()
            })
            .collect(),
        other @ Outcome::Done { .. } => panic!("`{sql}` answered {other:?}"),
    }
}

/// The `EXPLAIN` of a query, as one string.
fn explain(node: &mut Executor, sql: &str) -> String {
    rows(node, &format!("EXPLAIN {sql}"))
        .into_iter()
        .map(|row| row.join(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `EXPLAIN ANALYZE` of a query, as one string. Runs it.
fn explain_analyze(node: &mut Executor, sql: &str) -> String {
    rows(node, &format!("EXPLAIN ANALYZE {sql}"))
        .into_iter()
        .map(|row| row.join(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The one value a `SHOW` answers with.
fn show(node: &mut Executor, sql: &str) -> String {
    rows(node, sql).pop().and_then(|mut row| row.pop()).unwrap()
}

/// What this node answers for one `;`-separated script: `ok`, or `!SQLSTATE message`.
fn answer(script: &str) -> String {
    let mut node = node();
    let statements: Vec<&str> = script.split(';').map(str::trim).collect();
    let (last, setup) = statements.split_last().expect("a script has a statement");
    for statement in setup {
        if let Err(error) = run(&mut node, statement) {
            return format!("!{} {error} (in setup `{statement}`)", error.sqlstate());
        }
    }
    match run(&mut node, last) {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("!{} {error}", error.sqlstate()),
    }
}

/// The same, over the corpus fixture table.
fn answer_over_t(script: &str) -> String {
    let mut node = node();
    run(
        &mut node,
        "CREATE TABLE t (id int8 PRIMARY KEY, name text, amount int8)",
    )
    .expect("the corpus fixture must exist");
    match run(&mut node, script) {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("!{} {error}", error.sqlstate()),
    }
}

/// `(line number, script, expected answer)` for every case in the corpus.
fn corpus() -> Vec<(usize, String, String)> {
    cases(include_str!("corpus/pg19_routing_engine.txt"))
}

/// The same, for the plan-surface corpus.
fn explain_corpus() -> Vec<(usize, String, String)> {
    cases(include_str!("corpus/pg19_routing_explain.txt"))
}

fn cases(corpus: &str) -> Vec<(usize, String, String)> {
    corpus
        .lines()
        .enumerate()
        .filter_map(|(at, line)| {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (script, expected) = line.split_once('\t')?;
            Some((at + 1, script.trim().to_owned(), expected.trim().to_owned()))
        })
        .collect()
}
