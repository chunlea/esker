//! `TxnKv::ReclaimRange` end to end, against a real store on a real socket, killed with
//! `SIGKILL` part-way through the reclaim
//! ([ADR 0069](../../../docs/adr/0069-a-dropped-database-is-reclaimed-by-range-not-key-by-key.md)).
//!
//! The in-process tests in `crate::reclaim` drive `advance` directly and model a crash by dropping
//! the `Db`. This one does not model anything: the store is a **separate process**, the request
//! goes over the wire with a region header the store checks, and the crash is `kill -9` between
//! two chunks. What it proves that the unit tests cannot is that the record on disk is enough — a
//! process that never got to run a line of shutdown code leaves a range a fresh process finishes.
//!
//! The child is this file's [`the_child_serves_until_it_is_killed`], re-executed through the test
//! binary itself with `--exact`, the way `esker-client`'s crash loop does it: no second binary to
//! build and none to keep in step.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_engine::{WriteBatch, WriteOptions, cf};
use esker_proto::{
    BlockingTransport, Request, RequestHeader, Response, TransportConfig, TxnKvReq, TxnKvResp,
};

/// Set for the child, unset for an ordinary run.
const ENV_DIR: &str = "ESKER_RECLAIM_DIR";
/// The child's test name, selected with `--exact`.
const CHILD_TEST: &str = "the_child_serves_until_it_is_killed";
/// How long the parent waits for a child to say where it is listening.
const STARTUP: Duration = Duration::from_secs(30);
/// The drop's commit timestamp in this test, and the safepoint that has to reach it.
const BELOW_TS: u64 = 500;

/// Keys inside the range being reclaimed, and the two on either side that must survive it.
const INSIDE: [&[u8]; 4] = [b"d", b"e", b"f", b"g"];
const OUTSIDE: [&[u8]; 2] = [b"c", b"z"];

/// Writes one raw and one transactional version of `user`, so a clear has to reach both physical
/// namespaces and more than one column family before the range is empty.
fn seed(db: &esker_engine::Db, user: &[u8]) {
    let mut batch = WriteBatch::new();
    batch.put(
        db.cf_id(cf::DEFAULT).unwrap(),
        &esker_keys::prefix::raw_key(user),
        b"raw",
    );
    batch.put(
        db.cf_id(cf::DEFAULT).unwrap(),
        &esker_keys::prefix::txn_key(user, 7),
        b"txn",
    );
    batch.put(
        db.cf_id(cf::WRITE).unwrap(),
        &esker_keys::prefix::txn_key(user, 7),
        b"commit",
    );
    db.write(batch, &WriteOptions::synced()).unwrap();
}

/// Whether anything is stored under `user`, in any family or namespace.
fn holds(db: &esker_engine::Db, user: &[u8]) -> bool {
    let high = [user[0] + 1];
    for name in [cf::DEFAULT, cf::LOCK, cf::WRITE] {
        for (low, high) in [
            (
                esker_keys::prefix::raw_key(user),
                esker_keys::prefix::raw_key(&high),
            ),
            (esker_txn::key::prefix(user), esker_txn::key::prefix(&high)),
        ] {
            let mut iter = db
                .iter(name, &esker_engine::ReadOptions::default())
                .unwrap();
            iter.seek(&low);
            if iter.valid() && iter.key() < high.as_slice() {
                return true;
            }
        }
    }
    false
}

/// Opens a store on [`ENV_DIR`], seeds it the first time, serves it, and prints where.
///
/// In an ordinary run [`ENV_DIR`] is unset and this returns at once; it is a `#[test]` only so
/// that the parent can select it by name.
#[test]
fn the_child_serves_until_it_is_killed() {
    let Ok(dir) = std::env::var(ENV_DIR) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");

    // **Inside the runtime, or time never enters consensus.** `Store::open` spawns the peer's
    // ticker, and `esker-raft` counts ticks rather than reading a clock (invariant 4) — opened
    // outside a reactor the ticker never fires, the single voter never campaigns, and the split
    // below is refused with `NotLeader` twenty seconds later.
    let _guard = runtime.enter();

    let mut out = std::io::stdout().lock();
    // **A placement driver, because a split needs cluster-unique ids.** Without one this store
    // hosts a single region covering everything, the reclaim's chunk is the whole range, and there
    // is no window between two chunks for a kill to land in — which is the window this test is for.
    let pd: Arc<dyn esker_store::pd::PdClient> = Arc::new(esker_store::pd::FakePd::new());
    // And a Raft peer of its own, because a split is *proposed*: without one this store holds a
    // region record it does not lead, and `AdminReq::Split` answers "not replicated on this store".
    // A single voter elects itself, which is all this test needs from consensus.
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut raft = esker_store::server::RaftOptions::new(
        vec![esker_store::PeerAddress::new(1, 1, addr)],
        20_260_904,
    );
    raft.tick = Duration::from_millis(25);
    raft.bootstrap_voters = Some(vec![1]);
    let options = esker_store::StoreOptions {
        pd: Some(pd),
        raft: Some(raft),
        ..esker_store::StoreOptions::new()
    };
    let store = match esker_store::Store::open(&dir, options) {
        Ok(store) => store,
        Err(error) => {
            let _ = writeln!(out, "\nFAIL open: {error}");
            let _ = out.flush();
            return;
        }
    };
    // Seeded once: a restart must find the keys the first process left, not a fresh set.
    if !holds(store.db(), b"c") {
        for key in INSIDE.iter().chain(OUTSIDE.iter()) {
            seed(store.db(), key);
        }
    }
    // Serve only once it leads: a split proposed by a peer that is not the leader is refused, and
    // the parent would read that as a defect rather than as a race it lost.
    // Asked of every region, not of `Store::peer`: after the splits this store hosts three, and
    // the singular accessor answers for one of them — on the restart it answered none, and this
    // loop spent its whole twenty-second budget before serving anyway.
    let waited = Instant::now();
    while waited.elapsed() < Duration::from_secs(20) {
        if store
            .region_statuses()
            .iter()
            .any(|status| status.is_leader)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let service: Arc<dyn esker_proto::transport::Service> = esker_store::StoreService::new(store);
    let handle = runtime.block_on(async {
        esker_proto::transport::Server::bind("127.0.0.1:0", service, TransportConfig::new())
            .await
            .expect("the server binds")
            .spawn()
            .expect("the server starts")
    });

    // The leading newline and the flush are both load-bearing: the harness leaves `test <name>
    // ... ` unterminated, and this process never exits to flush anything on its own.
    let _ = writeln!(out, "\nADDR {}", handle.local_addr());
    let _ = out.flush();
    std::thread::sleep(Duration::from_secs(600));
}

/// A child process, and where it is.
struct Serving {
    process: Child,
    addr: SocketAddr,
}

impl Serving {
    fn start(dir: &Path) -> Self {
        let mut process =
            Command::new(std::env::current_exe().expect("the test binary has a path"))
                .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env(ENV_DIR, dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawning the child");

        let stdout = process.stdout.take().expect("the child's stdout");
        let (line_tx, line_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.trim_start().strip_prefix("ADDR ") {
                    let _ = line_tx.send(rest.to_owned());
                    return;
                }
            }
        });

        let Ok(reported) = line_rx.recv_timeout(STARTUP) else {
            let _ = process.kill();
            let _ = process.wait();
            panic!("the child did not report an address within {STARTUP:?}");
        };
        let addr: SocketAddr = reported
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .expect("an address");
        Self { process, addr }
    }

    /// Every region this store hosts, in key order.
    fn regions(&self) -> Vec<esker_proto::RegionStatus> {
        match self.admin(esker_proto::AdminReq::Regions) {
            esker_proto::AdminResp::Regions { regions } => regions,
            other => panic!("answered {other:?}"),
        }
    }

    /// The header addressing whichever region owns `key` — read back rather than remembered,
    /// because a split bumps the epoch and invariant 5 checks it.
    fn header_for(&self, key: &[u8]) -> RequestHeader {
        let region = self
            .regions()
            .into_iter()
            .map(|status| status.region)
            .find(|region| {
                region.start_key <= key && (region.end_key.is_empty() || key < &region.end_key[..])
            })
            .expect("some region owns the key");
        RequestHeader::new(region.id, region.epoch, 0)
    }

    /// Waits until the region owning `key` reports this store as its leader.
    ///
    /// A split creates a region that has not campaigned yet, so splitting the *result* of a split
    /// races the child's first election — and loses, with `NotLeader`. Waited on as an event
    /// rather than slept past: the answer is in `Admin::Regions`, which already reports it.
    fn led(&self, key: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.regions().into_iter().any(|status| {
                status.is_leader
                    && status.region.start_key <= key
                    && (status.region.end_key.is_empty() || key < &status.region.end_key[..])
            }) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no region owning {key:?} reported a leader within the deadline");
    }

    /// An `Admin` request, which carries no region header of its own.
    fn admin(&self, request: esker_proto::AdminReq) -> esker_proto::AdminResp {
        let transport = BlockingTransport::connect_with(self.addr, TransportConfig::new())
            .expect("connecting to the child");
        match transport
            .call(
                Request::Admin(request),
                Instant::now() + Duration::from_secs(30),
            )
            .expect("the child answered")
        {
            Response::Admin(response) => response,
            other => panic!("the child answered {other:?}"),
        }
    }

    /// One round trip, over the real wire, addressed to whichever region owns the range's start.
    fn call(&self, request: TxnKvReq) -> TxnKvResp {
        let header = match &request {
            TxnKvReq::ReclaimRange { start, .. } => self.header_for(start),
            _ => self.header_for(b"d"),
        };
        let transport = BlockingTransport::connect_with(self.addr, TransportConfig::new())
            .expect("connecting to the child");
        let response = transport
            .call(
                Request::txn_kv(header, request),
                Instant::now() + Duration::from_secs(30),
            )
            .expect("the child answered");
        match response {
            Response::TxnKv(response) => response,
            other => panic!("the child answered {other:?}"),
        }
    }

    /// `SIGKILL`. No shutdown runs, nothing is flushed that was not already synced.
    fn kill(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// A reclaim below the safepoint deletes nothing and says why.
///
/// A transaction whose snapshot predates the drop may still legally read these rows, so this is a
/// refusal rather than a failure: the cursor has not moved, and the answer carries the safepoint
/// the store is working to — which is what tells a caller when to ask again.
fn refuses_below_the_safepoint(child: &Serving) {
    let blocked = child.call(TxnKvReq::ReclaimRange {
        start: Bytes::from_static(b"d"),
        end: Bytes::from_static(b"h"),
        below_ts: BELOW_TS,
    });
    let TxnKvResp::ReclaimRange {
        cursor,
        finished,
        safepoint,
    } = blocked
    else {
        panic!("answered {blocked:?}")
    };
    assert_eq!(cursor, Bytes::from_static(b"d"), "a blocked pass moved");
    assert!(!finished);
    assert!(
        safepoint < BELOW_TS,
        "the gate was already open: {safepoint}"
    );
}

/// Splits the store's single region at `f` and `g`, so the range `[d, h)` spans three.
///
/// **A chunk is one hosted region** (ADR 0069), so a store with one region reclaims the whole
/// range in a single pass and there is no window between two chunks for a kill to land in. This is
/// what gives the test its subject.
fn split_into_three(child: &Serving) {
    for at in [&b"f"[..], b"g"] {
        // The second split is of a region the first one *created*, which has not campaigned yet;
        // splitting it before it leads is refused with `NotLeader`.
        child.led(at);
        let region_id = child.header_for(at).region_id;
        match child.admin(esker_proto::AdminReq::Split {
            region_id,
            split_key: Bytes::copy_from_slice(at),
        }) {
            esker_proto::AdminResp::Split { .. } => {}
            other => panic!("splitting at {at:?} answered {other:?}"),
        }
    }
    assert!(
        child.regions().len() >= 3,
        "the range does not span enough regions to have a crash window: {}",
        child.regions().len()
    );
}

/// **A reclaim survives `kill -9` between two chunks and finishes after a restart.**
///
/// The safepoint gate, the chunked walk, the persisted cursor and the crash are all here at once
/// because that is the only arrangement in which the record's ordering matters: it is written and
/// synced before the first delete, so a process that dies mid-range leaves a successor something
/// to finish rather than an unreachable range with keys in it.
#[test]
fn a_reclaim_survives_a_kill_and_finishes_after_the_restart() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let mut child = Serving::start(dir.path());

    split_into_three(&child);

    // Below the safepoint nothing may be deleted; the answer says so rather than failing.
    child.led(b"d");
    refuses_below_the_safepoint(&child);

    // Open the gate, then take one chunk and stop.
    child.call(TxnKvReq::GcSafepoint {
        safepoint: BELOW_TS,
    });
    let first = child.call(TxnKvReq::ReclaimRange {
        start: Bytes::from_static(b"d"),
        end: Bytes::from_static(b"h"),
        below_ts: BELOW_TS,
    });
    let TxnKvResp::ReclaimRange {
        cursor: after_one,
        finished: done_in_one,
        safepoint,
    } = first
    else {
        panic!("answered {first:?}")
    };
    assert_eq!(safepoint, BELOW_TS, "the gate did not open");
    assert!(
        after_one > Bytes::from_static(b"d"),
        "the pass that was let through cleared nothing: cursor {after_one:?}"
    );
    // **The kill has to land mid-reclaim or this test proves nothing.** One pass is one region, so
    // with three regions under the range the first pass must leave work behind; if it ever
    // finishes here the splits above stopped working and the crash window has quietly closed.
    assert!(
        !done_in_one,
        "the whole range was reclaimed in one pass, so the kill below is not mid-reclaim"
    );
    assert!(
        after_one < Bytes::from_static(b"h"),
        "the cursor reached the end in one pass: {after_one:?}"
    );

    // **kill -9 here.** Whatever the store had done is on disk or is not; nothing tidies up.
    child.kill();

    // A fresh process on the same directory. It has never seen the request.
    let mut restarted = Serving::start(dir.path());

    // **The gate is re-applied across the crash, and that is a safety property rather than an
    // inconvenience.** The safepoint is PD's to publish and lives in memory, so a store that has
    // just come up has not learned one — and a restarted store that carried on clearing on the
    // strength of a record written before the crash would be deleting below a floor it no longer
    // knows. The first answer after the restart is therefore blocked, and says so by echoing the
    // safepoint it is working to.
    let resumed = restarted.call(TxnKvReq::ReclaimRange {
        start: Bytes::from_static(b"d"),
        end: Bytes::from_static(b"h"),
        below_ts: BELOW_TS,
    });
    let TxnKvResp::ReclaimRange {
        cursor: resumed_at,
        finished,
        safepoint: after_restart,
    } = resumed
    else {
        panic!("answered {resumed:?}")
    };
    assert!(
        !finished,
        "a store that has learned no safepoint reported done"
    );
    assert!(
        after_restart < BELOW_TS,
        "the restarted store kept a safepoint it was never told again: {after_restart}"
    );
    // And it resumed from the record rather than from the beginning.
    assert_eq!(
        resumed_at, after_one,
        "the restart lost the cursor the first process persisted"
    );

    restarted.call(TxnKvReq::GcSafepoint {
        safepoint: BELOW_TS,
    });
    let mut passes = 0;
    loop {
        let answer = restarted.call(TxnKvReq::ReclaimRange {
            start: Bytes::from_static(b"d"),
            end: Bytes::from_static(b"h"),
            below_ts: BELOW_TS,
        });
        let TxnKvResp::ReclaimRange { finished, .. } = answer else {
            panic!("answered {answer:?}");
        };
        passes += 1;
        assert!(passes < 20, "the resumed walk did not terminate");
        if finished {
            break;
        }
    }
    restarted.kill();

    // Read the database directly, with nothing serving it: the range is empty and its neighbours
    // are not.
    let store = esker_store::Store::open(dir.path(), esker_store::StoreOptions::new())
        .expect("the store reopens");
    for key in INSIDE {
        assert!(
            !holds(store.db(), key),
            "{key:?} was inside the reclaimed range and survived"
        );
    }
    for key in OUTSIDE {
        assert!(
            holds(store.db(), key),
            "{key:?} was outside the reclaimed range and was taken"
        );
    }
}
