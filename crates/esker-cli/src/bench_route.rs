//! **Routing a benchmark through a placement driver**, so a band measures a cluster.
//!
//! `--remote HOST:PORT` builds its client on `StaticRegion::whole_key_space`, a resolver that says
//! the whole key space is one region on the store you named. That is exactly right for measuring
//! an engine behind a socket, and it cannot measure four stores: every key goes to the one store
//! whatever the cluster thinks. `docs/bench/v1.1.md` needs the other thing — four stores, a real
//! placement driver, and each key at whichever store holds it.
//!
//! Nothing new is spoken here. `PdReq::GetRegion` already answers with the region, its leader's
//! peer id and every store's address, which is everything a [`Route`] carries; this is that answer
//! translated, behind a mutex because a benchmark asks from several threads and `PdConn` keeps a
//! cluster id in a `Cell`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use esker_client::TcpStores;
use esker_client::region_cache::{RegionResolver, Route};
use esker_proto::{PdReq, PdResp, ProtoError, TransportConfig};

use crate::raw::resolve;
use crate::region::PdConn;

/// A [`RegionResolver`] that asks a placement driver.
pub(crate) struct PdRegions {
    /// One connection, shared: the client caches what it learns, so this is asked on a miss and
    /// not on a request.
    pd: Mutex<PdConn>,
}

impl std::fmt::Debug for PdRegions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PdRegions")
    }
}

impl RegionResolver for PdRegions {
    fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
        let answer = {
            let pd = self
                .pd
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pd.call(&PdReq::GetRegion {
                key: bytes::Bytes::copy_from_slice(key),
            })?
        };
        let PdResp::GetRegion {
            region,
            leader_peer_id,
            ..
        } = answer
        else {
            return Err(ProtoError::invalid(
                "the placement driver answered a different question",
            ));
        };
        // **`Ok(None)` is "no region covers this key", not "PD could not say".** The distinction is
        // the resolver contract's: an error here is retryable and a `None` is terminal, and
        // reporting one as the other makes every call in the process fail for as long as PD is
        // away. `PdConn::call` has already turned a transport failure into `Err`.
        Ok(region.map(|region| {
            let leader = region
                .peers
                .iter()
                .find(|peer| peer.peer_id == leader_peer_id)
                .copied();
            Route { region, leader }
        }))
    }
}

/// Connects to the placement driver, learns where the stores are, and builds both halves of a
/// routed client.
///
/// The store list comes from PD's own answer about the first key rather than from a flag: a
/// benchmark that had to be told the addresses could be told a set the cluster does not have.
pub(crate) fn routed(pd: &str) -> Result<(Arc<TcpStores>, Arc<PdRegions>), String> {
    let address: SocketAddr = resolve(pd)?;
    let conn = PdConn::connect(address)?;
    let PdResp::GetRegion { stores, .. } = conn
        .call(&PdReq::GetRegion {
            key: bytes::Bytes::new(),
        })
        .map_err(|error| format!("asking {pd} where the stores are: {error}"))?
    else {
        return Err("the placement driver answered a different question".to_owned());
    };
    if stores.is_empty() {
        return Err(format!("{pd} knows no stores yet"));
    }
    let addresses: Vec<SocketAddr> = stores
        .iter()
        .map(|store| resolve(&store.address))
        .collect::<Result<_, _>>()?;
    let transport = TcpStores::connect_all(&addresses, TransportConfig::new())
        .map_err(|error| format!("connecting to the {} stores: {error}", addresses.len()))?;
    Ok((
        Arc::new(transport),
        Arc::new(PdRegions {
            pd: Mutex::new(conn),
        }),
    ))
}
