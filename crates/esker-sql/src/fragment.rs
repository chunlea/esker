//! Where this node's fragments go, and the one trait between the planner and a cluster.
//!
//! `docs/plans/phase-10-routing.md` U2. Shaped like [`crate::pd::ColumnarReport`] and
//! [`crate::backend::SchemaLease`] and for the same two reasons: `esker-sql` is written against a
//! trait rather than against `esker-client`'s concrete types, and a node with **no** source is a
//! real configuration — a cluster with no placement driver, and every in-process test cluster in
//! this crate — rather than a broken one. A node without one plans every query on rows and says so
//! in `EXPLAIN` ([`crate::plan::routing::Reason::NoFragmentService`]).
//!
//! # What crosses this seam, and what does not
//!
//! The **request** crosses as `esker-columnar`'s bytes, because there is one definition of them
//! (`docs/plans/phase-8-learner.md` §wire). The **answer** crosses as `esker-proto`'s result
//! format, decoded here, because that is the format the wire carries and `esker-proto` must not
//! link `esker-columnar`. Neither crossing is a second opinion about a format.
//!
//! A **refusal is a value**, not an `Err`: [`Answer::Refused`]. An `Err` is a transport failure,
//! and the planner treats both the same way — fall back to rows in the same snapshot — but they
//! are different things and a caller that could not tell them apart could not say which in
//! `EXPLAIN`.

use crate::error::Result;

pub use esker_client::fragment::{FragmentAnswer as Answer, Shard};
pub use esker_proto::fragment::{RefusalReason, ScanStats};

/// Where a SQL node sends the fragments its planner builds.
pub trait FragmentSource: std::fmt::Debug + Send + Sync {
    /// Every region covering `[start, end)`, in key order, each carrying whether it has a columnar
    /// learner.
    ///
    /// A region **without** one is still returned. The planner needs to see it, because a fragment
    /// answered by every region but that one is an answer about part of the table — which is the
    /// one failure this feature must not have, and is why the shard list is complete or the query
    /// is not routed.
    fn shards(&self, start: &[u8], end: &[u8]) -> Result<Vec<Shard>>;

    /// Sends one fragment to one shard's columnar learner.
    ///
    /// `ts` is the reading transaction's snapshot — the *same* number the row path reads at, which
    /// is what makes the fallback answer the same question. `min_apply_index` is a floor the
    /// caller may name and a SQL node cannot: freshness comes from the `ReadIndex` round the
    /// learner runs for every fragment (ADR 0022 Decision 4, and
    /// `docs/plans/phase-10-routing.md` §2).
    fn evaluate(
        &self,
        shard: &Shard,
        fragment: &[u8],
        ts: u64,
        min_apply_index: u64,
    ) -> Result<Answer>;
}

/// A [`FragmentSource`] over `esker-client`'s fragment path.
///
/// The whole of the wiring, exactly as [`crate::backend::StoreBackend`] is the whole of the row
/// path's: this crate's trait was shaped against that client rather than against a sketch.
#[derive(Debug)]
pub struct ClientFragments {
    client: esker_client::FragmentClient,
}

impl ClientFragments {
    /// A source over a router — the **same** router the transactional backend uses, so one region
    /// cache serves both and a cache warmed by a row read is warm for a fragment.
    #[must_use]
    pub fn new(router: std::sync::Arc<esker_client::Router>) -> Self {
        Self {
            client: esker_client::FragmentClient::new(router),
        }
    }
}

impl FragmentSource for ClientFragments {
    fn shards(&self, start: &[u8], end: &[u8]) -> Result<Vec<Shard>> {
        self.client
            .shards(start, end)
            .map_err(|error| translate(&error))
    }

    fn evaluate(
        &self,
        shard: &Shard,
        fragment: &[u8],
        ts: u64,
        min_apply_index: u64,
    ) -> Result<Answer> {
        self.client
            .evaluate(
                shard,
                &esker_proto::fragment::FragmentReq {
                    fragment: bytes::Bytes::copy_from_slice(fragment),
                    ts,
                    min_apply_index,
                },
            )
            .map_err(|error| translate(&error))
    }
}

/// A client failure, as this crate's error.
///
/// Deliberately plain. Every caller of this trait treats an `Err` the same way — read the rows in
/// the same snapshot — so what the error needs to carry is a sentence for a log and nothing a
/// branch reads. Mapping each client variant to a SQLSTATE would be building a taxonomy nobody
/// consults.
fn translate(error: &esker_client::Error) -> crate::error::SqlError {
    crate::error::SqlError::StoreUnavailable(error.to_string())
}
