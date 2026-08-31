//! PD behind a socket: the async edge, and nothing else.
//!
//! This is the only async code in the crate. Everything it calls is synchronous and takes no
//! runtime ([`crate::pd`]), which is what `CLAUDE.md` means by "async only at the network
//! edge": the engine's `fsync` runs on a blocking thread, never on the reactor that every
//! other connection is served on.
//!
//! The one piece of policy here is the cluster check. It runs **once**, at the top of the
//! dispatch, for every method but `Bootstrap` — in one place, so that adding a method cannot
//! quietly add one that skips it.

use std::sync::Arc;

use esker_proto::pd::{PdReq, PdResp};
use esker_proto::{BoxFuture, ProtoError, Reply, Request, Response, Service};

use crate::StoreStats;
use crate::pd::Pd;
use crate::routing::{RegionBeat, StoreBeat};

/// The placement driver behind the wire.
#[derive(Debug)]
pub struct PdService {
    pd: Arc<Pd>,
}

impl PdService {
    /// Serves `pd`.
    #[must_use]
    pub fn new(pd: Arc<Pd>) -> Arc<Self> {
        Arc::new(Self { pd })
    }

    /// The placement driver it serves.
    #[must_use]
    pub fn pd(&self) -> &Arc<Pd> {
        &self.pd
    }
}

impl Service for PdService {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Reply, ProtoError>> {
        let pd = Arc::clone(&self.pd);
        Box::pin(async move {
            let (cluster_id, request) = match request {
                Request::Pd {
                    cluster_id,
                    request,
                } => (cluster_id, request),
                // The connection answers `Hello` itself; one reaching a service means the
                // transport changed underneath us, which is worth an error rather than a shrug.
                Request::Hello(_) => {
                    return Err(ProtoError::invalid(
                        "Hello is handled by the connection, not by the placement driver",
                    ));
                }
                // A placement driver is not a store. Answering a key-value request — even with
                // something helpful-looking — would let a misconfigured client believe it had
                // reached a store and read data from a process that has none.
                other => {
                    return Err(ProtoError::invalid(format!(
                        "{} is not a placement-driver method",
                        other.method().name()
                    )));
                }
            };

            let response = blocking(move || serve(&pd, cluster_id, &request)).await?;
            Ok(Reply::Unary(Response::Pd(response)))
        })
    }

    /// PD is not a store, so the handshake reports **zero** — which is not a store id anywhere
    /// in this codebase, and is therefore an honest way to say "there is no store here". A
    /// client keys its connections by this field (`esker_client::TcpStores`), and PD is never
    /// in that book.
    fn store_id(&self) -> u64 {
        0
    }
}

/// One request, synchronously.
fn serve(pd: &Pd, cluster_id: u64, request: &PdReq) -> Result<PdResp, ProtoError> {
    // The cluster check, in one place. `Bootstrap` may carry zero — asking is how a caller
    // learns the id — but a caller that *does* name a cluster is checked even there, so a store
    // that already belongs to one cannot bootstrap a second cluster on a wiped PD by accident.
    match request {
        PdReq::Bootstrap { .. } if cluster_id == 0 => {}
        _ => pd.check_cluster(cluster_id)?,
    }

    Ok(match request {
        PdReq::Bootstrap { store } => {
            let done = pd.bootstrap(store.store_id, &store.address)?;
            PdResp::Bootstrap {
                cluster_id: done.cluster_id,
                region: done.region,
            }
        }
        PdReq::StoreHeartbeat {
            store_id,
            capacity,
            available,
            region_count,
            leader_count,
            applied_bytes,
        } => {
            pd.store_heartbeat(&StoreBeat {
                store_id: *store_id,
                stats: StoreStats {
                    capacity: *capacity,
                    available: *available,
                    region_count: *region_count,
                    leader_count: *leader_count,
                    applied_bytes: *applied_bytes,
                },
            })?;
            PdResp::StoreHeartbeat
        }
        PdReq::RegionHeartbeat {
            region,
            leader_peer_id,
            term,
            approximate_size,
            applied_index,
        } => {
            // A stale beat is dropped rather than refused, and the answer does not say which:
            // the sender has nothing to do differently, and the next beat supersedes it. What
            // the answer carries is the operator PD wants this region's leader to propose,
            // which a stale beat can earn just as well as a fresh one — PD schedules against
            // the record it holds, not against the beat it was sent.
            let beat = pd.region_heartbeat(&RegionBeat {
                region: region.clone(),
                leader_peer_id: *leader_peer_id,
                term: *term,
                approximate_size: *approximate_size,
                applied_index: *applied_index,
            })?;
            PdResp::RegionHeartbeat {
                operator: beat.operator,
            }
        }
        PdReq::GetRegion { key } => match pd.get_region(key)? {
            Some(route) => PdResp::GetRegion {
                region: Some(route.region),
                leader_peer_id: route.leader_peer_id.unwrap_or(0),
                stores: route.stores,
            },
            None => PdResp::GetRegion {
                region: None,
                leader_peer_id: 0,
                stores: Vec::new(),
            },
        },
        PdReq::AllocId { count } => PdResp::AllocId {
            start: pd.alloc_id(*count)?,
            count: *count,
        },
        PdReq::Tso { count } => PdResp::Tso {
            start_ts: pd.tso(*count)?,
            count: *count,
        },
        PdReq::SchemaLease => {
            let lease = pd.schema_lease();
            PdResp::SchemaLease {
                lease_ms: lease.lease_ms,
                step_interval_ms: lease.step_interval_ms,
                removal_extra_ms: lease.removal_extra_ms,
            }
        }
    })
}

/// Runs synchronous engine work on a blocking thread.
///
/// `esker-engine` is synchronous and stays that way, so an `fsync` that takes ten milliseconds
/// blocks a blocking thread rather than the reactor.
async fn blocking<T, F>(work: F) -> Result<T, ProtoError>
where
    F: FnOnce() -> Result<T, ProtoError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ProtoError::internal(format!("the request task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::{PdService, serve};
    use crate::clock::TestClock;
    use crate::pd::{Pd, PdOptions};
    use esker_proto::pd::{PdReq, PdResp};
    use esker_proto::{ProtoError, Reply, Request, Response, Service, StoreInfo};
    use std::sync::Arc;

    fn open() -> (tempfile::TempDir, Arc<Pd>) {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let pd = Pd::open(
            dir.path(),
            PdOptions::with_clock(clock as Arc<dyn crate::Clock>),
        )
        .unwrap();
        (dir, pd)
    }

    /// Every method but `Bootstrap` is checked against the cluster id, and the check is in one
    /// place so that a new method cannot skip it.
    #[test]
    fn every_method_but_bootstrap_refuses_a_foreign_cluster_id() {
        let (_dir, pd) = open();
        let cluster_id = pd.bootstrap(1, "127.0.0.1:1").unwrap().cluster_id;
        let wrong = cluster_id ^ 1;

        let requests = [
            PdReq::StoreHeartbeat {
                store_id: 1,
                capacity: 0,
                available: 0,
                region_count: 0,
                leader_count: 0,
                applied_bytes: 0,
            },
            PdReq::GetRegion {
                key: bytes::Bytes::new(),
            },
            PdReq::AllocId { count: 1 },
            PdReq::Tso { count: 1 },
            PdReq::SchemaLease,
        ];
        for request in requests {
            let name = request.method().name();
            assert!(
                matches!(
                    serve(&pd, wrong, &request),
                    Err(ProtoError::ClusterMismatch { .. })
                ),
                "{name} served a foreign cluster id"
            );
            assert!(serve(&pd, cluster_id, &request).is_ok(), "{name}");
        }
    }

    /// A caller that names a cluster while bootstrapping is checked too: a store that already
    /// belongs to one must not quietly create a second on a PD whose state was wiped.
    #[test]
    fn bootstrap_may_omit_the_cluster_id_but_not_get_it_wrong() {
        let (_dir, pd) = open();
        let request = PdReq::Bootstrap {
            store: StoreInfo::new(1, "127.0.0.1:1"),
        };

        // A fresh PD, and a caller that thinks it knows the cluster: refused, because this PD
        // has no cluster to be that one.
        assert!(matches!(
            serve(&pd, 777, &request),
            Err(ProtoError::NotBootstrapped)
        ));

        // With zero it goes through, and the id it answers with is then required.
        let PdResp::Bootstrap { cluster_id, region } = serve(&pd, 0, &request).unwrap() else {
            panic!("Bootstrap answered something else");
        };
        assert!(region.is_some());
        assert!(serve(&pd, cluster_id, &request).is_ok());
        assert!(matches!(
            serve(&pd, cluster_id ^ 1, &request),
            Err(ProtoError::ClusterMismatch { .. })
        ));
    }

    /// A store's request reaching PD is a misconfiguration, and must be refused rather than
    /// answered with anything that could look like agreement.
    #[tokio::test]
    async fn a_key_value_request_is_refused() {
        let (_dir, pd) = open();
        let service = PdService::new(pd);
        let request = Request::raw_kv(
            esker_proto::RequestHeader::default(),
            esker_proto::RawKvReq::get(&b"k"[..]),
        );
        assert!(matches!(
            service.call(request).await,
            Err(ProtoError::InvalidRequest { .. })
        ));
        assert_eq!(service.store_id(), 0, "pd is not a store");
    }

    #[tokio::test]
    async fn the_service_answers_a_bootstrap() {
        let (_dir, pd) = open();
        let service = PdService::new(Arc::clone(&pd));
        let reply = service
            .call(Request::Pd {
                cluster_id: 0,
                request: PdReq::Bootstrap {
                    store: StoreInfo::new(1, "127.0.0.1:20160"),
                },
            })
            .await
            .unwrap();
        let Reply::Unary(Response::Pd(PdResp::Bootstrap { cluster_id, region })) = reply else {
            panic!("Bootstrap answered something else");
        };
        assert_eq!(cluster_id, pd.cluster_id().unwrap());
        assert_eq!(region.unwrap().id, 1);
    }
}
