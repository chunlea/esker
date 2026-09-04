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
use std::time::Instant;

use esker_proto::transport::{BlockingTransport, RpcTls, TransportConfig};

use crate::transport::StoreTransport;
use crate::wire::{CallResult, ProtoError, Request};

/// Connections to the stores of a cluster, one per store.
#[derive(Debug)]
pub struct TcpStores {
    connections: BTreeMap<u64, BlockingTransport>,
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
        Ok(Self {
            // `TODO(debt-c6 #4)`: PD's store list would replace this, and one connection would
            // become one case of a book that grows as stores are learned
            // (`docs/plans/debt-c6.md` §4).
            connections: BTreeMap::from([(store_id, connection)]),
        })
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
        let mut connections = BTreeMap::new();
        let mut last_error = None;
        for addr in addrs {
            match BlockingTransport::connect_with(*addr, config) {
                Ok(connection) => {
                    connections.insert(connection.hello_ack().store_id, connection);
                }
                Err(error) => last_error = Some(error),
            }
        }
        if connections.is_empty() {
            return Err(last_error
                .unwrap_or_else(|| ProtoError::not_sent("no addresses were given to connect to")));
        }
        Ok(Self { connections })
    }

    /// The store ids this book can reach.
    #[must_use]
    pub fn store_ids(&self) -> Vec<u64> {
        self.connections.keys().copied().collect()
    }

    /// The one store this book knows, while there is only one.
    #[must_use]
    pub fn only_store(&self) -> Option<u64> {
        let mut ids = self.connections.keys();
        match (ids.next(), ids.next()) {
            (Some(only), None) => Some(*only),
            _ => None,
        }
    }

    /// Whether every connection is still up.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.connections.values().any(BlockingTransport::is_closed)
    }
}

impl StoreTransport for TcpStores {
    fn call(&self, store_id: u64, request: &Request, deadline: Instant) -> CallResult {
        let Some(connection) = self.connections.get(&store_id) else {
            return Err(ProtoError::not_sent(format!(
                "no address is known for store {store_id}"
            )));
        };
        connection.call(request.clone(), deadline)
    }

    fn max_frame_size(&self) -> usize {
        // Every connection is built from one config, so any of them answers for all. An empty
        // book has no connection to ask and no call to bound; the protocol default is the
        // honest answer rather than zero, which would refuse every request.
        self.connections.values().next().map_or(
            crate::wire::MAX_FRAME_SIZE,
            BlockingTransport::max_frame_size,
        )
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
        let book = TcpStores {
            connections: std::collections::BTreeMap::new(),
        };
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
