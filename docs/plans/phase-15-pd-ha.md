# Phase 15 — PD high availability

Status: in progress · Lane: `pdha` · Branch cut from `main` at `c9afbd1`

This is 4e, deferred at the phase-4 gate and recorded there as the next milestone
(`docs/plans/phase-4.md` §15). It is opened as its own phase because everything above phase 4
now depends on PD: `esker-txn` cannot start a transaction without a `Tso`, and `esker-sql`
cannot serve a write without a schema lease. The single durable PD is therefore no longer a
single point of failure for *change* — it is one for the whole product.

## 1. What has to be true when this is done

**PD survives the loss of any one of three PD processes, with no loss of data and no timestamp
regression, and the cluster keeps allocating, routing and scheduling.**

Restated as the four properties the tests check:

1. **No acknowledged allocation is lost.** An id or a timestamp that left PD is never handed
   out again, whichever member is leading and whatever a `SIGKILL` interrupts.
2. **No timestamp regresses across a failover** (`CLAUDE.md` invariant 6). A new leader's first
   timestamp is strictly above every timestamp the old leader handed out.
3. **A follower never answers a question only a leader may answer.** It refuses with a hint.
4. **A store or client that holds three endpoints keeps working across a leader change**, with a
   bounded retry budget and a backoff when there is no leader at all.

## 2. What deferring cost, and what closes here

The cost table of `docs/plans/phase-4.md` §15, which this phase deletes:

| Down today | Closed by |
|---|---|
| `Tso` — no transaction can start or commit | units 1–3 |
| `AllocId` — no split, no new peer | units 1–2 |
| `Bootstrap` — no store can join | units 1–2 |
| `GetRegion` on a cold cache | units 2, 4 |
| heartbeats — repair and balance stop | units 2, 4 |

## 3. The shape, in one screen

PD keeps every rule it has. Only the meaning of *persisted* moves, from "one fsync returned" to
"a Raft entry this member has applied".

```text
                    ┌─────────────── esker-pd ───────────────┐
  store / client    │                                        │
  ──── Pd req ──────▶  PdService (async edge, leader only)    │
                    │      │ synchronous call                 │
                    │      ▼                                  │
                    │    Pd  ── propose(Command) ──▶ ┐        │
                    │      ▲                          │       │
                    │      └──── Applied ─────────────┤       │
                    │                                 ▼       │
                    │                        PdDriver (thread)│
                    │                    RawNode<PdLogStorage>│
                    │                     one Db, two CFs     │
                    └──────────┬─────────────────────┬────────┘
                               │ PdRaft batches      │
                        ┌──────▼──────┐       ┌──────▼──────┐
                        │  PD member 2 │       │ PD member 3 │
                        └─────────────┘       └─────────────┘
```

- **One database, two column families.** `default` keeps the records of ADR 0010, byte for
  byte and key for key. `raft` keeps PD's Raft log, hard state and applied index, exactly as
  `esker_store::raft_log` does for a region. One WAL, so an apply writes the record and the
  apply index in **one atomic batch** — which is what makes "applied" a single fact rather than
  two that can disagree. A 4a data directory gains the `raft` family on open; there is no
  migration.
- **Writes go through the log; reads do not.** The six records of ADR 0010 are written only by
  `apply`. `GetRegion`, `ScanRegions`, `Status` and the inspector read the engine directly, and
  the staleness that allows is the staleness `docs/DESIGN.md` §7 already tolerates: a stale
  route costs a redirect, never a wrong answer.
- **The scheduler does not move.** The in-flight set, the cooling map and the settling deltas
  stay memory, leader-only, re-derived from the next round of heartbeats. A PD that loses
  leadership is, to the scheduler, a PD that restarted — a case phase 4 already tests
  (ADR 0013, ADR 0018).
- **`esker-raft` gains nothing.** PD drives the same pure `RawNode` the store drives, with the
  same five-step `Ready` contract. If this lane finds a core gap it is written down and handed
  over, not patched in place.

### 3.1 The commands

An entry is `version:u8 ++ kind:u8 ++ fields`, hand-encoded, strictly decoded — the record
convention of ADR 0010, one level up. Every non-deterministic input the leader sampled travels
**in the command**, so that `apply` is a pure function of `(applied state, command)` and three
members reach the same state from the same log.

| Command | Carries | Apply writes |
|---|---|---|
| `TakeOffice` | `term`, `now_ms` | nothing; it is the barrier (§3.2) |
| `Bootstrap` | `store_id`, `address`, `base_id`, `cluster_id`, `now_ms` | cluster + store + region + range index, or, if a cluster exists, the store record alone |
| `ReserveIds` | `end` | `'m' 'a'` — `allocated_end = max(end, held)` |
| `AdvanceTso` | `mark` | `'m' 't'` — `high_water = max(mark, held)` |
| `StoreBeat` | the beat, `now_ms` | `'m' 's'` |
| `RegionBeat` | the beat, `now_ms` | `'m' 'r'` + `'m' 'k'`, under the epoch guard, answering `Upsert` |
| `Columnar` | the wishes | `'m' 'l'` |
| `History` | one event | `'m' 'h'` |

`max`, not assignment, on the two counters: an entry that applies twice — a restart replaying a
committed entry it had already applied is not possible, but a *stale* entry re-proposed by a
recovering leader is — must never move a mark backwards. It is a cheap way to make apply
idempotent in the direction that matters, and it is the direction the whole ADR 0010 argument
runs in.

**The wall clock is sampled once, by the leader, into the command.** Two members reading
`SystemClock::now_ms()` inside `apply` would diverge — and would be a second place in Esker
that orders on a wall clock, which invariant 6 forbids outright.

### 3.2 `TakeOffice`, and why a new leader waits

Raft guarantees a new leader's *log* holds every committed entry. It does not guarantee the new
leader has **applied** them, and PD's oracle and allocator are read out of applied state. A
leader that answered a `Tso` before applying the last `AdvanceTso` would resume below a mark
already committed — which is exactly the failure `docs/plans/phase-4.md` §15 names as "the one
thing to check first".

So on winning an election a member proposes `TakeOffice { term, now_ms }` and **serves nothing
until it applies**. When it does, every entry before it — which is every entry committed under
any earlier leader — has applied too, and the leader rebuilds its working state from the
applied records: `Allocator::load(allocated_end)` and `Oracle::load(high_water, now_ms)`, the
same two constructors a restart already uses. Until then, every request is answered
`PdNotLeader` with this member as the hint, because it is about to be the answer.

`esker-raft` already appends its own empty entry on taking office (§5.4.2, `election.rs`), and
`own_term_index` records it. `TakeOffice` is not a duplicate of it: the empty entry is what lets
the *log* commit, and this is what tells the *state machine* it has caught up. It is proposed by
the driver rather than read out of the core, so this lane adds nothing to `esker-raft`.

### 3.3 The timestamp argument, which is the whole of unit 3

Two things could hand out a colliding timestamp: a deposed leader that has not noticed, and a
new leader that starts too low. The persisted mark answers both, and it answers them with the
rule ADR 0010 already wrote — which is why unit 3 is a proof and a test rather than a mechanism.

- **Every timestamp handed out has `physical < mark`**, and the mark is committed before the
  first timestamp that would break that leaves. Under Raft, "committed" means the leader
  observed the commit, not that it proposed it: a deposed leader's proposal never commits, so
  its `tso` call **fails** rather than answering. Ack after commit, never after propose.
- **A new leader starts at `max(clock, committed mark)`**, which is `>= mark`, which is strictly
  above every `physical` the old leader could have handed out.

So the mark doubles as a lease, and PD needs no other one: the old leader is confined below the
mark by the same fsync-then-answer rule that made a *restart* safe, and the new leader begins
above it. Nothing in that argument depends on either member's clock being right, or on the two
clocks agreeing, which is what invariant 6 is for.

**The allocator's reservation is the same lease, one field over**, and the symmetry is worth
stating because it is what makes the design one rule rather than two. A deposed leader can hand
out ids without committing anything — up to `allocated_end`, and no further, because crossing it
takes a `ReserveIds` that will not commit. A new leader resumes at `allocated_end + 1`. The two
ranges cannot overlap for exactly the reason the two timestamp ranges cannot, and neither answer
needs to know whether the old leader has noticed yet.

### 3.4 The wire

Two additions to `esker-proto`. Neither changes a byte of an existing golden; both get a golden
line of their own, because the sweep in `tests/messages.rs` demands one per method and per code.

- **`Method::PdRaft = 0x030b`**, carrying `PdRaftBatch { group_id: u64, from: u64, messages:
  Vec<esker_raft::Message> }`. The `Message` codec is `esker_proto::raft`'s, unchanged — ADR 0009
  already decided the wire carries the real type, and a second copy of it for PD would be the
  mirrored-enum mistake that ADR argues against. There is no region and no epoch: PD's group is
  not a region.
- **`ProtoError::PdNotLeader { leader_id: u64, leader_address: String } = 19`.** A separate code
  from `NotLeader`, deliberately: `NotLeader` is region-scoped, its hint is a *peer* id, and the
  client's router answers it by repairing the region cache — a PD redirect routed through that
  path would poison the cache for a region that does not exist. The hint carries an **address**
  rather than a member id so that discovery needs no agreement about the order the endpoints
  were listed in.

**The group id.** PD's Raft group exists before the cluster does — `Bootstrap` is itself a log
entry — so the cluster id of ADR 0011 cannot guard its traffic. `group_id` is `mix64` over the
sorted member addresses, and a member refuses a batch that does not carry its own. It costs
eight bytes and it rules out the mistake ADR 0011 spent a section on, one layer down and with a
worse consequence: two PDs from different clusters pointed at each other by a stale flag would
otherwise form one group and replicate one cluster's routing table over the other's. Derived
rather than minted because membership is static here (§7); the ADR records what has to change
when it is not.

## 4. File list

**New**
- `crates/esker-pd/src/command.rs` — the command enum, its encoding, its strict decoder.
- `crates/esker-pd/src/machine.rs` — `apply`: the pure function from `(applied state, command)`
  to a `WriteBatch` and an answer.
- `crates/esker-pd/src/raft_log.rs` — `LogStorage` over the `raft` CF. Modelled on
  `esker_store::raft_log`, and deliberately not shared with it: that one is keyed by region.
- `crates/esker-pd/src/driver.rs` — the thread that owns the `RawNode` and enforces `Ready`'s
  five steps. The PD analogue of `esker_store::peer::PeerCore`.
- `crates/esker-pd/src/member.rs` — the member list, the group id, and the PD↔PD transport.
- `crates/esker-pd/tests/failover.rs` — three members in one process; unit 5's tests.
- `crates/esker-pd/tests/tso_window.rs` — the property test for the window.
- `docs/adr/0059-pd-is-a-raft-group.md`.

**Changed**
- `crates/esker-pd/src/pd/mod.rs` — every durable write becomes propose-and-await; the applied
  half of `State` moves to the driver.
- `crates/esker-pd/src/{alloc,tso}.rs` — no change to the state machines. Their `persist`
  callbacks become "propose and wait for the apply", which is what they were built for
  (ADR 0010, last consequence).
- `crates/esker-pd/src/service.rs` — the leader check, beside the cluster check, in one place.
- `crates/esker-pd/src/lib.rs`, `Cargo.toml` — the `esker-raft` dependency.
- `crates/esker-proto/src/{messages.rs,pd.rs,error.rs}` — the method, the batch, the code.
- `crates/esker-proto/tests/{messages.rs,golden/messages.hex}` — two golden lines added.
- `crates/esker-store/src/{pd.rs,pd_remote.rs}` — an endpoint list and the redirect.
- `crates/esker-client/src/` — the PD-endpoint side. **Declared before editing:** a new
  `pd.rs` holding the endpoint list and the redirect loop, plus the `pub use` line in `lib.rs`.
  Nothing in `router.rs`, `tcp.rs`, `retry.rs`, `txn.rs` or the region cache — `txn.rs` is the
  `h1` lane's (coordinator, 2026-09-04). If that turns out to be wrong, the coordinator hears
  about it before the edit.
- `crates/esker-cli/src/{args.rs,pd.rs,server.rs}` — `--pd-peers`, `pd members`, the leader in
  `inspect`.
- `docs/DESIGN.md` §7 and §15, `docs/plans/phase-4.md` §15 (the cost table is closed).

**Not touched:** `esker-raft` (read-only), `esker-sql`, `esker-keys`, `esker-engine`,
`esker-columnar`, and the rest of `esker-store`/`esker-client`.

## 5. Units, one commit each

0. **Plan and ADR.** This file, `docs/adr/0059-pd-is-a-raft-group.md`.
1. **The state machine behind a single-member group.** `command.rs`, `machine.rs`,
   `raft_log.rs`, `driver.rs`; `Pd::open` keeps its signature and campaigns synchronously, so a
   one-member PD is leader before `open` returns. **Every existing test passes unchanged** —
   that is the unit's acceptance criterion, and the reason it is worth a commit of its own.
2. **Three members.** `member.rs`, the `PdRaft` method, `PdNotLeader`, ticks from the service
   loop, followers refusing.
3. **TSO across failover.** The proof of §3.3 as a test that kills a leader mid-window.
4. **Stores and clients follow the hint.** Endpoint lists in `esker server`, `RemotePd` and the
   client; bounded retry, backoff when there is no leader.
5. **Tests.** `crash_kill.rs` extended, the `esker-sim` scenario, the property test, and a
   `stateright` model of leader change plus the window if the state space allows.
6. **`esker-cli`.** `pd members`, bootstrap with N endpoints, `inspect` naming the leader.
7. **Docs.** DESIGN.md §7 and §15 rewritten to what was built; the phase-4 §15 cost table closed.

## 6. Test list

| Test | Where | What it would catch |
|---|---|---|
| every 4a–4d test, unchanged | `esker-pd/tests/*` | unit 1 changing behaviour while changing the mechanism |
| `a_follower_refuses_every_method_a_leader_owns` | `failover.rs` | trap 1: a follower answering `Tso` |
| `a_timestamp_is_never_acked_before_its_mark_commits` | `failover.rs` | trap 2: ack on propose |
| `a_new_leader_starts_above_the_last_committed_window` | `failover.rs` | the §3.3 proof |
| `a_leader_killed_mid_window_loses_no_timestamp` | `crash_kill.rs` | the same, against a signal |
| `an_id_is_never_handed_out_twice_across_a_failover` | `failover.rs` | ack before commit, one command over |
| `a_deposed_leader_that_has_not_noticed_answers_nothing` | `failover.rs` | the lease argument |
| `the_survivors_serve_within_one_election_timeout` | `failover.rs` | liveness, the headline claim |
| `apply_is_a_pure_function_of_the_command` | `machine.rs` | a clock read sneaking into apply |
| the window property | `tso_window.rs` | `proptest` over interleaved allocate/kill/reload |
| a deterministic failover scenario | `esker-sim` | message loss and reordering around an election |
| leader change + window | `stateright` | if the space is small enough; §9 says if not |
| `a_batch_for_another_group_is_refused` | `member.rs` | the group-id guard |
| `a_client_with_three_endpoints_follows_the_hint` | `esker-client` | unit 4 |
| `no_leader_backs_off_rather_than_spinning` | `esker-client` | unit 4's other half |

## 7. Non-goals, said out loud

- **Dynamic PD membership change.** No `pd member add`/`remove`. The member list is
  configuration, identical on every member, and the group id is derived from it. Considered
  only if units 0–6 are done and gated, and it needs its own ADR: the group id has to move into
  the log, minted once like the cluster id.
- **More than three members.** Five would work; nothing is built or tested for it.
- **TLS between PD members.** Phase 6b's problem, unchanged.
- **A leader lease or `ReadIndex` for PD reads.** §3 says why: reads are advisory, and the two
  answers that are not go through the log.
- **Batching heartbeat applies.** One Raft round trip per heartbeat is the cost, and §9 prices it.
- **Moving the scheduler into the log.** It is memory by decision (ADR 0013).

## 8. Risks

1. **`Pd::open`'s signature is a cross-lane contract.** `esker-store`, `esker-sql` and
   `esker-cli` all call it, and two of those trees belong to other lanes. Unit 1 keeps it
   byte-identical, which is why the single-member group must reach leadership *inside* `open`
   with no runtime and no ticker.
2. **A deadlock between the state lock and the apply.** `Allocator::allocate` calls a `persist`
   that now blocks on the driver. The applied state therefore moves **out** of `Pd`'s mutex and
   onto the driver thread; nothing holds a lock the apply needs while waiting for it. Checked by
   a test that allocates from several threads at once.
3. **Throughput.** Every heartbeat is a Raft round trip, serialized. A thousand regions is ~100
   round trips a second. Measured in unit 5, recorded in `docs/bench/`; if it is a problem the
   fix is batching, and it is a non-goal until the number says otherwise.
4. **The `stateright` model may not fit.** Leader change crossed with a timestamp window is a
   large space. If it is, unit 5 says so and the property test carries the weight — as the brief
   allows.
5. **The `esker-client` blast radius.** §4 declares the files. If the endpoint list turns out to
   need `router.rs`, the coordinator decides before the edit.

## 9. Progress

- **Unit 0 — plan and ADR.** Done: `docs/adr/0059-pd-is-a-raft-group.md`.
- **Unit 1 — the state machine behind a single-member group.** Done, in two commits rather than
  one: the wire addition came first because a typed refusal is what everything above it branches
  on, and a temporary mapping onto `Internal` would have been a lie that later commits had to
  find again.
  - `feat(proto): PdNotLeader` — error code 19 and its two goldens.
  - `feat(pd): every durable write goes through a Raft log` — `raft_log`, `command`, `machine`,
    `member`, `driver`, and `Pd` rewired onto them.

  **All 187 `esker-pd` tests pass unchanged**, which was the unit's acceptance criterion, and the
  workspace is 3,093 green.

- **Units 2, 3, 4, 6 and 7 — three members, the timestamp proof, the redirect, the tools, the
  docs.** Done, and landed together rather than one commit apiece: unit 2's mechanism is only
  meaningful with unit 3's tests pointed at it, and unit 4's redirect is only reachable once a
  follower exists to refuse. What each added:
  - **Three members.** `member.rs` (the group and its id), `transport.rs` (one connection per
    member pair, a batch per tick), `Method::PdRaft` carrying ADR 0009's real `Message`, the
    leader check in `service::serve`, and the tick at the service edge.
  - **The timestamp proof.** `tests/failover.rs`: three placement drivers in one process over a
    queue the test drains by hand, so delivery is deterministic and a message can be dropped on
    purpose. It elects, cuts the leader off, moves every clock **backwards**, and asserts the
    successor's first timestamp is above the predecessor's last — and that a deposed leader that
    has not noticed cannot cross its own mark.
  - **The redirect.** `RemotePd::connect_to` takes the whole endpoint list, follows a named hint
    up to `REDIRECT_BUDGET` times, backs off when nobody will say who leads, and refuses a hint
    naming an address outside its configured set.
  - **The tools.** `esker pd serve --id --peers`, `esker pd members`, `--pd a,b,c` on
    `esker server`, and an `inspect` that shows the durable half of consensus.
  - **DESIGN.md** §7 rewritten to what was built, §15's "PD HA timing" closed and replaced by
    dynamic membership.

### 9.1 What the tests were shown red against

A green suite over a mechanism nobody has broken is evidence of nothing, so each rule was removed
in turn and `tests/failover.rs` re-run. The third row is the one worth reading.

| Mutation | Went red |
|---|---|
| `Oracle::load` resumes at the clock instead of `max(clock, mark)` | `a_new_leader_starts_above_the_last_committed_window` ("the mark did not survive the failover"), and `a_returning_leader_hands_out_nothing_its_successor_already_did` with an actual **duplicate** timestamp in the list |
| a proposal is answered when it is appended instead of when it applies | 10 of 14 |
| the leader check removed from both `Pd::leading` and `service::serve` | **1 of 14 at first, and for the wrong reason** — see below |

**The third mutation is why this section exists.** With the leader check gone,
`a_follower_refuses_every_answer_only_a_leader_may_give` still passed: a follower's `Tso` fails
*twice over* — once at the leader check, and once because `esker-raft` will not let a non-leader
propose — and the test only asked which variant came back. It was passing on the mechanism it was
not testing.

Two things came out of that. The refusal tests now assert the **address**, which only the leader
check can fill in because only `Pd` holds the member list; and a new test drives a follower through
`PdService` to prove it refuses a **read** — `GetRegion` takes no proposal, so before that test the
service's check had no coverage at all. The same mutation now turns three tests red, each naming
what it lost.

### 9.2 What the generator found

`tests/tso_window.rs` drives the oracle under `proptest`: an adversary picks the counts, picks the
clock — including a thousand years behind — decides when a mark **commits** and when it does not,
and fails the member over at will. A failover is modelled as what it is, a fresh `Oracle` loaded
from the *committed* mark.

Its first run failed, and on a claim this plan had made rather than on the code. The test asserted
that a failed commit leaves the oracle untouched — which is the **allocator's** guarantee, copied
onto the oracle by hand. The oracle does not make it: it moves its physical part before it asks,
so a commit that fails leaves a run of skipped timestamps behind. That is harmless and
`crate::tso` already says so — skipping costs nothing, and a timestamp is only ever required to be
ordered. What would be fatal is the **mark** moving without a commit, because a new leader resumes
at the committed mark and a member believing in an uncommitted one would hand out timestamps its
successor hands out again. So the assertion is now that, and the difference is written beside it.

The second test in that file is there because a green generator proves nothing if it never reaches
the case: it drives a resume with the clock *behind* the mark by hand, which is the only situation
in which `Oracle::load`'s `max` does any work at all.

### 9.3 Owed, and what each is worth

Named rather than quietly dropped. None of them is load-bearing for the units above; all of them
would add coverage the current tests do not.

- **A three-process kill test.** `tests/crash_kill.rs` now drives PD through the Raft log and
  still passes, so the *ordering* is tested against a real signal — but against a group of one.
  Killing the **leader of three** means three processes and a parent that can tell which of them
  was leading; it is the largest of these and the one with the most to find.
- **A deterministic `esker-sim` scenario.** `tests/failover.rs` drops messages by cutting a member
  off, which is a partition rather than a lossy link. A sim scenario would reorder and duplicate as
  well, over seeds.
- **A `stateright` model of leader change crossed with the window.** The state space is the
  product of Raft's and a 64-bit counter's, so it needs the counter abstracted to "before, inside,
  after the mark" before it is finite enough to check. Worth doing, and it is a design exercise
  rather than a transcription.
- **The heartbeat round trip, measured.** §8.3 prices it as ~100 round trips a second at a thousand
  regions and says the fix is batching if the number says so. `docs/bench/phase-4-pd.md` has the
  4a baseline for `tso` and `allocid` to compare against; nothing has been re-measured.
- **The real transport, end to end.** `PdRaft` has goldens and `transport.rs` has unit tests for
  its dropping rules, but no test sends a batch over a socket between two placement drivers.

### 9.4 Changes against §3 and §5, and why

- **The wire addition moved from unit 2 into unit 1** (above).
- **Snapshot and compaction were built in unit 1, not left for unit 2.** `Ready`'s snapshot arm
  had to be *something*, and a stub that logs is what left a store's peer holding a log position it
  had no data for (`docs/plans/phase-4.md` §17). PD's whole state machine is a couple of hundred
  kilobytes, so the snapshot is the `default` column family serialised as pairs and installing one
  is a `delete_range` plus the pairs — small enough that writing it was cheaper than writing down
  why it was missing. Unreachable until unit 2 puts a second member behind it, and the unit tests
  drive it directly in the meantime.
- **`Pd`'s state was split in two, which §3 did not say and the deadlock in §8.2 forces.** The
  applied records — cluster, `allocated_end`, `high_water_ms`, history, columnar wishes — moved out
  of `Pd`'s mutex into `machine::AppliedState` behind its own lock. A leader's `Allocator` calls a
  persist that blocks on the driver *while holding `Pd`'s mutex*, so the driver may never take that
  mutex; putting the two on separate locks is what makes "propose under the state lock" safe, and
  it is written at the top of `driver.rs` rather than remembered.
- **The `TakeOffice` barrier is proposed by the driver, not read out of the core.** `esker-raft`
  exposes no accessor for its own term-start index, and this lane may read that crate but not
  change it. Proposing an explicit barrier needs nothing from it and says the thing that actually
  matters — that the *state machine* has caught up, not that the log has.
- **`State` gained `office_term`.** A member rebuilds its allocator and oracle lazily, on the first
  write after taking office, rather than the driver reaching across to do it. That keeps the
  rebuild on the side of the lock that owns the state.
- **`Pd::Members` is a new method, not a field on `Pd::Status`.** The brief asks for `inspect` to
  show the leader; the honest version is two commands, because they answer two questions. `inspect`
  opens a **stopped** placement driver and can only show what it left behind — its term, its vote,
  how far it applied — and `members` asks a *running* group who leads. Adding the group to
  `PdResp::Status` would have changed a message whose bytes are frozen by a golden, which needs the
  human; a new method is an addition and needs only this note.
- **`esker pd inspect` grew a read-only type of its own (`inspect.rs`).** This is a **bug this
  phase introduced and closed in the same phase**: `Pd::open` now campaigns, and a campaign is a
  write, so the inspector as it stood would have appended a `TakeOffice` entry to the database it
  was asked to look at. `PdInspector` names no column family (so a 4a directory does not gain a
  `raft` one just by being looked at), starts no driver, and has a test that opens the same
  directory three times and asserts the term and the apply index never move.
- **Nothing was added to `esker-client`.** §4 declared a new `pd.rs` there; on reading the tree
  there are already three PD clients — `esker_store::pd_remote`, `esker_cli::region::PdConn` and
  `esker_sql::pd::PdConn` — and none of them is `esker-client`'s. A fourth that nothing calls would
  be a fourth place for the redirect rule to drift. What was built instead is the redirect inside
  `esker_store::pd_remote`, which `esker-cli` already uses for `esker server`. **Escalated:**
  `esker_sql::pd::PdConn` still holds one address and belongs to the SQL lane — §10 says what it
  must add.
- **The leader check went into `service::serve`, beside the cluster check**, and covers the reads.
  The writes refuse themselves inside `Pd`; without this a follower would still answer a
  `GetRegion` out of whatever it had applied. `Status` is exempt, for the reason it is exempt from
  the cluster check: it is a question about *this process*, and it is exactly what an operator asks
  a placement driver that is not answering.

## 10. What another lane must add

- **`esker-sql`: `crates/esker-sql/src/pd.rs`.** `PdConn` holds one `SocketAddr` and implements
  `RegionResolver`, so a SQL node whose placement driver has changed leader stops resolving until
  it is restarted. It needs the same three rules `esker_store::pd_remote` now has, and they are
  small: hold the endpoint list rather than one address; on `ProtoError::PdNotLeader` with a
  non-empty `leader_address` that is **in the list**, move to it and retry, bounded by a budget;
  on an empty hint, back off and try the next endpoint. A hint naming an address outside the list
  is a misconfiguration and must not be followed. The `--pd` flag that feeds it should take a
  comma-separated list, as `esker server`'s now does.
- **Nothing is asked of `esker-raft`.** PD drives the same pure `RawNode` under the same `Ready`
  contract, and this lane found no gap in it.
