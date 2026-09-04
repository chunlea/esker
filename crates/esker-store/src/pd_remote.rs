//! The placement driver over a socket: [`PdClient`] backed by `esker-proto`'s `PdChannel`.
//!
//! # The one awkward seam in the store, and why it is here
//!
//! `PdChannel` is asynchronous, because the wire is. [`PdClient`] is synchronous, because
//! everything in the store that calls it is: the bootstrap runs before the store serves anything,
//! and a heartbeat round runs on a blocking thread beside the engine. Something has to bridge
//! them, and the bridge belongs on this side — a `PdClient` that returned futures would push
//! `async` into the bootstrap path and into the heartbeat schedule, both of which are otherwise
//! ordinary synchronous code with no reason to know a network exists.
//!
//! `esker-proto`'s [`BlockingTransport`](esker_proto::BlockingTransport) is the same bridge for
//! the CLI, and it deliberately **refuses** to be called from inside a `tokio` runtime. A
//! heartbeat round runs on `spawn_blocking`, where the runtime handle *is* present, so that
//! refusal applies — which is why this is a second bridge rather than a use of the first.
//!
//! # How it works
//!
//! One dedicated thread owns a one-worker runtime and the connection. A call posts a closure to
//! it and blocks on the answer. The worker keeps the reader, the writer and the keepalive running
//! between calls, which a current-thread runtime would not: a connection that only advanced while
//! a caller was inside it could not notice a peer that had gone away.
//!
//! Blocking the caller is the point rather than a compromise. The two callers are a store opening
//! — once, before it serves anything — and a heartbeat round on a blocking thread, which is
//! exactly the thread pool that exists for work like this.
//!
//! # Reconnection
//!
//! There is no backoff loop. A failed call drops the connection and the next call builds a new
//! one, which is the same rule the Raft transport follows and for the same reason: the caller's
//! own cadence *is* the backoff. A store heartbeat retries in ten seconds whatever this does.
//!
//! # Three endpoints, and following the hint
//!
//! A placement driver is a Raft group of up to three members and only its leader answers
//! ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)). So a store is given the whole list,
//! believes one of them, and moves when it is told to:
//!
//! * a [`ProtoError::PdNotLeader`] that **names** a member moves this client to that endpoint and
//!   retries, up to [`REDIRECT_BUDGET`] times — a bound, because a group mid-election can hand out
//!   hints that chase each other and a client that followed them for ever would never fail;
//! * a refusal that names **nobody** is an election in progress, and there is nothing to chase.
//!   The client backs off, moves to the next endpoint in the list, and tries again — *never*
//!   spins, because the member's answer will not change until the election ends;
//! * a hint naming an address **outside** the list is no longer a misconfiguration, because a
//!   placement driver's membership moves
//!   ([ADR 0061](../../../docs/adr/0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)):
//!   it may simply be a member added since this store started. So it triggers a **refresh** —
//!   `Pd::Members` at the endpoint that gave the hint, which any member answers — and the list is
//!   adopted only if its **group id** matches the one this client first learned. That check is
//!   what keeps ADR 0059's protection: one cluster's placement driver still cannot route a store
//!   into another's, because the two have different group ids and always will.
//!
//! The believed endpoint is sticky: the next call starts where the last one succeeded, so a
//! cluster pays for a leader change once rather than on every heartbeat.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use esker_proto::{
    BoxFuture, PdChannel, ProtoError, StoreInfo as WireStoreInfo, TcpTransport, Transport,
    TransportConfig,
};

use crate::pd::{Bootstrapped, PdClient, RegionHeartbeat, RegionRoute, StoreHeartbeat, StoreInfo};

/// One unit of work for the dispatcher thread: a closure that owns its own reply channel.
///
/// A closure rather than an enum of five request shapes, because the reply type differs per
/// method and an enum would need a variant per `(request, reply)` pair — ten places to keep in
/// step with five methods.
///
/// The channel arrives as a `Result`, so a connection that could not be made reaches the caller
/// as the connect error itself rather than as "the client stopped".
type Job = Box<dyn FnOnce(Result<Arc<PdChannel>, ProtoError>) -> BoxFuture<'static, ()> + Send>;

/// Redirects one call will follow before it gives up.
///
/// Twice round a group of three. A bound rather than a timeout because the failure it guards
/// against is a *loop* — two members each naming the other while an election settles — and a loop
/// is bounded by counting, not by waiting.
pub const REDIRECT_BUDGET: usize = 6;

/// How long a client waits when no member will say who leads.
///
/// An election takes one to two seconds at the project's tick (`esker_raft::TICK_MS`), so this is
/// short enough to catch the end of one and long enough that a store is not spinning through
/// three endpoints while it runs. It doubles up to [`NO_LEADER_BACKOFF_MAX_MS`], which is the
/// difference between waiting and hammering.
pub const NO_LEADER_BACKOFF_MS: u64 = 100;

/// The cap on that backoff.
pub const NO_LEADER_BACKOFF_MAX_MS: u64 = 800;

/// A [`PdClient`] talking to a real placement driver.
#[derive(Debug)]
pub struct RemotePd {
    /// Every member this client knows of, in the order it was given them.
    ///
    /// **It grows and shrinks**, because a placement driver's group does. The configured list is
    /// where it starts; a refresh replaces it with what the group says, once the group has proved
    /// it is the same group.
    endpoints: Arc<RwLock<Vec<SocketAddr>>>,
    /// The group this client first learned, or zero before it has learned one.
    ///
    /// Latched, and never overwritten: it is what a refreshed member list is checked against, and
    /// a client that adopted a new id along with a new list would have checked nothing.
    group_id: AtomicU64,
    /// Which of them this client believes leads. Shared with the dispatcher, which reconnects
    /// when it moves.
    at: Arc<AtomicUsize>,
    /// `None` only while dropping: the dispatcher's loop ends when the last sender goes, so the
    /// sender has to be released before the thread can be joined.
    jobs: Option<Sender<Job>>,
    /// Joined on drop, so the runtime and the connection go away with this.
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl RemotePd {
    /// A client for the placement driver at `address`, with the project's transport defaults.
    ///
    /// **Connects lazily.** A store whose PD is not up yet fails its first call rather than its
    /// construction, which is what lets a cluster be started in any order — and a store that
    /// cannot reach PD at bootstrap fails to open, which is the behaviour
    /// [`crate::server::Store::open`] wants.
    pub fn connect(address: SocketAddr) -> Result<Self, ProtoError> {
        Self::connect_to(&[address], TransportConfig::new())
    }

    /// A client configured explicitly, against one member.
    pub fn connect_with(address: SocketAddr, config: TransportConfig) -> Result<Self, ProtoError> {
        Self::connect_to(&[address], config)
    }

    /// A client for a placement-driver **group**, given every member's address.
    ///
    /// The order is the order the operator wrote them in and means nothing beyond where the first
    /// attempt goes; the client learns who leads from the first refusal.
    pub fn connect_to(
        endpoints: &[SocketAddr],
        config: TransportConfig,
    ) -> Result<Self, ProtoError> {
        if endpoints.is_empty() {
            return Err(ProtoError::invalid(
                "a placement-driver client needs at least one endpoint",
            ));
        }
        let endpoints = Arc::new(RwLock::new(endpoints.to_vec()));
        let at = Arc::new(AtomicUsize::new(0));
        let (jobs, inbox) = channel();
        let thread = {
            let endpoints = Arc::clone(&endpoints);
            let at = Arc::clone(&at);
            std::thread::Builder::new()
                .name("pd-client".to_owned())
                .spawn(move || dispatch(&endpoints, &at, config, &inbox))
                .map_err(|error| {
                    ProtoError::internal(format!(
                        "could not start the placement-driver thread: {error}"
                    ))
                })?
        };
        Ok(Self {
            endpoints,
            group_id: AtomicU64::new(0),
            at,
            jobs: Some(jobs),
            thread: std::sync::Mutex::new(Some(thread)),
        })
    }

    /// The endpoint this client currently believes leads.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        let endpoints = self.endpoint_list();
        endpoints[self.at.load(Ordering::Acquire).min(endpoints.len() - 1)]
    }

    /// Every member this client knows of, as it currently believes the group to be.
    #[must_use]
    pub fn endpoints(&self) -> Vec<SocketAddr> {
        self.endpoint_list()
    }

    /// The group this client has latched, or zero before it has asked.
    #[must_use]
    pub fn group_id(&self) -> u64 {
        self.group_id.load(Ordering::Acquire)
    }

    /// The endpoints, or — if the lock is poisoned, which means a thread panicked holding it —
    /// what was there. Answering something beats panicking (`CLAUDE.md` invariant 9).
    fn endpoint_list(&self) -> Vec<SocketAddr> {
        self.endpoints.read().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |held| held.clone(),
        )
    }

    /// Points this client at `address` if it is one of the configured endpoints.
    ///
    /// Returns whether it moved. A hint naming an address outside the list is refused: it is a
    /// misconfiguration, and following it would let one cluster's placement driver route a store
    /// into another's.
    fn follow(&self, address: &str) -> bool {
        let Ok(hinted) = address.parse::<SocketAddr>() else {
            return false;
        };
        if let Some(at) = self.endpoint_list().iter().position(|end| *end == hinted) {
            return self.at.swap(at, Ordering::AcqRel) != at;
        }
        // Not one of ours — which, since a placement driver's membership moves, is as likely to
        // mean "added since this client started" as "misconfigured". Ask the member that gave the
        // hint; it is one of ours, and any member answers.
        if !self.refresh() {
            return false;
        }
        let Some(at) = self.endpoint_list().iter().position(|end| *end == hinted) else {
            tracing::warn!(
                %hinted,
                "the placement driver named a leader its own group does not contain"
            );
            return false;
        };
        self.at.swap(at, Ordering::AcqRel) != at
    }

    /// Asks the current endpoint who is in its group, and adopts the answer.
    ///
    /// **The group id is the guard**, and it is the whole of what makes this safe. Before ADR 0061
    /// a hint outside the configured list was refused outright, because following one would let
    /// another cluster's placement driver route this store. That refusal cannot stand once
    /// membership moves — the address may be a member added an hour ago — so the check moves with
    /// it: a list is adopted only from a group whose id is the one this client first learned.
    ///
    /// The first answer latches the id. There is nothing to compare it against, and nothing to
    /// gain from refusing it: this client was pointed at that address by its own configuration.
    fn refresh(&self) -> bool {
        let membership = match self.attempt(|channel| async move { channel.members().await }) {
            Ok(Ok(membership)) => membership,
            Ok(Err(error)) | Err(error) => {
                tracing::debug!(%error, "could not refresh the placement-driver group");
                return false;
            }
        };
        let known = self.group_id.load(Ordering::Acquire);
        if known != 0 && membership.group_id != known {
            tracing::error!(
                expected = format_args!("{known:#018x}"),
                actual = format_args!("{:#018x}", membership.group_id),
                "a placement driver answered for a different group; its members were not adopted"
            );
            return false;
        }
        let mut fresh = Vec::with_capacity(membership.members.len());
        for member in &membership.members {
            match member.address.parse::<SocketAddr>() {
                Ok(address) => fresh.push(address),
                // A member with an address this build cannot parse is one it cannot reach, and
                // dropping it is better than refusing the whole list: the others are still good.
                Err(error) => tracing::warn!(
                    id = member.id,
                    address = %member.address,
                    %error,
                    "a placement driver's address will not parse; skipping it"
                ),
            }
        }
        if fresh.is_empty() {
            return false;
        }
        self.group_id.store(membership.group_id, Ordering::Release);
        let moved = {
            let Ok(mut held) = self.endpoints.write() else {
                return false;
            };
            let moved = *held != fresh;
            *held = fresh;
            moved
        };
        if moved {
            // The index may now point past the end, or at a different member. Start again from
            // the top: the next refusal will say where to go, and one extra round trip after a
            // membership change is not worth remembering a position through it.
            self.at.store(0, Ordering::Release);
            tracing::info!(
                group_id = format_args!("{:#018x}", membership.group_id),
                members = membership.members.len(),
                "the placement-driver group has changed; adopting its members"
            );
        }
        moved
    }

    /// Moves to the next endpoint, for when nobody will say who leads.
    fn advance(&self) {
        let count = self.endpoint_list().len().max(1);
        let next = (self.at.load(Ordering::Acquire) + 1) % count;
        self.at.store(next, Ordering::Release);
    }

    /// Runs one asynchronous call on the dispatcher thread and waits for its answer, following a
    /// redirect if the member it reached is not the leader.
    ///
    /// `work` is called once per attempt, which is why it is `Fn` and `Clone` rather than
    /// `FnOnce`: a redirect is a call that has to be *made again*, at a different member, and a
    /// closure that could only run once would have to be rebuilt by every caller instead.
    ///
    /// Every attempt is safe to repeat. A `PdNotLeader` is a refusal — the member provably did
    /// nothing — so this loop never re-sends a request that may have taken effect.
    fn ask<T, F, Fut>(&self, work: F) -> Result<T, ProtoError>
    where
        T: Send + 'static,
        F: Fn(Arc<PdChannel>) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = Result<T, ProtoError>> + Send + 'static,
    {
        let mut budget = REDIRECT_BUDGET;
        let mut backoff = NO_LEADER_BACKOFF_MS;
        loop {
            match self.attempt(work.clone())? {
                Ok(answer) => return Ok(answer),
                Err(ProtoError::PdNotLeader {
                    leader_id,
                    leader_address,
                }) => {
                    if budget == 0 {
                        return Err(ProtoError::PdNotLeader {
                            leader_id,
                            leader_address,
                        });
                    }
                    budget -= 1;
                    if !leader_address.is_empty() && self.follow(&leader_address) {
                        continue;
                    }
                    // Nobody will say who leads, or the hint was one this store cannot use. There
                    // is nothing to chase: the member's answer will not change until the election
                    // ends, so wait before asking the next one.
                    self.advance();
                    std::thread::sleep(Duration::from_millis(backoff));
                    backoff = (backoff * 2).min(NO_LEADER_BACKOFF_MAX_MS);
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// One attempt. The outer `Result` is "the client is working"; the inner one is the answer.
    fn attempt<T, F, Fut>(&self, work: F) -> Result<Result<T, ProtoError>, ProtoError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<PdChannel>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, ProtoError>> + Send + 'static,
    {
        let (reply, answer) = channel();
        let job: Job = Box::new(move |channel| {
            Box::pin(async move {
                let result = match channel {
                    Ok(channel) => work(channel).await,
                    Err(error) => Err(error),
                };
                // The receiver is gone only if the caller stopped waiting, which is not this
                // side's problem: the call was still made, and PD has still recorded it.
                let _ = reply.send(result);
            })
        });
        self.jobs
            .as_ref()
            .ok_or_else(|| ProtoError::internal("the placement-driver client has stopped"))?
            .send(job)
            .map_err(|_| ProtoError::internal("the placement-driver client has stopped"))?;
        answer
            .recv()
            .map_err(|_| ProtoError::internal("the placement-driver client stopped mid-call"))
    }
}

impl Drop for RemotePd {
    fn drop(&mut self) {
        // The sender has to go first: the dispatcher's loop ends when the channel closes, and
        // joining a thread that is still blocked in `recv` would wait for ever. Joining after it
        // makes the shutdown ordering observable rather than eventual — the runtime and the
        // connection are gone by the time this returns.
        drop(self.jobs.take());
        let thread = self.thread.lock().ok().and_then(|mut slot| slot.take());
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl PdClient for RemotePd {
    fn bootstrap(&self, store: &StoreInfo) -> Result<Bootstrapped, ProtoError> {
        let store = WireStoreInfo::new(store.store_id, store.address.clone());
        let (cluster_id, region) = self.ask(move |channel| {
            // Cloned per attempt rather than moved: a redirect makes this call again, at another
            // member, and the payload has to still be here to make it with.
            let store = store.clone();
            async move {
                // The channel latches the cluster id, so every later call carries it without
                // this layer holding a copy that could drift.
                channel.bootstrap(store).await
            }
        })?;
        Ok(Bootstrapped { cluster_id, region })
    }

    fn alloc_id(&self, count: u64) -> Result<u64, ProtoError> {
        self.ask(move |channel| async move { channel.alloc_id(count).await })
    }

    fn get_region(&self, key: &[u8]) -> Result<Option<RegionRoute>, ProtoError> {
        let key = bytes::Bytes::copy_from_slice(key);
        let found = self.ask(move |channel| {
            let key = key.clone();
            async move { channel.get_region(key).await }
        })?;
        Ok(found.map(|(region, leader, stores)| RegionRoute {
            region,
            leader_peer_id: leader.unwrap_or(0),
            stores: stores
                .into_iter()
                .map(|store| (store.store_id, store.address))
                .collect(),
        }))
    }

    fn store_heartbeat(&self, beat: &StoreHeartbeat) -> Result<(), ProtoError> {
        let beat = *beat;
        self.ask(move |channel| async move {
            channel
                .store_heartbeat(
                    beat.store_id,
                    beat.capacity,
                    beat.available,
                    beat.region_count,
                    beat.leader_count,
                    beat.applied_bytes,
                )
                .await
        })
    }

    fn region_heartbeat(
        &self,
        beat: &RegionHeartbeat,
    ) -> Result<Option<esker_proto::Operator>, ProtoError> {
        let beat = beat.clone();
        self.ask(move |channel| {
            let beat = beat.clone();
            async move {
                channel
                    .region_heartbeat(
                        beat.region,
                        beat.leader_peer_id,
                        beat.term,
                        beat.approximate_size,
                        beat.applied_index,
                    )
                    .await
            }
        })
    }
}

/// The dispatcher thread: own the runtime, hold the connection, run one job at a time.
///
/// One at a time is deliberate. PD calls from a store are a bootstrap and a heartbeat every ten
/// seconds; concurrency here would buy nothing and would make "the connection failed, rebuild it"
/// a decision taken while other calls were in flight on the connection being replaced.
fn dispatch(
    endpoints: &RwLock<Vec<SocketAddr>>,
    at: &AtomicUsize,
    config: TransportConfig,
    inbox: &Receiver<Job>,
) {
    // One worker thread, so the reader, the writer and the keepalive keep running between calls.
    // On a current-thread runtime they would only advance while a job was being awaited, and a
    // connection that only lives during a call cannot notice a dead peer.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "the placement-driver client could not build a runtime");
            return;
        }
    };

    // The `TcpTransport` is kept beside the channel because only it can say whether the socket
    // has ended; `PdChannel` holds an `Arc<dyn Transport>`, which has no such question — a fake
    // transport has no socket to close.
    let mut open: Option<(TcpTransport, Arc<PdChannel>)> = None;
    let mut connected_to: Option<SocketAddr> = None;
    let mut cluster_id = 0_u64;
    while let Ok(job) = inbox.recv() {
        // The caller moves this when it is redirected, so a change here is "go and talk to a
        // different member" and the old socket is of no use for it.
        let listed: Vec<SocketAddr> = endpoints.read().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |held| held.clone(),
        );
        if listed.is_empty() {
            continue;
        }
        // The **address**, not the index, is what identifies a connection: a refresh can reorder
        // the list, and an index that meant one member a moment ago can mean another now.
        let want = listed[at.load(Ordering::Acquire).min(listed.len() - 1)];
        if Some(want) != connected_to {
            open = None;
        }
        if open
            .as_ref()
            .is_some_and(|(transport, _)| transport.is_closed())
        {
            tracing::debug!(address = %want, "the placement-driver connection closed; reconnecting");
            open = None;
        }
        if open.is_none() {
            match runtime.block_on(TcpTransport::connect_with(want, config)) {
                Ok(transport) => {
                    // The cluster id survives a reconnection — and a *redirect*: it is a fact
                    // about the cluster, not about the socket or about which member is answering,
                    // and re-learning it would mean a second `Bootstrap`.
                    let channel = PdChannel::with_cluster(
                        Arc::new(transport.clone()) as Arc<dyn Transport>,
                        cluster_id,
                    );
                    open = Some((transport, Arc::new(channel)));
                    connected_to = Some(want);
                }
                Err(error) => {
                    tracing::debug!(address = %want, %error, "could not reach the placement driver");
                    // The caller is told what went wrong rather than being left to infer it from
                    // a closed reply channel. Its own cadence is the retry.
                    runtime.block_on(job(Err(error)));
                    continue;
                }
            }
        }
        let Some((_, channel)) = open.as_ref().map(|(t, c)| (t, Arc::clone(c))) else {
            continue;
        };
        runtime.block_on(job(Ok(Arc::clone(&channel))));
        cluster_id = channel.cluster_id();
    }
}

#[cfg(test)]
mod tests {
    use super::RemotePd;
    use crate::pd::{PdClient, StoreInfo};

    /// A placement driver that is not there fails the call rather than hanging or panicking.
    /// This is the path a store hits when it is started before its PD, and `Store::open` turns
    /// it into a failed open rather than into a region of its own.
    #[test]
    fn a_placement_driver_that_is_not_listening_fails_the_call() {
        // Port 1 on the loopback: privileged, and nothing of ours listens there.
        let pd = RemotePd::connect("127.0.0.1:1".parse().unwrap()).unwrap();
        let error = pd
            .bootstrap(&StoreInfo {
                store_id: 1,
                address: "127.0.0.1:20160".to_owned(),
            })
            .unwrap_err();
        assert!(!error.to_string().is_empty());

        // And a second call behaves the same way rather than wedging on the first failure.
        assert!(pd.alloc_id(1).is_err());
    }

    /// A hint outside the list is followed only after the group has proved it is the same group.
    ///
    /// Nothing is listening here, so the refresh fails and the hint is refused — which is the
    /// behaviour that matters: **an unreachable group cannot talk a client into a new member
    /// list**, and a client that adopted an address on the strength of a hint alone would be
    /// following one cluster's placement driver into another's.
    #[test]
    fn a_hint_outside_the_list_is_not_followed_on_the_hint_alone() {
        let pd = RemotePd::connect_to(
            &[
                "127.0.0.1:1".parse().unwrap(),
                "127.0.0.1:2".parse().unwrap(),
            ],
            esker_proto::TransportConfig::new(),
        )
        .unwrap();
        assert_eq!(pd.endpoints().len(), 2);
        assert_eq!(pd.group_id(), 0, "nothing has been learned yet");

        assert!(!pd.follow("127.0.0.1:9"), "an unknown address was adopted");
        assert_eq!(pd.endpoints().len(), 2, "the list changed on a hint alone");
        assert_eq!(pd.group_id(), 0);

        // One that *is* in the list moves the client without asking anyone.
        assert!(pd.follow("127.0.0.1:2"));
        assert_eq!(pd.address().port(), 2);
        // And moving to where it already is is not a move.
        assert!(!pd.follow("127.0.0.1:2"));
    }

    /// Dropping the client stops its thread. A store that is reopened in a loop — which the
    /// restart tests do — must not leak one per open.
    #[test]
    fn dropping_the_client_stops_its_thread() {
        for _ in 0..4 {
            let pd = RemotePd::connect("127.0.0.1:1".parse().unwrap()).unwrap();
            assert_eq!(pd.address().port(), 1);
            drop(pd);
        }
    }
}
