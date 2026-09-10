//! The one place in this crate that knows a socket exists.
//!
//! [`TcpStores`] adapts `esker-proto`'s blocking connection — one peer, `Request` in and
//! `Response` out — to the client's [`StoreTransport`], which addresses a **store id** and
//! answers with a `RawKvResp`. Everything above it routes by region and peer and never learns
//! what an address is.
//!
//! # Where the address book comes from
//!
//! From `--addr`, today, and the store id is **learned rather than guessed**: the handshake's
//! `HelloAck` says which store answered, so the book is keyed by what the server calls itself.
//! A `Peer` carries a store id, and turning that into a socket address is the placement
//! driver's job (`docs/DESIGN.md` §7): phase 4 replaces this map with the store list PD hands
//! out, and nothing above this file changes when it does.
//!
//! A call for a store this book has never heard of is [`ProtoError::NotSent`] — provably
//! never on the wire, so a caller may repeat it safely, and not retryable here because
//! waiting cannot conjure an address.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use esker_proto::transport::{BlockingTransport, RpcTls, TransportConfig};

use crate::transport::StoreTransport;
use crate::wire::{CallResult, ProtoError, Request};

/// How long a store whose dial failed is left alone before another call tries again.
///
/// Not zero, and the number comes from the load: `esker durability record` drove **thirty-one
/// thousand attempts a second** in run 120, and a client that dialled a down store once per
/// attempt would spend that rate on `connect(2)`. Not long either — it is also how quickly a
/// store that has come back is found again, and the supervisor's own restart backoff starts at
/// 500 ms.
const REDIAL_AFTER: Duration = Duration::from_millis(100);

/// One store: where it is, and the connection to it.
#[derive(Debug)]
struct Link {
    /// **Kept**, which is the whole of the fix in this file. Without the address a closed
    /// connection could not be replaced even in principle.
    address: SocketAddr,
    connection: Arc<BlockingTransport>,
    /// Not before this instant. See [`REDIAL_AFTER`].
    redial_after: Instant,
}

/// Connections to the stores of a cluster, one per store, **redialled when one closes**.
///
/// # Why a connection has to be replaceable
///
/// This book used to hold one `BlockingTransport` per store, built at construction, and to throw
/// the addresses away. A store that is killed closes its connection, and every later call to it
/// answered `NotSent { "the connection is closed" }` — which is deliberately *not* retryable,
/// being the class that provably never left the client, so nothing above retried or re-routed.
/// The store never came back as far as that client was concerned, however many times it restarted.
///
/// run 122 is what that costs: four kills, seven million refused writes, and a Raft group the
/// census showed **healthy and idle** for eighty seconds because no write ever reached it. And a
/// load generator is the mild case — every `esker-sql` node holds one of these for the life of the
/// process.
#[derive(Debug)]
pub struct TcpStores {
    links: Mutex<BTreeMap<u64, Link>>,
    /// Kept so a redial is built the way the first dial was.
    config: TransportConfig,
    tls: RpcTls,
}

impl TcpStores {
    /// Connects to one store — phase 2's whole cluster.
    ///
    /// The store id comes from the handshake rather than from the caller: `HelloAck` says
    /// which store answered, and keying the book by anything else would let a client route
    /// confidently to a store that is not the one on the other end of the socket.
    pub fn connect(addr: SocketAddr) -> Result<Self, ProtoError> {
        Self::connect_with(addr, TransportConfig::new())
    }

    /// [`TcpStores::connect`], with the transport configured explicitly.
    pub fn connect_with(addr: SocketAddr, config: TransportConfig) -> Result<Self, ProtoError> {
        Self::connect_over(addr, config, &RpcTls::disabled())
    }

    /// [`TcpStores::connect_with`], over TLS.
    ///
    /// This is the client↔store link, and it is the one where the two ends are **not** both ours:
    /// the client may be a SQL node with no certificate of its own. A store that requires client
    /// certificates would refuse it, which is why mTLS is a choice per link rather than a mode.
    ///
    /// # Errors
    ///
    /// As [`TcpStores::connect_with`], plus a handshake the store refused.
    pub fn connect_with_tls(
        addr: SocketAddr,
        config: TransportConfig,
        tls: &RpcTls,
    ) -> Result<Self, ProtoError> {
        Self::connect_over(addr, config, tls)
    }

    /// The one connect path; TLS or not is decided by `tls`.
    fn connect_over(
        addr: SocketAddr,
        config: TransportConfig,
        tls: &RpcTls,
    ) -> Result<Self, ProtoError> {
        let connection = BlockingTransport::connect_with_tls(addr, config, tls, None)?;
        let store_id = connection.hello_ack().store_id;
        // `TODO(debt-c6 #4)`: PD's store list would replace this, and one connection would become
        // one case of a book that grows as stores are learned (`docs/plans/debt-c6.md` §4).
        Ok(Self::of(
            BTreeMap::from([(store_id, Link::to(addr, connection))]),
            config,
            tls.clone(),
        ))
    }

    /// Connects to every address, keying each connection by the store id its handshake reports.
    ///
    /// This is the stand-in for the placement driver's store list (`docs/DESIGN.md` §7). It is
    /// what makes a redirect work across sockets: the region's peer list turns a `NotLeader` hint
    /// into a store id, and this book turns that into the connection to send on. With one address
    /// a client can learn *who* leads and still have no way to reach it.
    ///
    /// An address that cannot be reached is not fatal — a cluster with one node down is still a
    /// cluster — but every address failing is, because the book would be empty.
    pub fn connect_all(addrs: &[SocketAddr], config: TransportConfig) -> Result<Self, ProtoError> {
        let mut links = BTreeMap::new();
        let mut last_error = None;
        for addr in addrs {
            match BlockingTransport::connect_with(*addr, config) {
                Ok(connection) => {
                    links.insert(connection.hello_ack().store_id, Link::to(*addr, connection));
                }
                // **A store that is down right now is missing from this book for ever**, because
                // a book keyed by the store id its handshake reported has no key for an address
                // that never answered. That is the same `TODO(debt-c6 #4)` — the store list PD
                // hands out is what closes it — and it is a different hole from the one this file
                // just closed, which was about a store that *was* here and came back.
                Err(error) => last_error = Some(error),
            }
        }
        if links.is_empty() {
            return Err(last_error
                .unwrap_or_else(|| ProtoError::not_sent("no addresses were given to connect to")));
        }
        Ok(Self::of(links, config, RpcTls::disabled()))
    }

    /// The book, its dialling settings, and nothing else.
    fn of(links: BTreeMap<u64, Link>, config: TransportConfig, tls: RpcTls) -> Self {
        Self {
            links: Mutex::new(links),
            config,
            tls,
        }
    }

    /// The connection to `store_id`, redialling it if the one held has closed.
    ///
    /// **The dial happens outside the lock.** Two callers meeting a closed connection at once
    /// would otherwise queue behind one `connect(2)`; here they both dial, one wins the slot and
    /// the other's is dropped, which costs a socket and never a wait.
    fn connection(&self, store_id: u64) -> Result<Arc<BlockingTransport>, ProtoError> {
        let address = {
            let mut links = self.lock();
            let Some(link) = links.get_mut(&store_id) else {
                return Err(ProtoError::not_sent(format!(
                    "no address is known for store {store_id}"
                )));
            };
            if !link.connection.is_closed() {
                return Ok(Arc::clone(&link.connection));
            }
            if Instant::now() < link.redial_after {
                // **The only place a statement can learn it met a store that was not there.**
                // A failed attempt is a round trip like any other, so without this a statement
                // that met a dead store and recovered is indistinguishable from one that never
                // met one — see `crate::stmt_stats`.
                crate::stmt_stats::record_not_sent(store_id);
                return Err(ProtoError::not_sent(format!(
                    "the connection to store {store_id} at {} is closed",
                    link.address
                )));
            }
            link.redial_after = Instant::now() + REDIAL_AFTER;
            link.address
        };

        let fresh = BlockingTransport::connect_with_tls(address, self.config, &self.tls, None)
            .map_err(|error| {
                crate::stmt_stats::record_not_sent(store_id);
                ProtoError::not_sent(format!(
                    "reconnecting to store {store_id} at {address}: {error}"
                ))
            })?;
        // **Who answered, not who used to.** An address can be reused; sending to whoever holds it
        // now would be routing confidently to the wrong store, which is the mistake the handshake
        // exists to prevent in the first place.
        let answered = fresh.hello_ack().store_id;
        if answered != store_id {
            return Err(ProtoError::not_sent(format!(
                "{address} answers as store {answered} now, not store {store_id}"
            )));
        }
        // **The recovery itself**, recorded where it happens and nowhere else.
        crate::stmt_stats::record_redial(store_id);
        let fresh = Arc::new(fresh);
        let mut links = self.lock();
        if let Some(link) = links.get_mut(&store_id)
            && link.connection.is_closed()
        {
            link.connection = Arc::clone(&fresh);
        }
        Ok(fresh)
    }

    /// The book, or the book a panicking thread left behind.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Link>> {
        self.links.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The store ids this book can reach.
    #[must_use]
    pub fn store_ids(&self) -> Vec<u64> {
        self.lock().keys().copied().collect()
    }

    /// The one store this book knows, while there is only one.
    #[must_use]
    pub fn only_store(&self) -> Option<u64> {
        let links = self.lock();
        let mut ids = links.keys();
        match (ids.next(), ids.next()) {
            (Some(only), None) => Some(*only),
            _ => None,
        }
    }

    /// Whether every connection is still up.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lock().values().any(|link| link.connection.is_closed())
    }
}

impl Link {
    fn to(address: SocketAddr, connection: BlockingTransport) -> Self {
        Self {
            address,
            connection: Arc::new(connection),
            redial_after: Instant::now(),
        }
    }
}

impl StoreTransport for TcpStores {
    fn call(&self, store_id: u64, request: &Request, deadline: Instant) -> CallResult {
        self.connection(store_id)?.call(request.clone(), deadline)
    }

    fn max_frame_size(&self) -> usize {
        // From the config every connection is built from, rather than from whichever connection
        // is in the book: they cannot differ, and an empty book would otherwise have to answer
        // with a default that is a second copy of the same number.
        self.config.max_frame_size
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::TcpStores;
    use crate::transport::StoreTransport;
    use crate::wire::{Epoch, ProtoError, RawKvReq, Request, RequestHeader};

    /// An empty book is the state before anything is connected, and it must refuse rather
    /// than panic — and refuse in the way that says the request never left.
    #[test]
    fn a_store_with_no_address_is_refused_as_never_sent() {
        let book = TcpStores::of(
            std::collections::BTreeMap::new(),
            esker_proto::transport::TransportConfig::new(),
            esker_proto::transport::RpcTls::disabled(),
        );
        let request = Request::raw_kv(
            RequestHeader::new(1, Epoch::INITIAL, 1),
            RawKvReq::get(b"k".as_slice()),
        );
        let error = book
            .call(7, &request, Instant::now() + Duration::from_secs(1))
            .expect_err("no address");
        assert!(matches!(error, ProtoError::NotSent { .. }));
        assert!(
            error.outcome() == crate::wire::RequestOutcome::NotApplied,
            "a request with nowhere to go provably did not happen"
        );
        assert!(book.store_ids().is_empty());
        assert_eq!(book.max_frame_size(), crate::wire::MAX_FRAME_SIZE);
    }
}
