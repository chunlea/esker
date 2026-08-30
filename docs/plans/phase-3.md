# Phase 3 plan — Raft as a pure state machine, and the machinery that tries to break it

Status: **in progress**. Written before implementation; §9 records progress and §10 what changed.
Spec: `prompts/03-raft.md`. Constitution: `CLAUDE.md` (invariant 4 is this phase's whole subject).
Design: `docs/DESIGN.md` §5, §11, §14. Companion: `docs/raft-spec.md` maps every rule to its function.

Phases 0–2 are accepted and frozen. This phase adds the one component the rest of the system is
shaped around: a Raft that contains **no threads, no timers, no sockets, no file I/O and no wall
clock**. Time enters through `tick()`, messages through `step()`, every effect leaves through
`Ready`. That purity is not an aesthetic preference — it is what lets a discrete-event simulator
run ten thousand seeded fault schedules and a model checker enumerate a small cluster exhaustively.
An implementation that reaches for `Instant::now()` in one place cannot be either.

## 1. Scope, and the two lanes

Two lanes build it in parallel, against the types pinned in §3. `esker-raft` has exactly one writer.

| Lane | Owns | Sub-phases |
|---|---|---|
| `wy-p3-raft` | `crates/esker-raft/**`, `docs/raft-spec.md`, this plan, raft ADRs (0007+) | 3a, 3d |
| `cl-p3-sim` | `crates/esker-sim/**`, the stateright model, `docs/bench` entries it produces | 3b, 3c |

3e (one replicated region in `esker-store`) is **not in this phase's lanes** — it opens after 3a–3d
land, because it needs both a finished core and a simulator that can drive the real store code.

The dependency runs one way: the simulator is written against §3 and does not wait for the core to
be finished. `wy-p3-raft`'s first code commit lands the whole public surface compiling, with
`unimplemented!()`-free stubs where a body is not written yet, so `cl-p3-sim` can build a harness on
day one. **A change to anything in §3 is a message to the coordinator, never a unilateral edit**;
`cl-p3-sim` never edits `crates/esker-raft/**` and `wy-p3-raft` never edits `crates/esker-sim/**`.

### Steps

| # | Step | Lane |
|---|---|---|
| 0 | This plan; the public types compiling; `docs/raft-spec.md` skeleton | raft |
| 1 | Election: roles, randomised timeout from the injected RNG, RequestVote rules, vote persistence | raft |
| 2 | Replication: `next`/`match`, consistency check and conflict backoff, flow control, commit rule | raft |
| 3 | `Ready`/`advance` bookkeeping; the in-crate proptest for election safety | raft |
| 4 | ReadIndex on the leader | raft |
| 5 | Log compaction and `InstallSnapshot` (3d) | raft |
| 6 | Membership, learners, leadership transfer, pre-vote, check-quorum (3d) | raft |
| 7 | `docs/raft-spec.md` closed: every rule names its function, every feature names its tests | raft |
| 8 | `esker-sim`: `Clock`, `Network`, `FaultPlan`, seeded discrete-event loop over N `RawNode`s | sim |
| 9 | The four safety checkers + the liveness check; seed printing and compact traces | sim |
| 10 | The stateright model of a 3-node cluster, bounded, under two minutes in CI | sim |

## 2. File list

```
crates/esker-raft/Cargo.toml            bytes, thiserror, tracing, esker-base; dev: proptest
crates/esker-raft/src/lib.rs            module tree, timing constants, the crate's invariants
crates/esker-raft/src/error.rs          RaftError: Compacted, Unavailable, StepPeerNotFound, …
crates/esker-raft/src/types.rs          NodeId/Term/Index, Entry, HardState, ConfState, Snapshot   (contract)
crates/esker-raft/src/message.rs        Message and its accessors                                 (contract)
crates/esker-raft/src/storage.rs        LogStorage trait + MemStorage                             (contract)
crates/esker-raft/src/config.rs         Config, validation, the injected RNG                      (contract)
crates/esker-raft/src/progress.rs       Progress, Inflights, ProgressMap (Vec-backed, sorted)
crates/esker-raft/src/log.rs            RaftLog: the unstable tail over LogStorage
crates/esker-raft/src/core.rs           Raft: roles, step() dispatch, tick, term/vote transitions
crates/esker-raft/src/election.rs       campaign, RequestVote, pre-vote, check-quorum
crates/esker-raft/src/replication.rs    AppendEntries both sides, commit advancement
crates/esker-raft/src/snapshot.rs       InstallSnapshot, restore, compaction bookkeeping
crates/esker-raft/src/conf.rs           ConfChange encoding, apply-at-append, truncation rollback
crates/esker-raft/src/readonly.rs       ReadIndex rounds and ReadState
crates/esker-raft/src/raw_node.rs       RawNode, Ready, advance                                   (contract)
crates/esker-raft/tests/**              rule-by-rule unit tests, the election-safety proptest
docs/raft-spec.md                       Figure 3.1 condensed, every rule → its function
docs/adr/0007-*.md                      the first decision the dissertation leaves open
```

Every file stays under ~800 lines; `core.rs` is split the moment it approaches it.

## 3. The contract

**Pinned.** `cl-p3-sim` codes against exactly this. Shapes are binding; field-level additions that
do not change an existing meaning are allowed and reported, anything else is escalated.

```rust
pub type NodeId = u64;
pub type Term   = u64;
pub type Index  = u64;

pub enum EntryKind { Normal, ConfChange }
pub struct Entry { pub term: Term, pub index: Index, pub kind: EntryKind, pub data: Bytes }

pub struct HardState { pub term: Term, pub voted_for: Option<NodeId>, pub commit: Index }
pub struct ConfState { pub voters: Vec<NodeId>, pub learners: Vec<NodeId> }   // sorted, deduped
pub struct SnapshotMeta { pub index: Index, pub term: Term, pub conf: ConfState }
pub struct Snapshot { pub meta: SnapshotMeta, pub data: Bytes }
pub struct ReadState { pub index: Index, pub ctx: Bytes }

pub enum ConfChangeKind { AddVoter, AddLearner, Remove }
pub struct ConfChange { pub kind: ConfChangeKind, pub node: NodeId, pub context: Bytes }

pub enum Message {                       // every variant carries { from, to, term }
  RequestVote { from, to, term, last_log_index, last_log_term, pre_vote: bool, force: bool },
  RequestVoteResponse { from, to, term, granted: bool, pre_vote: bool },
  AppendEntries { from, to, term, prev_log_index, prev_log_term,
                  entries: Vec<Entry>, leader_commit: Index, context: Bytes },
  AppendEntriesResponse { from, to, term, reject: bool, index: Index,
                          hint_term: Term, context: Bytes },
  InstallSnapshot { from, to, term, snapshot: Snapshot },
  TimeoutNow { from, to, term },
  ReadIndex { from, to, term, ctx: Bytes },
  ReadIndexResponse { from, to, term, index: Index, ctx: Bytes },
}
impl Message { fn sender(&self) -> NodeId; fn recipient(&self) -> NodeId; fn term(&self) -> Term; }

pub struct InitialState { pub hard_state: HardState, pub conf_state: ConfState }
pub trait LogStorage {
  fn initial_state(&self) -> Result<InitialState>;
  fn entries(&self, low: Index, high: Index, max_bytes: u64) -> Result<Vec<Entry>>; // [low, high)
  fn term(&self, index: Index) -> Result<Term>;
  fn first_index(&self) -> Result<Index>;
  fn last_index(&self) -> Result<Index>;
  fn snapshot(&self) -> Result<Snapshot>;
}
pub struct MemStorage;                   // in-memory LogStorage for tests and the simulator

pub struct Config {
  pub id: NodeId, pub voters: Vec<NodeId>, pub learners: Vec<NodeId>,
  pub election_tick: (u64, u64),         // inclusive, default (10, 20)
  pub heartbeat_tick: u64,               // default 2
  pub max_inflight_msgs: usize,          // default 256
  pub max_size_per_msg: u64,             // append batching bound, in bytes
  pub pre_vote: bool, pub check_quorum: bool,
  pub applied: Index,
  pub rng: esker_base::rng::Pcg32,       // the only randomness in the crate
}

pub struct Ready {
  pub hard_state: Option<HardState>, pub entries: Vec<Entry>, pub snapshot: Option<Snapshot>,
  pub messages: Vec<Message>, pub committed_entries: Vec<Entry>, pub read_states: Vec<ReadState>,
}
impl<S: LogStorage> RawNode<S> {
  fn new(config: Config, storage: S) -> Result<Self>;
  fn tick(&mut self);
  fn step(&mut self, msg: Message) -> Result<()>;
  fn propose(&mut self, data: Bytes) -> Result<()>;
  fn propose_conf_change(&mut self, cc: ConfChange) -> Result<()>;
  fn read_index(&mut self, ctx: Bytes);
  fn campaign(&mut self) -> Result<()>;            // the simulator forces an election deterministically
  fn transfer_leader(&mut self, target: NodeId);
  fn has_ready(&self) -> bool;
  fn ready(&mut self) -> Ready;
  fn advance(&mut self, rd: &Ready);
  fn role(&self) -> Role;  fn term(&self) -> Term;  fn leader(&self) -> Option<NodeId>;
  fn status(&self) -> Status;                      // what the checkers read
}
```

Three shapes deserve their reason:

**Heartbeats are `AppendEntries` with no entries** (`prompts/03-raft.md` 3a). One message type means
one consistency check, one commit-index carrier, one rejection path — the split in the original
paper exists for exposition, not for the algorithm. The `context` field is what a ReadIndex round
rides on, and is empty on an ordinary append.

**A snapshot is acknowledged with `AppendEntriesResponse`**, not a message of its own. Installing a
snapshot is a jump of the follower's log to `meta.index`, and its acknowledgement answers exactly
the question an append response answers: what index does this follower now match?

**`Snapshot.data` is opaque to this crate.** The core reads `meta` — index, term, configuration —
and never the bytes; the store streams them (`docs/DESIGN.md` §5). This is invariant 7 applied to
consensus: Raft moves a snapshot's identity, not its meaning.

## 4. The driver contract

`Ready` is the only way an effect leaves the core, and the order in which the driver discharges it
is part of Raft's safety argument, not an implementation detail. The doc comment on `Ready` is the
normative text; this is its summary, and `cl-p3-sim` tests violations of it deliberately.

1. **Persist `hard_state` and `entries`, with fsync, before sending any of `messages`.**
   `hard_state` carries `voted_for`: answering a RequestVote before the vote is durable is how a
   node votes twice for the same term across a crash, which elects two leaders. Entries have the
   same shape of problem one level up: a leader that counts an acknowledgement for an entry a
   follower has not durably written can commit an entry that a crash then loses.
2. **Apply `snapshot` before `entries`** when both are present; the snapshot replaces the log
   prefix the entries continue.
3. **Apply `committed_entries` in order**, to the state machine, exactly once. They are already
   durable — they were in some earlier `Ready`'s `entries`, or in a snapshot.
4. **`read_states` may be answered only after `committed_entries` up to their index are applied.**
5. **Then call `advance(&rd)`.** Nothing already returned is ever returned again; the core resumes
   from the indices `advance` records.

A driver that sends before persisting is not slightly wrong. It is a cluster that loses
acknowledged writes, and the simulator's job is to prove that we notice.

## 5. Test list

| Area | Tests |
|---|---|
| Figure 3.1 | one unit test per numbered rule in `docs/raft-spec.md`; the spec names the test |
| election | timeout randomised per election from the injected RNG, not fixed at boot; two nodes on the same seed diverge; a candidate that loses reverts to follower; the up-to-date check (§5.4.1) grants and refuses in all four orderings of (term, index) |
| vote durability | the `Ready` that carries a granted RequestVoteResponse carries the `HardState` recording it, in that same `Ready` — never a later one |
| replication | consistency check rejects and the leader backs off by the hint; batching respects `max_size_per_msg`; `max_inflight_msgs` stops a runaway leader; a follower truncates a conflicting suffix |
| commit rule §5.4.2 | **a prior-term entry replicated to a majority does not commit alone**; it commits when a current-term entry above it does |
| Ready/advance | nothing re-emitted after `advance`; `has_ready` is false immediately after; a dropped (never advanced) `Ready` is re-offered unchanged |
| ReadIndex | a read state appears only after a heartbeat quorum in the current term; a leader that loses quorum produces none; a follower forwards and answers from the response |
| snapshot | restore resets the log and the configuration; a snapshot older than the log is ignored; one that overlaps the log truncates it; a snapshot ahead of the log replaces it wholesale |
| membership | config applies at **append**, not commit; **a truncated uncommitted ConfChange reverts the config**; a removed leader steps down; learners do not count toward quorum; only one uncommitted change at a time |
| transfer | `TimeoutNow` makes the target campaign immediately; a transfer to a lagging target waits for it to catch up; a transfer that times out is abandoned |
| pre-vote + check-quorum | **both on together**: a partitioned leader steps down, the returning node's pre-vote fails and does not bump the cluster's term |
| robustness | `step()` never panics: every message kind × every role × (stale, current, future) term × (known, unknown, removed) sender |
| property (in-crate) | random interleavings of the messages a 3–5 node cluster generates never yield two leaders in one term |
| simulation (`cl-p3-sim`) | election safety, log matching, leader completeness, state-machine safety after every event; liveness after faults heal; 10,000 seeds |
| model (`cl-p3-sim`) | the same four properties, exhaustively, on a bounded 3-node model |

## 6. The race list

The orderings that break Raft implementations. Each is a named test in this lane, and each is a
`FaultPlan` shape `cl-p3-sim` can reach; the simulator is what finds the ones this list misses.

1. **Vote vs. crash.** RequestVote answered, node crashes before the vote is durable, restarts and
   votes again in the same term. Prevented by §4 rule 1; tested by the `Ready`-content test, because
   the core cannot test the driver.
2. **Commit vs. term change (§5.4.2).** A leader replicates a prior-term entry to a majority, then
   fails; a new leader with a shorter log may still overwrite it. Counting replicas commits it; the
   term check does not.
3. **Append vs. truncation of a ConfChange.** The config applies when the entry is appended, so a
   leader that appends `add-node`, loses leadership, and has the entry truncated must revert to the
   previous configuration — otherwise it counts a quorum over members that were never added.
4. **Election vs. snapshot.** A follower installing a snapshot receives a RequestVote from a
   candidate whose log is shorter than the snapshot's index; its own last index must come from the
   snapshot metadata, not the (now empty) log tail.
5. **Snapshot vs. append in flight.** The leader sends a snapshot and appends behind it; the
   follower must not accept an AppendEntries whose `prev_log_index` precedes the snapshot it just
   installed, and the leader must not treat the pending snapshot's index as matched until it acks.
6. **Check-quorum vs. pre-vote.** A partitioned leader steps down under check-quorum while the
   returning node's pre-vote is in flight; the pre-vote must not bump terms, and the step-down must
   not leave a stale `leader` pointer that answers reads.
7. **Leadership transfer vs. a new proposal.** After `TimeoutNow` is sent, the outgoing leader must
   stop accepting proposals; one accepted in the window is a proposal that no leader owns.
8. **Read vs. leadership loss.** A ReadIndex round confirmed by a quorum, then the leader is
   partitioned before the read state is answered. The read is linearizable at its index; the driver
   must apply to that index before answering, which is §4 rule 4.
9. **Removed peer's late vote.** A vote response from a peer removed by a config change applied at
   append arrives afterwards; it must be ignored without panicking and without counting.
10. **Duplicate and reordered appends.** The same AppendEntries delivered twice, and out of order.
    Idempotence comes from the consistency check, not from a sequence number.

## 7. Risks

1. **The core grows a hidden clock.** Every convenience that reads time or entropy outside `tick()`
   and the injected RNG destroys the simulator's value. Mitigation: `esker-raft` has no
   `std::time` import at all, and its `Cargo.toml` carries no dependency that could provide one.
2. **`HashMap` iteration in a decision path.** Two runs with the same seed must produce the same
   trace; iterating a `HashMap` breaks that silently and only under some seeds. Mitigation: progress
   is a sorted `Vec`, and no `HashMap` appears in the crate.
3. **The unstable-log seam.** Every serious bug in an etcd-shaped Raft lives where the in-memory
   tail meets storage: an off-by-one in `first_index`, a snapshot that lands while entries are
   unstable. Mitigation: `RaftLog` is one module with its own unit tests, and index arithmetic goes
   through named helpers rather than being written out at each call site.
4. **Contract churn hurts the sibling.** Every change to §3 costs `cl-p3-sim` a rebuild of its
   harness. Mitigation: the whole surface lands compiling in the first code commit, before any
   behaviour, so the shapes are exercised by a real caller early.
5. **A test suite that only tests the happy path.** Rule-by-rule tests confirm what the code does;
   they do not find what it forgot. Mitigation: the property test and the simulator are the actual
   coverage, and the race list above is written before the code, not after.

## 8. Non-goals for this phase

- **Joint consensus.** Single-server changes only, one at a time (`docs/DESIGN.md` §5); joint
  consensus is an ADR when a use case needs it.
- **Lease reads.** ReadIndex only. Leases need a bounded clock skew assumption, which is a
  deliberate decision to make later, not a shortcut to take now.
- **Snapshot bytes.** The core moves metadata; streaming is the store's, in 3e.
- **Batching proposals across regions, apply-thread pipelining, async fsync.** Performance work
  after correctness, with a profile (`CLAUDE.md`).
- **`esker-store` integration (3e).** Out of both lanes, by the gate.
- **Witness/non-voting replicas beyond learners, and pre-vote for learners.**

## 9. Progress

- [x] step 0 — this plan
- [x] step 0 — the public types compiling; `docs/raft-spec.md` skeleton
- [x] step 1 — election
- [x] step 2 — replication
- [x] step 3 — Ready/advance, the election-safety proptest
- [x] step 4 — ReadIndex
- [x] step 5 — compaction and `InstallSnapshot`
- [x] step 6 — membership, learners, transfer, pre-vote, check-quorum
- [x] step 7 — `docs/raft-spec.md` closed; ADRs 0007 and 0008; `docs/DESIGN.md` §5 realigned

## 10. Changes vs plan

The raft lane's steps 0–7 are done. What differs from what §3 and §5 said before the code existed:

1. **`RawNode::advance` takes `&Ready`, not `Ready`.** The driver needs the value after advancing —
   to answer reads, to log what it wrote — and taking it by value forced a clone at every call site.

2. **`Ready::messages` are taken, not re-offered.** §4 rule 5 originally said a dropped `Ready` is
   re-offered "unchanged". That is true of the state and false of the messages, which `ready()`
   moves out. It is safe (the network may lose a message anyway, so Raft already retries
   everything), but the text was wrong and is now corrected in both places.

3. **`AppendEntries` gained a `context` field**, and its response echoes it. Folding heartbeats into
   appends (§3) left a `ReadIndex` round with nothing to ride on; etcd puts the context on its
   separate heartbeat message, and with no separate message it has to go here. Recorded in ADR 0007.

4. **`maybe_append` reports what it truncated.** §4.1's apply-at-append rule needs its inverse —
   revert-on-truncate — and the layer above the log cannot work out what was truncated for itself.
   `AppendOutcome { last, truncated_from }` is the smallest thing that says it.

5. **Two files were split** (`election.rs`, `replication.rs`) into implementation plus a
   `tests.rs` submodule, to stay under `CLAUDE.md`'s ~800-line limit while keeping the tests'
   crate-internal access.

Five defects were found by tests during the phase, four of them by tests written before the fix:

- **The log seam.** Truncating below the unstable tail emptied it and lowered its offset, so
  `last_index()` fell through to storage and reported entries the log had just discarded — a
  follower would advertise a longer log than it holds and could win an election it has no right to.
- **A follower returning from a partition** was heartbeated forever and never sent the entries it
  was missing: the leader only replicated when a response moved something, and a partition drops
  the probe that would have moved it.
- **A compacted leader never sent a snapshot** when the follower's rejection could not move `next`
  any further back. The rejection did nothing at all, so the follower stayed broken.
- **An empty append consumed the in-flight window**, so a leader with a small window throttled the
  very messages that advertise a new commit index.
- **`RawNode::campaign()` panicked on a leader** (a debug assert), found by the property test on its
  first run. It is a public method, so that is an invariant-9 violation.

Two overflow panics on adversarial input were found by the `step()` fuzz test — a snapshot index at
`u64::MAX`, and term arithmetic at `u64::MAX`. Every index arriving off the network is now clamped
to what the receiver itself holds, and the arithmetic saturates.

Every safety-critical test was checked against a deliberate mutation of the rule it covers: the
one-vote-per-term rule, §5.4.2's term condition, the `ReadIndex` quorum round, the snapshot
replacing the log, apply-at-append and revert-on-truncate. One test — the first snapshot one —
did **not** catch its mutation, because no test had an unpersisted tail when the snapshot arrived.
That interleaving is now `a_snapshot_discards_an_unpersisted_tail`, and it does.

Not done here, by the gate: 3b/3c (`esker-sim`, the sibling lane) and 3e (`esker-store`).
