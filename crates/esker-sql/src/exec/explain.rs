//! What `EXPLAIN` says about a routed plan.
//!
//! `docs/plans/phase-10-routing.md` U3. ADR 0022 Decision 2 ends with one sentence and this module
//! is it: *"`EXPLAIN` must name the engine it chose. A routing decision nobody can see is one
//! nobody can debug — and this is the feature most likely to produce 'it was fast yesterday'."*
//!
//! # A declared divergence, not a parity gap
//!
//! PostgreSQL's plan text has no equivalent to any of this, because PostgreSQL has no second
//! engine to choose between. Under [ADR 0031](../../../docs/adr/0031-rails-compatibility-is-measured.md)
//! that makes it a **declared** divergence: the corpus records what a real server prints for the
//! same statements, the divergence list carries both sides, and nothing here pretends to be
//! parity. What is borrowed is the *shape* — a node per line, indented by depth, with the node's
//! own details on lines beneath it — because a user reading `EXPLAIN` here should not have to
//! learn a second layout.
//!
//! # Three things a reader needs and only one of them is the engine
//!
//! * **which engine**, and **why** — the ratio, the override, or the refusal that sent it back to
//!   the rows. A fallback is silent to the client by design (the answer is the same either way);
//!   it must not be silent here.
//! * **how many fragments**, because that is the shape of the work: one per region of the table.
//! * **what it cost**, under `EXPLAIN ANALYZE`, from the `ScanStats` the response has carried since
//!   phase 8 — carried then rather than added now precisely because *"`EXPLAIN` is the named
//!   consumer"*.

use std::fmt::Write as _;

use esker_columnar::fragment::{Aggregate as ColAggregate, Output as ColOutput};

use crate::plan::routing::{Columnar, Engine, Finish, Output};

/// The columnar node's own line and the lines that belong to it.
///
/// Called from [`crate::plan::Node`]'s `describe`, which owns the walk and the indentation. The
/// split is the same one every other node makes there: this says *what* to print, that says
/// *where*.
pub(crate) fn describe(columnar: &Columnar, columns: &[String]) -> (String, String) {
    let engine = columnar.decision.engine;
    let line = match engine {
        Engine::Columnar => format!(
            "Columnar Aggregate on {}  ({} fragments)",
            columnar.table,
            columnar.shards.len()
        ),
        // It was planned columnar and answered by the rows. Naming it `Aggregate` is what it now
        // *is*; the engine line below says what happened.
        Engine::Row => format!("Aggregate on {}", columnar.table),
    };

    let mut extra = format!(
        "Engine: {}  ({})",
        engine.name(),
        columnar.decision.reason.describe()
    );
    let names = slot_names(columnar, columns);
    if let ColOutput::Aggregates {
        group_by,
        aggregates,
    } = &columnar.fragment.output
    {
        if !group_by.is_empty() {
            let keys: Vec<&str> = group_by.iter().map(|slot| name(&names, *slot)).collect();
            let _ = write!(extra, "\nGroup Key: {}", keys.join(", "));
        }
        let calls: Vec<String> = columnar
            .outputs
            .iter()
            .filter_map(|output| match output {
                Output::Key(_) => None,
                Output::Aggregate(finish) => Some(render(*finish, aggregates, &names)),
            })
            .collect();
        if !calls.is_empty() {
            let _ = write!(extra, "\nAggregates: {}", calls.join(", "));
        }
    }
    // **The join the fragment absorbed**, named with the table its keys came from. Without this
    // a reader sees a plan with no `Nested Loop` in it and no account of where the join went
    // (`docs/plans/phase-16-mpp.md` §J6).
    if let Some(semi) = &columnar.semi_join {
        let keys = match &columnar.fragment.filter {
            Some(filter) => in_list_len(filter),
            None => 0,
        };
        let _ = write!(
            extra,
            "\nSemi Join Filter: {} in {}  ({keys} keys)",
            name(&names, semi.outer_slot),
            semi.inner_table
        );
    }
    if columnar.fragment.filter.is_some() {
        // The pushed-down predicate, named by the columns it reads rather than re-rendered from
        // the fragment's own tree: what a reader wants to know is *that* the filter went down and
        // over what, and the row plan's `Filter` line above already prints the predicate itself.
        let _ = write!(extra, "\nFilter: pushed down over {}", names.join(", "));
    }

    if let Some(run) = &columnar.run {
        let _ = write!(
            extra,
            "\nFragments: {} asked, {} answered",
            run.asked, run.answered
        );
        let stats = run.stats;
        let _ = write!(
            extra,
            "\nStripes: {} of {} read   Chunks: {}   Rows: {} scanned, {} matched",
            stats.stripes_read,
            stats.stripes_considered,
            stats.chunks_decoded,
            stats.rows_scanned,
            stats.rows_matched
        );
    }
    (line, extra)
}

/// The row plan to print beneath the node, which is the one that ran — and only then.
///
/// A plan that was answered by columns did not run its fallback, and printing a subtree nothing
/// executed is how an `EXPLAIN` becomes something a reader has to second-guess.
#[must_use]
pub(crate) fn child(columnar: &Columnar) -> Option<&crate::plan::Node> {
    match columnar.decision.engine {
        Engine::Row => Some(&columnar.fallback),
        Engine::Columnar => None,
    }
}

/// The table column each projection slot reads, by name.
fn slot_names(columnar: &Columnar, columns: &[String]) -> Vec<String> {
    columnar
        .fragment
        .projection
        .iter()
        .map(|column| {
            columns
                .get(*column as usize)
                .cloned()
                .unwrap_or_else(|| format!("column {column}"))
        })
        .collect()
}

fn name(names: &[String], slot: u32) -> &str {
    names.get(slot as usize).map_or("?", String::as_str)
}

/// One aggregate call, as the user wrote it.
///
/// `avg` is the one that is not a fragment aggregate: it went down as a `sum` and a `count` asked
/// for together, and printing those two would show a query the user did not write. It prints as
/// `avg`, over the column its `sum` reads.
fn render(finish: Finish, asks: &[ColAggregate], names: &[String]) -> String {
    let of = |index: usize| -> String {
        match asks.get(index) {
            Some(ColAggregate::CountStar) => "*".to_owned(),
            Some(ask) => ask
                .slot()
                .map_or_else(|| "*".to_owned(), |slot| name(names, slot).to_owned()),
            None => "?".to_owned(),
        }
    };
    match finish {
        Finish::Count(at) => format!("count({})", of(at)),
        Finish::Sum(at) => format!("sum({})", of(at)),
        Finish::Min(at) => format!("min({})", of(at)),
        Finish::Max(at) => format!("max({})", of(at)),
        Finish::Avg { sum, .. } => format!("avg({})", of(sum)),
    }
}

/// How many values the `IN` list in a filter carries, for the `EXPLAIN` line.
///
/// Zero when there is none — which is what a plan that was built but never run looks like, since
/// the keys are read at resolve time and not while planning.
fn in_list_len(filter: &esker_columnar::Expr) -> usize {
    match filter {
        esker_columnar::Expr::In { values, .. } => values.len(),
        esker_columnar::Expr::And(left, right) | esker_columnar::Expr::Or(left, right) => {
            in_list_len(left).max(in_list_len(right))
        }
        esker_columnar::Expr::Not(inner) => in_list_len(inner),
        _ => 0,
    }
}
