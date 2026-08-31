//! The bank test: N accounts, M clients, random transfers, and money that is never created or
//! destroyed (`prompts/05-txn.md`, "Tests").
//!
//! Two claims, and they catch different failures:
//!
//! * **The sum of every balance, read at one snapshot ts, is what it was at the start.** A
//!   transfer moves money between two accounts inside one transaction, so no interleaving of
//!   them can change the total — *if* transactions are atomic and reads see a snapshot. This
//!   holds no matter how many transactions failed, were abandoned or were rolled back, which is
//!   what makes it assertable under faults at all.
//! * **Every transfer the client was told committed is visible afterwards.** Each transfer
//!   writes a **witness** key in the same transaction as the two balances, always in the region
//!   the primary is *not* in. The first claim would still hold if a committed transaction lost
//!   one of its keys on a secondary — the two balances could both have landed and the witness
//!   not — and this is the claim that would not.
//!
//! The second one is not a formality: `tests/txn_crash_boundaries.rs` documents the resolution
//! bug it was written to catch, where a reader that met a committed transaction's leftover lock
//! rolled it *back*.
//!
//! # What the run does to itself
//!
//! * **Contention.** Few accounts and several clients, so transactions collide on purpose:
//!   every collision is a lock met, resolved or waited out, and a prewrite conflict that must
//!   refuse the loser rather than lose an update.
//! * **Crashed clients.** Some transfers are driven by hand and abandoned mid-commit — after
//!   the primary's prewrite, after the secondaries', or after the primary's *commit*. The locks
//!   they leave are cleaned up by whichever live client next meets them, which is the
//!   TTL-and-resolution path, and nothing else in the suite exercises it under load.
//! * **Leader kills.** A region's leader is killed and restarted underneath the traffic, so
//!   every phase of a two-phase commit can land on one leader and be read by another.
//!
//! # What a client may and may not conclude
//!
//! `commit()` answering `Ok` means the primary's `write` record is durable, so the transfer
//! happened. A `TxnConflict` or a `TxnSettled` is the *store's* determination that it did not.
//! **Anything else is unknown** — an ambiguous answer is exactly the case Percolator is built
//! around (`docs/DESIGN.md` §10), and a run that recorded those as failures would be asserting
//! something it does not know. So an unknown transfer's fate is read out of its witness key at
//! the end, which is the same thing any other client would do, and the reconciliation is exact
//! rather than approximate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_base::rng::Pcg32;
use esker_client::wire::{Body, Response, TxnKvReq, TxnKvResp, TxnMutation, TxnStatus};
use esker_client::{Error, Router, TxnClient};

#[path = "txn_cluster/mod.rs"]
mod txn_cluster;

use txn_cluster::{Cluster, Topology};

/// Accounts either side of the region boundary. Half are named `a..`, half `n..`, so the
/// audit's scan has to walk both regions and a transfer between two of them is usually a
/// transaction whose keys are in two Raft groups.
const ACCOUNTS_PER_REGION: u64 = 8;
const ACCOUNTS: u64 = ACCOUNTS_PER_REGION * 2;

/// What every account starts with. Large enough that a transfer is rarely refused for want of
/// funds, small enough that an overflow could never hide a mistake.
const OPENING_BALANCE: u64 = 1_000;

/// The invariant, in one number.
const TOTAL: u64 = OPENING_BALANCE * ACCOUNTS;

/// Most one transfer moves.
const MAX_TRANSFER: u64 = 50;

/// The lease a transfer's locks are taken with. Short, because a crashed client's locks are
/// cleaned up by the next reader to meet them and a sixty-second run should not spend three
/// seconds of it waiting out each one.
const TTL_MS: u64 = 400;

/// One account's key. `a00…` in the low region, `n00…` in the high one.
fn account_key(account: u64) -> Vec<u8> {
    if account < ACCOUNTS_PER_REGION {
        format!("a{account:02}").into_bytes()
    } else {
        format!("n{:02}", account - ACCOUNTS_PER_REGION).into_bytes()
    }
}

/// The key that says a transfer happened. `w…`, so it is in the high region and — for every
/// transfer whose primary is a low-region account — in a different Raft group from the primary.
fn witness_key(client: u64, sequence: u64) -> Vec<u8> {
    format!("w{client:03}-{sequence:08}").into_bytes()
}

/// The account range the audit scans: past every account key, short of every witness.
const ACCOUNTS_FROM: &[u8] = b"a";
const ACCOUNTS_TO: &[u8] = b"o";

fn balance_bytes(amount: u64) -> Bytes {
    Bytes::copy_from_slice(&amount.to_le_bytes())
}

fn balance_of(value: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    assert_eq!(value.len(), 8, "a balance is eight bytes, got {value:?}");
    bytes.copy_from_slice(value);
    u64::from_le_bytes(bytes)
}

/// One transfer, as a client decides to attempt it.
#[derive(Debug, Clone)]
struct Move {
    from: u64,
    to: u64,
    amount: u64,
    /// The key written alongside the two balances, in the same transaction, that says this
    /// transfer happened.
    witness: Vec<u8>,
}

/// One transfer, as the run remembers it.
#[derive(Debug, Clone)]
struct Transfer {
    at: Move,
    outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The client was told it committed. It **must** be visible afterwards.
    Committed,
    /// The store determined it did not commit: a write conflict, or someone else settled it.
    Refused,
    /// No usable answer came back. Its witness decides.
    Unknown,
}

/// How one run is shaped.
#[derive(Debug, Clone)]
struct Plan {
    seed: u64,
    clients: usize,
    duration: Duration,
    /// Leader kills, spread across the run. Zero for a topology with nothing to fail over to.
    kills: u32,
    /// How often a transfer is driven by hand and abandoned part-way.
    crash_rate: f64,
    topology: Topology,
}

impl Plan {
    /// The short run that goes in `just check`.
    fn fast(seed: u64) -> Self {
        Self {
            seed,
            clients: 4,
            duration: Duration::from_secs(3),
            kills: 1,
            crash_rate: 0.10,
            topology: Topology::two_regions(seed),
        }
    }

    /// The acceptance run: sixty seconds under the fault plan.
    fn sixty_seconds(seed: u64) -> Self {
        Self {
            duration: Duration::from_secs(60),
            kills: 12,
            clients: 6,
            ..Self::fast(seed)
        }
    }

    /// One of a thousand seeds: the same shape, in a fraction of the time.
    fn one_seed(seed: u64) -> Self {
        Self {
            duration: Duration::from_millis(1_200),
            kills: 1,
            ..Self::fast(seed)
        }
    }
}

/// What one client did, for the report at the end.
#[derive(Debug, Default)]
struct Tally {
    committed: u64,
    refused: u64,
    unknown: u64,
    /// Transfers refused for want of funds, which never left the process.
    insufficient: u64,
    /// Transactions abandoned on purpose, part-way through their commit.
    crashed: u64,
    /// Times a client rebuilt its connections after an answer it could not use.
    reconnects: u64,
    /// Unknowns whose transaction never wrote anything: a read that could not be answered.
    /// Its witness is absent and its fate is "it did not happen".
    unknown_before_writing: u64,
    /// Unknowns from a commit that went out and was not answered — the case Percolator is
    /// built around, and the one the witness has to decide.
    unknown_after_writing: u64,
}

impl Tally {
    fn merge(&mut self, other: &Self) {
        self.committed += other.committed;
        self.refused += other.refused;
        self.unknown += other.unknown;
        self.insufficient += other.insufficient;
        self.crashed += other.crashed;
        self.reconnects += other.reconnects;
        self.unknown_before_writing += other.unknown_before_writing;
        self.unknown_after_writing += other.unknown_after_writing;
    }
}

/// Where every client records what it did.
#[derive(Debug, Default)]
struct Ledger {
    transfers: Mutex<Vec<Transfer>>,
}

impl Ledger {
    fn record(&self, transfer: Transfer) {
        self.transfers.lock().expect("the ledger").push(transfer);
    }
}

/// Opens every account at [`OPENING_BALANCE`], in one transaction per region's worth.
///
/// Retried as a whole: a cluster that has just elected can still refuse one call, and a bank
/// that started with fifteen accounts would fail the audit for a reason that is not a bug.
fn open_accounts(cluster: &Cluster) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let client = cluster
            .client_within(1, Duration::from_secs(20))
            .expect("a client to open the accounts with");
        let mut txn = client.begin().expect("a snapshot");
        for account in 0..ACCOUNTS {
            txn.put(&account_key(account), &balance_bytes(OPENING_BALANCE));
        }
        match txn.commit() {
            Ok(Some(_)) => return,
            Ok(None) => panic!("the opening transaction wrote nothing"),
            Err(error) => assert!(
                Instant::now() < deadline,
                "the accounts could never be opened: {error}"
            ),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Runs transfers until `stop`, recording every one.
fn transfer_loop(
    client_id: u64,
    cluster: &Arc<Cluster>,
    plan: &Plan,
    ledger: &Arc<Ledger>,
    stop: &Arc<AtomicBool>,
) -> Tally {
    let mut tally = Tally::default();
    let mut rng = Pcg32::from_seed(plan.seed ^ (client_id << 32));
    let Some(mut client) = cluster.client_within(client_id, Duration::from_secs(20)) else {
        return tally;
    };
    let mut router = cluster.router(client_id + 500);
    let mut sequence = 0u64;

    while !stop.load(Ordering::Relaxed) {
        sequence += 1;
        let from = rng.range_inclusive(0, ACCOUNTS - 1);
        // Never itself: a transfer from an account to itself would keep the sum right for a
        // reason that has nothing to do with transactions.
        let at = Move {
            from,
            to: (from + rng.range_inclusive(1, ACCOUNTS - 1)) % ACCOUNTS,
            amount: rng.range_inclusive(1, MAX_TRANSFER),
            witness: witness_key(client_id, sequence),
        };

        // A hand-driven transfer that stops part-way is a client that died mid-commit. It needs
        // the router rather than the transaction API, because `commit()` crosses every boundary
        // in one call and offers nowhere to die in between.
        let crashing = rng.chance(plan.crash_rate) && router.is_some();
        let outcome = if crashing {
            let Some(router) = router.as_ref() else {
                continue;
            };
            let boundary = rng.range_inclusive(0, 2);
            // `None` is "the source could not afford it": nothing was written and nothing was
            // promised, so it is not a transfer at all.
            let Some(outcome) =
                crashed_transfer(cluster, &client, router, &at, boundary, &mut tally)
            else {
                tally.insufficient += 1;
                continue;
            };
            tally.crashed += 1;
            outcome
        } else {
            let Some(outcome) = transfer(&client, &at, &mut tally) else {
                tally.insufficient += 1;
                continue;
            };
            outcome
        };

        match outcome {
            Outcome::Committed => tally.committed += 1,
            Outcome::Refused => tally.refused += 1,
            Outcome::Unknown => tally.unknown += 1,
        }
        ledger.record(Transfer { at, outcome });

        // A client that lost the store it was talking to builds a new book to find another,
        // the way `chaos_linearizability.rs` does: `TcpStores` opens its connections once.
        if outcome == Outcome::Unknown {
            tally.reconnects += 1;
            if let Some(fresh) = cluster.client_within(client_id, Duration::from_secs(5)) {
                client = fresh;
            }
            router = cluster.router(client_id + 500);
        }
    }
    tally
}

/// One transfer through the ordinary API. `None` if the source could not afford it.
fn transfer(client: &TxnClient, at: &Move, tally: &mut Tally) -> Option<Outcome> {
    let Ok(mut txn) = client.begin() else {
        tally.unknown_before_writing += 1;
        return Some(Outcome::Unknown);
    };
    let balances = (txn.get(&account_key(at.from)), txn.get(&account_key(at.to)));
    // A read that could not be answered, or an account that is not there yet. Neither is a
    // transfer, and neither wrote anything.
    let (Ok(Some(from_value)), Ok(Some(to_value))) = balances else {
        tally.unknown_before_writing += 1;
        return Some(Outcome::Unknown);
    };
    let (before_from, before_to) = (balance_of(&from_value), balance_of(&to_value));
    if before_from < at.amount {
        return None;
    }

    txn.put(
        &account_key(at.from),
        &balance_bytes(before_from - at.amount),
    );
    txn.put(&account_key(at.to), &balance_bytes(before_to + at.amount));
    txn.put(&at.witness, &balance_bytes(at.amount));
    Some(match txn.commit() {
        Ok(Some(_)) => Outcome::Committed,
        // `Ok(None)` is a transaction that wrote nothing, which this one did not — but under
        // every reading of it the transfer did not happen, which is what `Refused` says. The
        // two statuses beside it are the store's own determination of the same thing.
        Ok(None) | Err(Error::TxnConflict { .. } | Error::TxnSettled { .. }) => Outcome::Refused,
        Err(_) => {
            tally.unknown_after_writing += 1;
            Outcome::Unknown
        }
    })
}

/// A transfer driven by hand and abandoned at `boundary`: 0 after the primary's prewrite, 1
/// after the secondaries', 2 after the primary's **commit** — which is the one that committed.
///
/// The snapshot is a real transaction's, so the balances are read at the `start_ts` the
/// prewrites then use. What is missing is only the part a dead client would not have done.
fn crashed_transfer(
    cluster: &Cluster,
    client: &TxnClient,
    router: &Router,
    at: &Move,
    boundary: u64,
    tally: &mut Tally,
) -> Option<Outcome> {
    let txn = client.begin().ok()?;
    let start_ts = txn.start_ts();
    let balances = (txn.get(&account_key(at.from)), txn.get(&account_key(at.to)));
    let (Ok(Some(from_value)), Ok(Some(to_value))) = balances else {
        tally.unknown_before_writing += 1;
        return Some(Outcome::Unknown);
    };
    let (before_from, before_to) = (balance_of(&from_value), balance_of(&to_value));
    if before_from < at.amount {
        return None;
    }
    drop(txn);

    // The primary is the lowest key of the write set, which is how `TxnClient` picks one.
    let mut keys: Vec<Vec<u8>> = vec![account_key(at.from), account_key(at.to), at.witness.clone()];
    keys.sort();
    let primary = keys[0].clone();
    let value_for = |key: &[u8]| {
        if key == account_key(at.from) {
            balance_bytes(before_from - at.amount)
        } else if key == account_key(at.to) {
            balance_bytes(before_to + at.amount)
        } else {
            balance_bytes(at.amount)
        }
    };

    let prewrite = |key: &[u8]| -> Option<bool> {
        let request = TxnKvReq::Prewrite {
            start_ts,
            primary: Bytes::from(primary.clone()),
            ttl_ms: TTL_MS,
            mutations: vec![TxnMutation::Put {
                key: Bytes::copy_from_slice(key),
                value: value_for(key),
            }],
        };
        match router.call(&Body::Txn(request)).ok()?.into_txn_kv() {
            Ok(TxnKvResp::Prewrite { keys }) => {
                Some(keys.iter().all(|status| *status == TxnStatus::Ok))
            }
            _ => None,
        }
    };

    // The primary alone, first, exactly as a client would.
    match prewrite(&primary) {
        Some(true) => {}
        // Refused: someone else holds the key or committed above our snapshot. Nothing of ours
        // is anywhere, and the locks we did not take need no cleaning.
        Some(false) => return Some(Outcome::Refused),
        None => {
            tally.unknown_before_writing += 1;
            return Some(Outcome::Unknown);
        }
    }
    if boundary == 0 {
        return Some(Outcome::Refused);
    }

    for key in keys.iter().filter(|key| **key != primary) {
        match prewrite(key) {
            Some(true) => {}
            Some(false) => return Some(Outcome::Refused),
            None => {
                tally.unknown_after_writing += 1;
                return Some(Outcome::Unknown);
            }
        }
    }
    if boundary == 1 {
        return Some(Outcome::Refused);
    }

    // The commit point. After this the transaction has committed, whatever happens to the
    // client — and every key of it must be readable, which is what the ledger will check.
    let commit_ts = cluster.oracle().tso_one();
    let request = TxnKvReq::Commit {
        start_ts,
        commit_ts,
        keys: vec![Bytes::from(primary.clone())],
    };
    match router.call(&Body::Txn(request)).map(Response::into_txn_kv) {
        Ok(Ok(TxnKvResp::Commit {
            status: TxnStatus::Ok,
        })) => Some(Outcome::Committed),
        Ok(Ok(TxnKvResp::Commit { .. })) => Some(Outcome::Refused),
        _ => {
            tally.unknown_after_writing += 1;
            Some(Outcome::Unknown)
        }
    }
}

/// Why an audit could not be taken. Not failures — a cluster mid-election or a key someone is
/// mid-way through writing — but worth counting, because an audit that never succeeds is an
/// assertion that is never made and a run that says so is honest about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoAudit {
    /// The scan could not be answered: a lock that would not clear, or nobody leading.
    Unreadable,
    /// Fewer accounts came back than exist. Adding those up would turn a partial read into a
    /// broken invariant, so it is not added up.
    Short(usize),
}

/// Reads every account at **one** snapshot and answers with the total.
fn audit(client: &TxnClient) -> Result<u64, NoAudit> {
    let txn = client.begin().map_err(|_| NoAudit::Unreadable)?;
    let pairs = txn
        .scan(
            ACCOUNTS_FROM,
            ACCOUNTS_TO,
            u32::try_from(ACCOUNTS).unwrap_or(u32::MAX) * 2,
        )
        .map_err(|_| NoAudit::Unreadable)?;
    if pairs.len() != usize::try_from(ACCOUNTS).unwrap_or(usize::MAX) {
        return Err(NoAudit::Short(pairs.len()));
    }
    Ok(pairs.iter().map(|(_, value)| balance_of(value)).sum())
}

/// The whole run.
#[allow(clippy::too_many_lines, reason = "one run, told in order")]
fn bank(plan: &Plan) {
    let label = format!("seed {}", plan.seed);
    let cluster = Cluster::start(plan.topology.clone());
    assert!(
        cluster.settle(Duration::from_secs(30)),
        "{label}: the cluster never elected a leader to begin with"
    );
    open_accounts(&cluster);

    let ledger = Arc::new(Ledger::default());
    let stop = Arc::new(AtomicBool::new(false));
    let audits = Arc::new(AtomicU64::new(0));

    let clients: Vec<_> = (0..plan.clients)
        .map(|at| {
            let (cluster, plan, ledger, stop) = (
                Arc::clone(&cluster),
                plan.clone(),
                Arc::clone(&ledger),
                Arc::clone(&stop),
            );
            std::thread::spawn(move || {
                transfer_loop(at as u64 + 1, &cluster, &plan, &ledger, &stop)
            })
        })
        .collect();

    // The auditor: the sum invariant, checked at a single snapshot ts, over and over, *while*
    // the transfers and the kills are happening. Checking it only at the end would let a
    // cluster that broke it in the middle and healed itself pass.
    let attempts = Arc::new(AtomicU64::new(0));
    let unreadable = Arc::new(AtomicU64::new(0));
    let short = Arc::new(AtomicU64::new(0));
    let shortest = Arc::new(AtomicU64::new(u64::MAX));
    let auditor = {
        let (cluster, stop, audits, attempts, label) = (
            Arc::clone(&cluster),
            Arc::clone(&stop),
            Arc::clone(&audits),
            Arc::clone(&attempts),
            label.clone(),
        );
        let (unreadable, short, shortest) = (
            Arc::clone(&unreadable),
            Arc::clone(&short),
            Arc::clone(&shortest),
        );
        std::thread::spawn(move || {
            // A wider lock budget than a transfer's: an audit reads every account at one
            // snapshot, so it meets whatever any writer is holding at that instant and has to
            // wait each of them out. A transfer that gave up would simply try again; an audit
            // that gave up would be an assertion not made.
            let auditing = |cluster: &Cluster| {
                cluster
                    .client_within(999, Duration::from_secs(20))
                    .map(|client| client.with_max_lock_resolutions(32))
            };
            let Some(mut client) = auditing(&cluster) else {
                return;
            };
            // A client whose store was killed has a dead connection and fails instantly for
            // ever after: `TcpStores` opens its book once. Rebuilding after a run of failures
            // is what a real client does, and without it an auditor stops auditing at the first
            // kill while still looking busy.
            let mut consecutive = 0u32;
            while !stop.load(Ordering::Relaxed) {
                attempts.fetch_add(1, Ordering::Relaxed);
                if consecutive >= 5 {
                    consecutive = 0;
                    if let Some(fresh) = auditing(&cluster) {
                        client = fresh;
                    }
                }
                match audit(&client) {
                    Ok(total) => {
                        assert_eq!(
                            total, TOTAL,
                            "{label}: the sum of the balances at one snapshot changed — a \
                             transfer was applied to one account and not the other"
                        );
                        audits.fetch_add(1, Ordering::Relaxed);
                        consecutive = 0;
                    }
                    Err(NoAudit::Unreadable) => {
                        unreadable.fetch_add(1, Ordering::Relaxed);
                        consecutive += 1;
                    }
                    Err(NoAudit::Short(seen)) => {
                        short.fetch_add(1, Ordering::Relaxed);
                        shortest.fetch_min(seen as u64, Ordering::Relaxed);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };

    // The fault plan: a leader killed and restarted under the traffic, spread over the run.
    let mut rng = Pcg32::from_seed(plan.seed ^ 0xfa17);
    let mut killed = 0;
    let interval = plan.duration / (plan.kills.max(1) + 1);
    for _ in 0..plan.kills {
        std::thread::sleep(interval);
        let group =
            usize::try_from(rng.range_inclusive(0, cluster.regions() as u64 - 1)).unwrap_or(0);
        let Some(at) = cluster.leader_of(group) else {
            continue;
        };
        cluster.kill(at);
        killed += 1;
        // Long enough for the survivors to notice and elect, then the victim comes back and
        // has to catch up.
        std::thread::sleep(interval / 2);
        cluster.start_node(at);
    }
    std::thread::sleep(interval);

    stop.store(true, Ordering::Relaxed);
    let mut tally = Tally::default();
    for handle in clients {
        tally.merge(&handle.join().expect("a client thread"));
    }
    auditor.join().expect("the auditor thread");

    assert!(
        cluster.settle(Duration::from_secs(60)),
        "{label}: the cluster never came back after {killed} kills"
    );
    reconcile(&cluster, &ledger, &label, &tally);
    cluster.shutdown();

    println!(
        "{label}: {} committed, {} refused, {} unknown ({} before writing anything, {} after), \
         {} short of funds, {} crashed part-way, {} of {} audits complete, {killed} leader kills",
        tally.committed,
        tally.refused,
        tally.unknown,
        tally.unknown_before_writing,
        tally.unknown_after_writing,
        tally.insufficient,
        tally.crashed,
        audits.load(Ordering::Relaxed),
        attempts.load(Ordering::Relaxed)
    );
    let fewest = match shortest.load(Ordering::Relaxed) {
        u64::MAX => "none were short".to_owned(),
        seen => format!("fewest accounts seen: {seen}"),
    };
    println!(
        "{label}: audits not taken — {} unreadable, {} short ({fewest})",
        unreadable.load(Ordering::Relaxed),
        short.load(Ordering::Relaxed)
    );
    assert!(
        tally.committed > 0,
        "{label}: not one transfer committed, so the run proves nothing"
    );
    assert!(
        audits.load(Ordering::Relaxed) > 0,
        "{label}: the sum was never read at a complete snapshot"
    );
}

/// The two claims, checked against what the run recorded.
fn reconcile(cluster: &Cluster, ledger: &Ledger, label: &str, tally: &Tally) {
    let client = cluster
        .client_within(1_000, Duration::from_secs(30))
        .expect("a client for the reconciliation");
    let transfers = ledger.transfers.lock().expect("the ledger").clone();
    let witnesses = all_witnesses(&client)
        .unwrap_or_else(|| panic!("{label}: the witnesses could not be read back"));

    // Every transfer's fate, read at a snapshot above all of them. An unknown one is decided by
    // its witness — the same way any other client would decide it — and a *committed* one must
    // be there, which is the claim that a lost secondary commit breaks.
    let mut applied: Vec<&Move> = Vec::new();
    let mut resolved_unknown = 0u64;
    for transfer in &transfers {
        let seen = witnesses.contains(&transfer.at.witness);
        match transfer.outcome {
            Outcome::Committed => {
                assert!(
                    seen,
                    "{label}: a transfer the client was told committed is not visible \
                     afterwards — {} → {} of {}, witness {:?}. A key of a committed \
                     transaction was lost.",
                    transfer.at.from, transfer.at.to, transfer.at.amount, transfer.at.witness
                );
                applied.push(&transfer.at);
            }
            Outcome::Refused => assert!(
                !seen,
                "{label}: a transfer the store refused is visible anyway — {} → {} of {}",
                transfer.at.from, transfer.at.to, transfer.at.amount
            ),
            Outcome::Unknown => {
                resolved_unknown += 1;
                if seen {
                    applied.push(&transfer.at);
                }
            }
        }
    }

    // And the balances are exactly the opening ones plus what was applied. This is stronger
    // than the sum: a pair of transfers that cancelled out would keep the total right and this
    // wrong.
    let mut expected = vec![OPENING_BALANCE; usize::try_from(ACCOUNTS).unwrap_or(0)];
    for transfer in &applied {
        let from = usize::try_from(transfer.from).unwrap_or(0);
        let to = usize::try_from(transfer.to).unwrap_or(0);
        // A transfer only ever left an account it could afford, so this cannot go below zero
        // unless the same transfer was applied twice — which is worth saying out loud rather
        // than wrapping past in a release build.
        expected[from] = expected[from]
            .checked_sub(transfer.amount)
            .unwrap_or_else(|| {
                panic!(
                    "{label}: account {} was overdrawn by the transfers this run believes happened",
                    transfer.from
                )
            });
        expected[to] += transfer.amount;
    }
    for account in 0..ACCOUNTS {
        let key = account_key(account);
        let value = read_with_retries(&client, &key)
            .unwrap_or_else(|| panic!("{label}: account {account} could not be read back"));
        assert_eq!(
            balance_of(&value),
            expected[usize::try_from(account).unwrap_or(0)],
            "{label}: account {account} does not hold its opening balance plus every transfer \
             that was applied to it ({} committed, {} unknown resolved by witness)",
            tally.committed,
            resolved_unknown
        );
    }

    let total: u64 = expected.iter().sum();
    assert_eq!(
        total, TOTAL,
        "{label}: the expected balances do not add up, which is a bug in this test"
    );
}

/// One read, retried while the cluster is still settling.
///
/// `None` covers both "never answered" and "not there", because for the keys this is used on —
/// the accounts, after the run — either one is the same failure and the caller says so once.
fn read_with_retries(client: &TxnClient, key: &[u8]) -> Option<Bytes> {
    for _ in 0..60 {
        if let Ok(txn) = client.begin()
            && let Ok(value) = txn.get(key)
        {
            return value;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Every witness key in the cluster, at one snapshot, in pages.
///
/// One `get` per transfer would be tens of thousands of round trips after a long run; a paged
/// scan is a handful. The pages are all read by **one** transaction, so what comes back is one
/// snapshot rather than a series of them — and the locks a crashed client left are resolved on
/// the way, which is the same work any reader would do.
fn all_witnesses(client: &TxnClient) -> Option<std::collections::BTreeSet<Vec<u8>>> {
    /// Witness keys per round trip. Well inside the frame limit at nine bytes of value each.
    const PAGE: u32 = 1_000;

    for _ in 0..30 {
        let Ok(txn) = client.begin() else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let mut found = std::collections::BTreeSet::new();
        let mut cursor = b"w".to_vec();
        let complete = loop {
            let Ok(page) = txn.scan(&cursor, b"x", PAGE) else {
                break false;
            };
            let Some((last, _)) = page.last() else {
                break true;
            };
            cursor = last.to_vec();
            cursor.push(0);
            let before = found.len();
            found.extend(page.iter().map(|(key, _)| key.to_vec()));
            // A page that added nothing new would loop for ever: the cursor did not move past
            // what it returned, which is a bug in the walk rather than an empty range.
            if found.len() == before {
                break true;
            }
        };
        if complete {
            return Some(found);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// The run `just check` does: a few seconds, one leader kill, crashed clients throughout.
#[test]
fn money_is_never_created_or_destroyed() {
    bank(&Plan::fast(20_260_831));
}

/// The acceptance run of `prompts/05-txn.md`: sixty seconds under the fault plan.
///
/// ```text
/// cargo test -p esker-client --release --test bank -- --ignored --nocapture sixty
/// ```
#[test]
#[ignore = "the sixty-second acceptance run"]
fn sixty_seconds_of_transfers_under_faults() {
    bank(&Plan::sixty_seconds(20_260_831));
}

/// The acceptance run's other half: a thousand seeds, each a whole cluster of its own.
///
/// Every failure names its seed, in the message and in the line printed before the run starts,
/// so a failing seed can be re-run alone with `ESKER_BANK_SEED`.
///
/// ```text
/// cargo test -p esker-client --release --test bank -- --ignored --nocapture thousand
/// ```
#[test]
#[ignore = "a thousand clusters; tens of minutes"]
fn a_thousand_seeds() {
    let seeds: u64 = std::env::var("ESKER_BANK_SEEDS")
        .ok()
        .and_then(|count| count.parse().ok())
        .unwrap_or(1_000);
    let first: u64 = std::env::var("ESKER_BANK_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(1);
    let started = Instant::now();
    for seed in first..first + seeds {
        println!("--- seed {seed} ---");
        bank(&Plan::one_seed(seed));
    }
    println!("{seeds} seeds in {:?}", started.elapsed());
}
