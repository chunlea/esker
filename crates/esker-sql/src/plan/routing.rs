//! Which engine a query runs on, and why.
//!
//! [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 2, milestone 4;
//! `docs/plans/phase-10-routing.md` U2. Everything in this module is **pure**: it decides against a
//! [`Shape`] and a [`Setting`] and touches no catalog, no network and no clock, so the rule can be
//! tested as a table rather than against a cluster. Building the fragment, asking for it and
//! folding what comes back is `crate::exec::fragment`'s.
//!
//! # The rule, in ADR 0022 Decision 2's order
//!
//! 1. **A point read or a bounded range stays on rows.** Always, no estimate. Columnar loses badly
//!    — a point read touches one key in a row store and one stripe header *per column* in a
//!    columnar one — and the shape is exactly detectable, because the access path already knows
//!    whether it narrowed.
//! 2. **Anything the columnar side cannot answer stays on rows**: a table with no columnar copy, a
//!    region in the range without a learner, an aggregate or a filter no fragment can express, a
//!    node with no way to ask. Each is a [`Reason`] of its own so that `EXPLAIN` can say which.
//! 3. **The ratio decides.** Bytes read, not rows: a columnar scan reads one chunk per projected
//!    column per stripe where a row scan reads every column of every row, so what matters is
//!    `projected / stored` and not how many rows there are. [`RATIO`] is the threshold and it is a
//!    fraction on purpose — a table that grows a column moves the decision on its own, which is
//!    what ADR 0022 means by *"stating it as a ratio is what stops the threshold from being a
//!    magic number somebody tunes once and forgets"*.
//! 4. **`esker.engine` overrides**, and this is the one place this module reads the ADR
//!    non-literally. Decision 2 calls the session variable *"overrides all of it"*; taken at its
//!    word that would let a session ask for an answer a fragment cannot produce. What it overrides
//!    is **rule 3**, the estimate, and nothing else: `'row'` forces rows, which is always possible,
//!    and `'columnar'` skips the ratio but still cannot express what is not expressible. The
//!    override moves the threshold; it never moves the correctness rules.

use crate::value::Datum;

/// The projection width, as a fraction of the table's width, at or below which columns win.
///
/// One half: below it a columnar scan reads strictly fewer bytes than a row scan even before
/// compression, because a row scan has no way to skip the columns it does not want. Written as a
/// numerator and a denominator rather than a float so that the comparison is exact and the
/// arithmetic is integer — `projected * RATIO.1 <= stored * RATIO.0`.
pub const RATIO: (usize, usize) = (1, 2);

/// Which engine a scan runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// The row replicas, through the transactional client.
    Row,
    /// A columnar learner, through the fragment service.
    Columnar,
}

impl Engine {
    /// What `EXPLAIN` calls it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Engine::Row => "rows",
            Engine::Columnar => "columnar",
        }
    }
}

/// `esker.engine`, the session override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Setting {
    /// Never route columnar.
    Row,
    /// Route columnar wherever a fragment can express the query — the ratio is not consulted.
    Columnar,
    /// The rule above decides.
    #[default]
    Auto,
}

impl Setting {
    /// The setting a stored parameter value names.
    ///
    /// The value reaching here has already been through
    /// [`crate::parameter::Parameter::normalise`], which folds it to one of the three canonical
    /// spellings, so anything else is a value that was never stored and the default is the honest
    /// answer for it.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "row" => Setting::Row,
            "columnar" => Setting::Columnar,
            _ => Setting::Auto,
        }
    }

    /// The spelling `SHOW` hands back and `EXPLAIN` quotes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Setting::Row => "row",
            Setting::Columnar => "columnar",
            Setting::Auto => "auto",
        }
    }
}

/// What the planner knew when it decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// Columns the table stores.
    pub stored: usize,
    /// Distinct table columns the fragment would have to read: the grouping keys, the aggregate
    /// arguments and every column the filter names.
    pub projected: usize,
    /// The access path is a point read or a range the primary key narrowed — rule 1.
    pub bounded: bool,
    /// How many columnar replicas the table's catalog record asks for.
    pub replicas: u8,
}

/// Why an engine was chosen. `EXPLAIN` prints this, which is the whole reason it is an enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// `SET esker.engine` said so.
    Override(Setting),
    /// The ratio favoured columns.
    Ratio {
        /// Columns the fragment reads.
        projected: usize,
        /// Columns the table stores.
        stored: usize,
    },
    /// The ratio favoured rows: the query reads too much of the table for columns to pay.
    TooWide {
        /// Columns the fragment would read.
        projected: usize,
        /// Columns the table stores.
        stored: usize,
    },
    /// A point read or a bounded range — rule 1.
    Bounded,
    /// The table asks for no columnar replicas.
    NotAsked,
    /// The table asks for one and a region of its range has none, so a fragment would answer about
    /// part of the table. Refusing the *whole* query is the only safe answer.
    NoLearner,
    /// This node has no way to ask a fragment — no placement driver, no fragment client. Every
    /// in-process test cluster in this crate.
    NoFragmentService,
    /// The query is a shape no fragment expresses. Names which part, for `EXPLAIN`.
    NotExpressible(&'static str),
    /// It was routed, asked, and refused at run time; the rows answered instead.
    ///
    /// Only ever set by `crate::exec::fragment` after a call, so a plain `EXPLAIN` never carries
    /// one and `EXPLAIN ANALYZE` can.
    Refused(&'static str),
}

impl Reason {
    /// Whether `EXPLAIN` prints an engine line for this reason at all.
    ///
    /// **Two reasons are silent, and both say the same thing: there was no choice.** A table
    /// nobody asked for a columnar copy of has one engine, and a node with no way to ask a
    /// fragment has one engine for every table — so a line about the engine on those plans is
    /// noise on every query on every ordinary cluster, and noise is what stops a line being read
    /// when it does matter. Every other reason describes a decision that was actually made, which
    /// is the thing ADR 0022 Decision 2 asks to be visible.
    #[must_use]
    pub fn worth_printing(&self) -> bool {
        !matches!(self, Reason::NotAsked | Reason::NoFragmentService)
    }

    /// The sentence `EXPLAIN` puts in brackets after the engine.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Reason::Override(setting) => format!("esker.engine = '{}'", setting.name()),
            Reason::Ratio { projected, stored } | Reason::TooWide { projected, stored } => {
                format!("{projected} of {stored} columns projected")
            }
            Reason::Bounded => "a bounded range, which columns cannot restrict to".to_owned(),
            Reason::NotAsked => "the table asks for no columnar replicas".to_owned(),
            Reason::NoLearner => "a region of this table has no columnar learner".to_owned(),
            Reason::NoFragmentService => "this node cannot ask a fragment".to_owned(),
            Reason::NotExpressible(what) => format!("no fragment expresses {what}"),
            Reason::Refused(what) => format!("columnar refused: {what}"),
        }
    }
}

/// An engine and the reason it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Which engine.
    pub engine: Engine,
    /// Why.
    pub reason: Reason,
}

impl Decision {
    /// A row plan, for `reason`.
    #[must_use]
    pub fn rows(reason: Reason) -> Self {
        Decision {
            engine: Engine::Row,
            reason,
        }
    }
}

/// Applies the rule to a shape a fragment is already known to express.
///
/// Everything this cannot see — whether the aggregate has a `DISTINCT`, whether the filter uses an
/// operator the fragment has — is decided before it is called, and arrives as
/// [`Reason::NotExpressible`] instead. So the only questions left here are the cheap ones, which
/// is what makes this a table a test can enumerate.
#[must_use]
pub fn decide(shape: Shape, setting: Setting) -> Decision {
    // Rule 4's forcing half comes first, because it is the one a user reaches for when the rule
    // below is wrong and they need it to stop being consulted.
    if setting == Setting::Row {
        return Decision::rows(Reason::Override(Setting::Row));
    }
    // Rule 1. Ahead of the override, because a fragment cannot restrict to a key range at all
    // (`esker_columnar::fragment::KeyRange`: "a columnar file records none") — asking for one is
    // not a preference a session can express.
    if shape.bounded {
        return Decision::rows(Reason::Bounded);
    }
    // Rule 2, in the order that says the most: a table nobody asked for a copy of is a
    // configuration answer, and a table that was asked for one and has a region without a learner
    // is a placement answer still settling.
    if shape.replicas == 0 {
        return Decision::rows(Reason::NotAsked);
    }
    // Rule 4's preferring half, and rule 3.
    let reason = if setting == Setting::Columnar {
        Reason::Override(Setting::Columnar)
    } else if shape.projected * RATIO.1 <= shape.stored * RATIO.0 {
        Reason::Ratio {
            projected: shape.projected,
            stored: shape.stored,
        }
    } else {
        return Decision::rows(Reason::TooWide {
            projected: shape.projected,
            stored: shape.stored,
        });
    };
    Decision {
        engine: Engine::Columnar,
        reason,
    }
}

/// The columnar half of a plan: what to ask every region for, and the row plan to run instead when
/// any of them will not answer.
///
/// **The fallback is a field, not a branch somebody remembers.** A refusal is a normal answer
/// (ADR 0022 Decision 3) and the node that receives one is the node that knows what to run in its
/// place — which is what makes *"a routing decision never changes an answer"* a property of the
/// type rather than a rule spread over call sites.
///
/// Not `PartialEq`: it holds a [`crate::plan::Node`], which is not either — a plan can carry a
/// float literal, and two of those are compared the way this crate compares floats rather than by
/// an equality that pretends `NaN` equals itself.
#[derive(Debug, Clone)]
pub struct Columnar {
    /// The table the fragment reads.
    pub table_id: u64,
    /// Its name, for `EXPLAIN`.
    pub table: String,
    /// The fragment, built once and sent to every region of the table's range.
    pub fragment: esker_columnar::Fragment,
    /// One output column per grouping key followed by one per aggregate — the row space
    /// [`crate::plan::Node::Aggregate`] produces, which is what makes the substitution exact.
    pub outputs: Vec<Output>,
    /// Whether the statement wrote a `GROUP BY`, which decides the empty-input rule.
    pub grouped: bool,
    /// The engine and the reason, as the planner decided them. Replaced by a
    /// [`Reason::Refused`] when the run falls back.
    pub decision: Decision,
    /// One shard per region of the table's range, **with the epochs the planner saw**.
    ///
    /// Kept rather than re-derived at run time, and that is the correctness half of this field: a
    /// region that split between planning and running answers `EpochNotMatch` rather than
    /// answering about the half it kept, so "one fragment per region" covers the whole table or
    /// nothing.
    pub shards: Vec<esker_client::Shard>,
    /// The row plan this falls back to.
    pub fallback: Box<crate::plan::Node>,
    /// What actually happened, filled in when the plan runs. `None` in a plan that has not.
    pub run: Option<Run>,
    /// The join this fragment absorbed, when it absorbed one.
    ///
    /// A join reaches the columnar path by becoming a **semi-join**: the inner side's key set is
    /// read here and pushed into the outer table's fragment as a membership test
    /// (`docs/plans/phase-16-mpp.md` §J2, §J3). The keys cannot be collected while planning —
    /// planning does no I/O — so what the plan carries is *how to read them*, and
    /// `crate::exec::fragment::resolve` reads them in the **same transaction at the same
    /// snapshot** as the fragment and the fallback. That shared snapshot is the whole of why
    /// absorbing a join cannot change an answer.
    pub semi_join: Option<SemiJoin>,
}

/// How to read the key set a semi-join pushes down, and where it goes in the fragment.
///
/// Exact only when at most one inner row can match an outer row, which is what
/// `Probe::PrimaryKey` and `Probe::UniqueIndex` mean and what `crate::exec::fragment` checks
/// before building one. Every other join shape refuses and says which rule refused it.
#[derive(Debug, Clone)]
pub struct SemiJoin {
    /// A plan producing one column: the inner side's join keys, already filtered.
    pub keys: Box<crate::plan::Node>,
    /// The fragment projection slot holding the outer side's join column.
    pub outer_slot: u32,
    /// The inner table's name, for `EXPLAIN`.
    pub inner_table: String,
}

/// One column of a columnar node's output row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// A grouping key: the value of the group, at this position among the keys.
    Key(usize),
    /// An aggregate, finished from the partials every region returned.
    Aggregate(Finish),
}

/// How one aggregate is finished from partials.
///
/// The four a fragment computes, plus the one it does not: `avg` is a `sum` and a `count` asked
/// for together and divided here, because there is no partial average that combines — averaging
/// two averages is only right when the groups are the same size, and they are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// Add the counts.
    Count(usize),
    /// Add the sums; NULL over no rows, which is not zero.
    Sum(usize),
    /// The least, in `pg_cmp` order.
    Min(usize),
    /// The greatest.
    Max(usize),
    /// `sum / count`, from two partials the fragment was asked for together.
    Avg {
        /// Which partial holds the sum.
        sum: usize,
        /// Which holds the count.
        count: usize,
    },
}

/// What a routed plan actually did, for `EXPLAIN ANALYZE` and for the fallback's reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Regions asked.
    pub asked: usize,
    /// Regions that answered.
    pub answered: usize,
    /// What the answers cost, summed.
    pub stats: esker_proto::fragment::ScanStats,
    /// The finished rows, or `None` when a refusal sent this back to the rows.
    pub rows: Option<Vec<Vec<Datum>>>,
}

#[cfg(test)]
mod tests {
    use super::{Engine, Reason, Setting, Shape, decide};

    fn shape(projected: usize, stored: usize) -> Shape {
        Shape {
            stored,
            projected,
            bounded: false,
            replicas: 1,
        }
    }

    /// `count(*)` reads no column at all, so the ratio is zero and columns always win. This is the
    /// query ADR 0022 exists for and it must not depend on a threshold.
    #[test]
    fn a_count_star_is_always_columnar() {
        for stored in 1..20 {
            let decision = decide(shape(0, stored), Setting::Auto);
            assert_eq!(decision.engine, Engine::Columnar, "{stored} columns");
        }
    }

    /// Exactly half is the boundary and it is inclusive, which is what a ratio written as `<=`
    /// means. Asserted on both sides of it so a later change to `RATIO` cannot slide past.
    #[test]
    fn the_ratio_is_half_and_the_boundary_is_columnar() {
        assert_eq!(decide(shape(3, 6), Setting::Auto).engine, Engine::Columnar);
        assert_eq!(decide(shape(4, 6), Setting::Auto).engine, Engine::Row);
        assert_eq!(
            decide(shape(4, 6), Setting::Auto).reason,
            Reason::TooWide {
                projected: 4,
                stored: 6
            }
        );
    }

    /// A one-column table can never be narrow enough, and says so as a ratio rather than as a
    /// special case.
    #[test]
    fn a_single_column_table_reads_as_rows_unless_nothing_is_projected() {
        assert_eq!(decide(shape(1, 1), Setting::Auto).engine, Engine::Row);
        assert_eq!(decide(shape(0, 1), Setting::Auto).engine, Engine::Columnar);
    }

    /// Rule 1 beats the override, because a fragment cannot restrict to a key range at all — this
    /// is not a preference a session can express.
    #[test]
    fn a_bounded_range_stays_on_rows_even_under_the_override() {
        let bounded = Shape {
            bounded: true,
            ..shape(0, 10)
        };
        let decision = decide(bounded, Setting::Columnar);
        assert_eq!(decision.engine, Engine::Row);
        assert_eq!(decision.reason, Reason::Bounded);
    }

    /// `'row'` beats everything, including a shape the ratio loves.
    #[test]
    fn the_row_override_beats_the_ratio() {
        let decision = decide(shape(0, 10), Setting::Row);
        assert_eq!(decision.engine, Engine::Row);
        assert_eq!(decision.reason, Reason::Override(Setting::Row));
    }

    /// `'columnar'` skips the ratio and says so, so a user reading `EXPLAIN` sees that the
    /// estimate was not consulted rather than that it agreed.
    #[test]
    fn the_columnar_override_skips_the_ratio_and_names_itself() {
        let decision = decide(shape(9, 10), Setting::Columnar);
        assert_eq!(decision.engine, Engine::Columnar);
        assert_eq!(decision.reason, Reason::Override(Setting::Columnar));
    }

    /// A table nobody asked for a copy of is rows whatever the session says: the override moves
    /// the threshold, never the correctness rules.
    #[test]
    fn a_table_with_no_copy_is_rows_under_every_setting() {
        let none = Shape {
            replicas: 0,
            ..shape(0, 10)
        };
        for setting in [Setting::Auto, Setting::Columnar, Setting::Row] {
            assert_eq!(decide(none, setting).engine, Engine::Row, "{setting:?}");
        }
    }

    /// The three spellings, and the fact that anything else reads as `auto` — which can only be a
    /// value that was never stored, because `normalise` folds before this is reached.
    #[test]
    fn a_setting_parses_from_what_normalise_stores() {
        assert_eq!(Setting::parse("row"), Setting::Row);
        assert_eq!(Setting::parse("columnar"), Setting::Columnar);
        assert_eq!(Setting::parse("auto"), Setting::Auto);
        assert_eq!(Setting::parse("sideways"), Setting::Auto);
        for setting in [Setting::Row, Setting::Columnar, Setting::Auto] {
            assert_eq!(Setting::parse(setting.name()), setting);
        }
    }
}
