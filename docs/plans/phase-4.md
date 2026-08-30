# Phase 4 plan — many regions, and the driver that places them

Status: **in progress** — 4a accepted, 4b done, 4c–4e gated. Written before implementation; §9 records progress
and §10 what changed. Spec: `prompts/04-multiraft-pd.md`. Constitution: `CLAUDE.md` (invariant 5 is
this phase's whole subject). Design: `docs/DESIGN.md` §2, §6, §7, §9, §14.

Phases 0–3 are accepted and frozen. Phase 3e built a store that replicates **one** region whose
range is the whole key space and whose epoch never moves. Every piece of the routing story is
already there in shape — a request carries `{ region_id, epoch, peer }`, an error carries a redirect
hint, a client caches regions by range — and every one of them is currently a single-element
special case that cannot fail. This phase makes them plural, and therefore makes them fail.

The prompt says this is where most distributed KV projects die, and names the reason: split,
snapshot transfer and rebalancing all interact with Raft membership and with each other. The
defence is that the sub-phases are gates, and that §6's race list is written *before* the code that
has to survive it.

## 1. Scope, and the two lanes

Two lanes build 4a in parallel, against the contract in §3. Each shared crate has exactly one
writer.

| Lane | Owns | Sub-phases |
|---|---|---|
| `cl-p4-store` | `crates/esker-store/**`, `crates/esker-proto/src/region.rs` + the `RaftTransport` parts of proto, `crates/esker-client/**`, `crates/esker-cli/**`, this plan | 4a (then 4b–4d as re-tasked) |
| `cl-p4-pd` | `crates/esker-pd/**`, the `Pd` service section of `crates/esker-proto/**` | 4a (PD v1), 4e |

The dependency runs one way and is *cut* rather than waited on. The store lane needs `GetRegion`,
`AllocId`, `Bootstrap` and the two heartbeats; the PD lane needs `Region`/`Peer`/`Epoch` on the
wire. So:

- The store lane's **first code commit** puts `Region`/`Peer`/`PeerRole`/`Epoch` into
  `crates/esker-proto/src/region.rs` with hand-rolled `encode`/`decode` and golden bytes — they
  travel in heartbeats and in `GetRegion`, which is kvproto's `metapb` and the same reason.
  *(Landed ahead of this plan: the types were already moved into proto during phase 3, because
  `ProtoError::EpochNotMatch` carries a `Region` and the error enum is in proto. §10 records it.)*
- The store lane defines a **`PdClient` trait** in `esker-store` with an in-memory fake, and codes
  the whole store side against it. When the PD lane lands the `Pd` wire section, one implementation
  of that trait is added and nothing above it changes. Neither lane edits the other's files; a
  contract gap is a message to the coordinator.

### Sub-phases (gates)

| # | Sub-phase | Opens |
|---|---|---|
| 4a | Regions and routing: many `RawNode`s per store, ownership checks, PD v1, the client's cache | **accepted** |
| 4b | Split: size check, split key, the `Split` admin entry through Raft, epoch bumps | **done** (§12) |
| 4c | Snapshot transfer and peer movement: `checkpoint(range)` streamed, `AddPeer`/`RemovePeer`, replica repair | after 4b |
| 4d | Balance: leader and region-count operators, `esker-cli region ls/split/transfer-leader` | after 4c |
| 4e | PD high availability: three PDs replicated with `esker-raft`, TSO high-water mark through Raft | after 4d |

## 2. File list (4a, store lane)

```
crates/esker-proto/src/region.rs        Region/Peer/PeerRole/Epoch + encode/decode + goldens   (landed)
crates/esker-store/src/region.rs        RegionMeta: the per-region ownership checks
crates/esker-store/src/regions.rs       NEW — the store's region map: many peers keyed by region id
crates/esker-store/src/meta.rs          NEW — RegionMeta ⇄ the `'m' ++ region_id` record in the raft CF
crates/esker-store/src/pd.rs            NEW — the PdClient trait, its fake, the heartbeat payloads
crates/esker-store/src/heartbeat.rs     NEW — tick-driven store and region heartbeats
crates/esker-store/src/server.rs        Store over a region map; ownership checks on every request
crates/esker-store/src/peer.rs          unchanged in shape; one RaftPeer per region
crates/esker-store/src/raft_log.rs      unchanged; already keyed by region id
crates/esker-client/src/region_cache.rs the cache generalised past one entry; a PD-backed resolver
crates/esker-cli/src/args.rs            cluster/server flags for a PD address and a store id
crates/esker-store/tests/multi_region.rs NEW — routing, epoch staleness, heartbeats, restart
```

Every file stays under ~800 lines. `server.rs` is at 995 and is split by this work, not grown.

## 3. The contract

**Pinned.** Both lanes code against exactly this. Shapes are binding; a field addition that does
not change an existing meaning is allowed and reported, anything else is escalated.

### 3.1 On the wire (`esker-proto`, store lane)

```rust
pub struct Epoch { pub conf_ver: u64, pub version: u64 }      // conf_ver: membership; version: split
pub enum PeerRole { Voter = 1, Learner = 2 }                  // 0 reserved, as everywhere
pub struct Peer { pub store_id: u64, pub peer_id: u64, pub role: PeerRole }
pub struct Region {
    pub id: u64,
    pub start_key: Bytes,          // inclusive
    pub end_key: Bytes,            // exclusive; EMPTY MEANS +∞ — see §5
    pub peers: Vec<Peer>,
    pub epoch: Epoch,
}
```

Encoding: `id:varint ++ start:bytes ++ end:bytes ++ epoch ++ count:varint ++ peers`, where `epoch`
is two varints and a peer is `store_id:varint ++ peer_id:varint ++ role:u8`. Golden-tested.

### 3.2 The PD client, as the store sees it (`esker-store/src/pd.rs`, store lane)

```rust
pub trait PdClient: Send + Sync + fmt::Debug {
    /// Register this store. Returns the region to bootstrap if the cluster is empty and this
    /// store is the first, `None` if the cluster already exists.
    fn bootstrap(&self, store: &StoreInfo) -> Result<Option<Region>, ProtoError>;
    /// A block of cluster-unique ids, for new regions and new peers.
    fn alloc_id(&self, count: u64) -> Result<u64, ProtoError>;
    /// Where a key lives, and who PD believes leads it.
    fn get_region(&self, key: &[u8]) -> Result<Option<(Region, Option<u64>)>, ProtoError>;
    /// Capacity and load, every 10 s.
    fn store_heartbeat(&self, beat: &StoreHeartbeat) -> Result<(), ProtoError>;
    /// One region's leader reporting, every 60 s or on change.
    fn region_heartbeat(&self, beat: &RegionHeartbeat) -> Result<(), ProtoError>;
}
```

`StoreHeartbeat { store_id, capacity, available, region_count, leader_count, applied_bytes }` and
`RegionHeartbeat { region, leader_peer_id, term, approximate_size, applied_index }` are the payloads
the PD lane's wire methods carry; their *field sets* are the contract, their encoding is the PD
lane's. The store lane golden-tests the fields it fills in, not the bytes.

### 3.3 What each lane may not do

`cl-p4-store` never edits `crates/esker-pd/**` or the `Pd` service section of proto.
`cl-p4-pd` never edits `crates/esker-store/**`, `crates/esker-client/**` or `region.rs`.
Neither touches `esker-engine`, `esker-keys`, `esker-base`, `esker-raft` or `esker-sim`: a defect
found there is reported, not fixed in place.

## 4. Units — 4a (store lane), one commit each

| # | Unit | Files | Status |
|---|---|---|---|
| 0 | This plan | `docs/plans/phase-4.md` | done |
| 1 | `Region`/`Peer`/`PeerRole`/`Epoch` in proto with goldens | `esker-proto/src/region.rs` | done (phase 3) |
| 2 | The region map: many `RawNode`s per store, keyed by region id | `esker-store/src/regions.rs`, `transport.rs`, `server.rs` | **done** |
| 3 | Region metadata persisted at `'m' ++ region_id`; a restart recovers every region it hosted | `esker-store/src/meta.rs` | **done** |
| 4 | Ownership checks on every request, with `EpochNotMatch` carrying **every overlapping local region** | `esker-store/src/region.rs`, `regions.rs` | **done** |
| 5 | `PdClient` + the in-memory fake; bootstrap through it | `esker-store/src/pd.rs`, `server.rs` | **done** |
| 5b | That trait over a socket, against the sibling's `PdChannel` | `esker-store/src/pd_remote.rs` | **done** |
| 6 | Tick-driven store and region heartbeats | `esker-store/src/heartbeat.rs` | **done** |
| 7 | The client's region cache past one entry; a fallible resolver | `esker-client/src/region_cache.rs`, `raw.rs` | **done** |
| 8 | CLI: `esker server --pd HOST:PORT` | `esker-cli/src/server.rs`, `args.rs` | **done** (the `args.rs` half is held, §9) |
| 9 | The multi-region test spine | `esker-store/tests/multi_region.rs`, `tests/pd_client.rs`, `esker-client/tests/multi_region.rs` | **done** |

## 5. The five decisions worth writing down before the code

**One apply worker for the store, not one per region.** `docs/DESIGN.md` §6 says "one worker per
store (sharded by region id later)", and 4a keeps it. Phase 3e's driver thread already owns one
region's `RawNode`, its log writes and its apply loop with no locks between them; the multi-region
version is the same thread per region for the Raft half. Sharding the *apply* half is 4d's, and the
constraint that survives either arrangement is in §6: **entries of one region never interleave
across a restart**, which is `apply_index` per region and one `WriteBatch` per entry batch per
region — exactly what 3e proved and what this must not lose.

**An empty `end_key` means +∞ in region metadata and nowhere else.** Region 1 is `["", "")`. In a
scan, `end = ""` means "no upper bound" too — but in a `WriteBatch`, an engine iterator bound or a
`start > end` check, `""` is just the empty byte string and sorts below everything. Every comparison
against an `end_key` is therefore written out rather than left to `Ord`; `Region::contains` and
`Region::contains_range` are the only two places that decide it, and both are tested at the
boundary. A `BTreeMap` keyed by `end_key` gets the last region wrong for ever for this reason, which
is why the client's cache is keyed by `start_key` and walks backwards (§10, item 2).

**`EpochNotMatch` carries every local region overlapping the request's range, not just the one that
was asked for.** A client whose cached region has just split is asking for a range that is now two
regions; answering with only the region whose id it named teaches it half of what it needs and costs
a `GetRegion` round trip for the other half. Under 4b's split storm that is a round trip per stale
request against a single PD, which is how the PD melts. The payload is asserted, not assumed.

**Region metadata is a record in the `raft` CF, not a derived value.** `'m' ++ region_id` holds the
`Region` — range, peers, epoch. A restart reads every `'m'` record and starts a peer for each, so
the set of regions a store hosts is on disk rather than in a config file or in PD's answer. This is
the same rule that phase 3's `91de89a` established for the configuration: *the persisted thing is
the anchor a restart replays from.* PD is asked what to bootstrap only when there is nothing there.

**No wall clock in a decision, and none in a heartbeat's cadence either.** Heartbeat intervals are
counted in ticks from the same injected ticker the Raft driver uses. A test drives the ticker and
asserts the exact beat, rather than sleeping ten seconds and hoping.

## 6. The race list

The prompt names five races and asks for them explicitly. Each one gets the test named here at the
sub-phase where the code that can lose it exists; 4a builds the *checks* that the later ones lean
on, so the 4a column says what 4a must already get right.

| # | Race | What breaks | Where it is tested | What 4a must already have |
|---|---|---|---|---|
| 1 | **Split vs. leader change** | The `Split` entry is proposed on a leader that loses office before it commits. A new leader either commits it (both halves exist everywhere) or does not (the parent is intact). The forbidden outcome is one peer with two regions and another with one. | 4b: `esker-sim` kills the leader inside the split window across seeds; the "key space is a contiguous partition" invariant checked after every event. | Apply is deterministic and `apply_index` is per region: a split that applies on one peer applies on all, at the same index. |
| 2 | **Split vs. snapshot** | A follower is being caught up by a snapshot of the parent's range while the parent splits. The snapshot describes a range the region no longer owns. | 4c: a follower forced onto the snapshot path while its leader splits; the ingested range must be re-checked against the region's *current* metadata before the Raft snapshot metadata is applied. | The region's range is read from the `'m'` record at apply time, never captured once at open. |
| 3 | **Remove-peer vs. crash** | A peer is removed and the store hosting it crashes before it destroys its data. On restart it finds a `'m'` record for a region it is no longer in, and may campaign or serve. | 4c: kill a store mid-`RemovePeer`, restart it, assert it neither serves nor votes for that region. | A restart starts a peer only for a `'m'` record whose peer list contains **this store**; a record that does not is tombstoned rather than started. |
| 4 | **Stale epoch after split** | A client with the pre-split region asks for a key that is now in the sibling. Answering `KeyNotInRegion` alone makes it guess; answering `EpochNotMatch` with only the named region makes it round-trip PD. | 4a (checks) and 4b (against real splits): the epoch-staleness matrix, and the assertion that `current_regions` holds **every** overlapping local region. | The whole of unit 4 — this is 4a's headline. |
| 5 | **PD restart mid-operator** | PD issues an operator, restarts, and reissues or forgets it. A second operator for a region while one is in flight is the thing §7 forbids. | 4c/4d, PD lane: operators are a state machine with a timeout, and the in-flight set is rebuilt from region heartbeats after a restart rather than from PD's memory. | Region heartbeats carry enough — epoch, leader, term, applied index — for PD to rebuild its view from them alone. |

Two more the prompt does not name but 4a can lose on its own:

| # | Race | Test |
|---|---|---|
| 6 | **A Raft message for a region this store used to host** | Already dropped rather than refused (3e). Generalised: a message for a region not in the map is dropped, and one whose epoch is stale is dropped, and neither is an error to the sender. |
| 7 | **Two peers of one region on one store** | Structurally impossible while the map is keyed by region id — inserting a second peer for the same region is a typed bootstrap failure, tested. |

## 7. Test list (4a)

| Area | Tests |
|---|---|
| region metadata | golden bytes for the `'m'` record; a store hosting two regions writes two records; a restart recovers exactly the regions it hosted, ranges and epochs intact |
| routing | two regions hand-seeded; a request for each lands on the right peer; a key in neither is `KeyNotInRegion` with the range that refused it; a region id the store does not host is `RegionNotFound` |
| epoch matrix | `conf_ver` and `version` each ahead and behind, in both directions, against a store hosting two regions: every mismatch is `EpochNotMatch`, and every payload carries **every** overlapping local region |
| the `["", "")` boundary | region 1 contains `b""`, `b"\xff\xff\xff\xff"`, and the namespace byte; a bounded region refuses an unbounded scan; `start > end` is a caller error, not an empty answer |
| heartbeats | the fake PD records the beats a driven ticker produces: a store beat every 10 s, a region beat every 60 s *or* on an epoch/leader change, and the field-level content of each pinned |
| PD client | bootstrap on an empty store creates region 1 `["", "")` through the fake; bootstrap on a non-empty store does not ask; `alloc_id` blocks are not reused |
| client cache | a lookup across three regions including the unbounded last one; a gap misses rather than routing to the region before it; `EpochNotMatch{current_regions}` replaces the parent with its children in one pass; `KeyNotInRegion` drops the lying entry; refresh retries are bounded |
| cluster | 3e's three-store cluster generalised to two hand-seeded regions per store: writes to both, reads from both, kill the leader, nothing acknowledged is lost |

## 8. Non-goals for 4a

- **Split.** No size check, no split key, no `Split` admin entry. 4b.
- **Snapshot bytes.** `engine.checkpoint(range)` and the `Stream` frames stay `TODO(phase-4c)`.
- **Membership operators.** `AddPeer`/`RemovePeer` exist in the Raft core and have no driver here.
- **Balance, `esker-cli region`.** 4d.
- **PD HA.** One PD with durable state, as `docs/DESIGN.md` §7 permits until 4e.
- **Sharded apply.** One apply worker per store, as §6 of the design says.

## 9. Progress

**4a, store lane: units 0–9 landed.** One commit per unit, `just check`'s gates run per crate at
each one (`cargo fmt --check`, `clippy -D warnings`, `cargo doc -D warnings`, tests).

| Commit | Unit |
|---|---|
| `477cac8` | 0 — this plan |
| `b64577d` | 1 — the region types' goldens (the move itself predates the plan, §10) |
| `f2ce2a6` | 2 — one connection per store pair, carrying every region's messages |
| `63220bf` | 2, 4 — the region map, and `EpochNotMatch` carrying every overlapping region |
| `85cbe5e` | 3 — the `'m'` record |
| `86cab14` | 2 — a store hosts a map of regions, read from its own records |
| `92ba6a0` | 5, 6, 9 — the `PdClient` seam, the heartbeats, `tests/multi_region.rs` |
| `75f66e6` | 7 — a fallible resolver, `RegionTable`, `esker-client/tests/multi_region.rs` |
| `6f207ec` | 5b — `RemotePd`, and heartbeat rounds moved off the reactor |
| `a9ecb71` | 8 — `esker server` builds a `RemotePd` when it is given one |
| `c8593be` | 8 — `--pd`, once the sibling lane had committed its own edits to `args.rs` |
| *(this commit)* | DESIGN §6 and §10 brought into line with the code |

**At the 4a gate:** `just check` green on the whole workspace — `cargo fmt --check`,
`clippy --all-targets --all-features -D warnings`, `cargo deny check`, the full test suite, and
`cargo doc -D warnings`. `esker-store` 119 unit + 39 integration tests, `esker-client` 41 + 26,
`esker-proto` 85. No dependency was added.

## 10. Changes vs plan

1. **`region.rs` in proto landed in phase 3, not as this phase's first commit.** The brief and §1
   both call it the store lane's opening move. It was already there: `ProtoError::EpochNotMatch`
   carries a `Region`, the error enum lives in `esker-proto`, and phase 2 could not have compiled
   otherwise. The types, the encoding and the goldens match §3.1 as written, so the contract the
   sibling lane consumes is the one this plan pins — it simply predates the plan. Nothing was moved
   for this phase.

2. **The client's region cache is keyed by `start_key`, not by `end_key`.** The brief asks for the
   TiKV-shaped map keyed by `end_key`. TiKV can do that because it represents the unbounded last
   region's end as a sentinel maximum; with `Bytes` as the key, `b""` sorts *below* every key, so a
   map keyed by `end_key` routes every key above the last region's start to nothing, for ever. The
   cache phase 2 built is keyed by `start_key` and walks backwards to the last region starting at or
   before the key, then checks that the region actually reaches it. That is the same lookup with the
   same cost and no such case, and the reasoning is already in the module's docs with the test that
   pins it. Kept, not rewritten.

3. **The Raft transport became one connection per *store pair*, which §2 did not list as a unit.**
   `docs/DESIGN.md` §6 has always said the connection is per `(store, store)` and carries messages
   for all regions; phase 3e had one region and could not tell the difference. With fifty regions
   on five stores it is four connections per store instead of two hundred, and one batched frame
   per tick instead of fifty — so it is part of "many regions per store" rather than an
   optimisation, and it landed as `f2ce2a6` before the region map.

4. **`RegionResolver::locate` became fallible.** It returned `Option<Route>`, which made an
   unreachable placement driver indistinguishable from "no region covers this key". The first is
   retryable and the second is terminal, so the two collapsed together turn a momentary PD outage
   into a terminal error on every call in the process. It now returns `Result<Option<Route>>` and a
   resolver failure goes through the same classifier a store's refusal does. Found while writing
   unit 7 against a real `GetRegion` rather than a constant.

5. **`RemotePd` is a second sync/async bridge, not a use of `BlockingTransport`.** The plan's §3.2
   pinned `PdClient` as synchronous — correctly, since the store's bootstrap and its heartbeat
   schedule are both ordinary synchronous code — and the placement-driver lane's `PdChannel` is
   asynchronous, as the wire is. `esker-proto`'s `BlockingTransport` exists for exactly this and
   **refuses to run inside a `tokio` runtime**; a heartbeat round runs on `spawn_blocking`, whose
   threads carry a runtime handle, so the refusal applies. `pd_remote.rs` is therefore a dedicated
   thread owning a one-worker runtime. This is the one place in the store where the two worlds
   meet, and the module says so.

6. **The heartbeat round moved onto `spawn_blocking`.** A synchronous `PdClient` called from the
   reactor holds a worker for a network round trip — and a placement driver that had gone away
   holds it for the whole timeout, every ten seconds, on every store. Not a design change so much
   as the consequence of §3.2's synchronous trait, spelled out once the trait had a real
   implementation behind it.

7. **A store told the cluster already exists hosts nothing, rather than bootstrapping a region of
   its own.** §5 says PD answers the bootstrap question; it did not say what a store does with
   `None`. It hosts an empty region map and waits for PD to place a region on it, which is 4c's
   work. The alternative — falling back to `["", "")` when PD says no or cannot be reached — is a
   second claim to every key in the cluster, and the two stores would not find out until a client
   asked one of them. A PD that cannot be reached therefore **fails the open**.

8. **`Store::peer()` and `Store::region()` answer `None` for a store hosting several.** They were
   written for a store with exactly one of each. Rather than pick one, they say so, and
   `peer_of(region_id)` / `regions()` are the questions with an answer in every case.

9. **`capacity`, `available`, `applied_bytes` and `approximate_size` are zero, and say so.**
   Reading a filesystem's size needs `statvfs`, which `std` does not expose and which no crate on
   the allowlist provides without compiling C — so a real number needs an ADR of its own, and 4d's
   balance operators are the first thing that needs one. `approximate_size` is 4b's, from SST
   properties plus the memtable. Each is documented as a placeholder at its field rather than left
   to look like a measurement.

## 11. Closed after 4a

Both items §11 opened are now closed; the coordinator's rulings are recorded with them.

### 11.1 `docs/DESIGN.md` §6 — **done**

§6 now describes what exists: the `'m'` record as the anchor a restart replays from, `apply_index`
per region with no batch spanning two, the heartbeat cadences counted in ticks with "or on change"
defined, and the placeholder fields named as placeholders. §10 gained the `start_key` keying and the
`Ok(None)` / `Err` distinction. What §6 said before, and why it was not simply corrected:

1. **"Apply loop: one worker per store (sharded by region id later)."** What 4a ships is **one
   driver thread per region** — phase 3e's `raft-{region_id}` thread, one per entry in the region
   map. The invariant the brief names is intact: `apply_index` is per region, one `WriteBatch` per
   entry batch per region, and entries of one region cannot interleave. What is not intact is the
   *thread count*: fifty regions on a store is fifty OS threads, and five simulated stores in one
   process is two hundred and fifty.

   The consolidation is not obviously the right fix either, and this is the part worth deciding
   rather than assuming. One worker per store makes region A's `fsync` block region B's consensus
   entirely, which is worse for tail latency than the threads are for memory — which is why TiKV
   has a *pool* sized independently of the region count rather than either extreme. **This is a
   coordinator decision for 4d** ("sharded later" is already where §6 puts it), and until it is
   taken §6 should describe what exists.

2. **"Heartbeats *(phase 4 — `esker-pd` is a stub until then)*."** Built: `heartbeat.rs` counts
   ticks, a store beats every 10 s and a region's leader every 60 s *or on a change*, where a
   change is an epoch bump or a leader change. The parenthetical is gone and the "or on change" has
   its definition.

### 11.2 The `--pd` flag's parsing — **done**

Landed once the placement-driver lane committed its own edits to `args.rs`. Coordinator ruling on
the overlap: `crates/esker-cli/src/pd.rs` belongs to that lane, everything else in `esker-cli` to
this one.

### 11.3 The one deviation the coordinator ratified as the standard

The client's region cache is keyed by `start_key`, not by `end_key` (§10, item 2). Ratified: the
`end_key` keying the brief asked for is the trap, and this shape is the one to build on.

## 12. Sub-phase 4b — split

Spec: `prompts/04-multiraft-pd.md` 4b, `docs/DESIGN.md` §6's Split bullet — whose *"(phase 4 — not
yet implemented)"* qualifier comes off in the commit that implements it. The rows of §6's race list
that involve a split are this sub-phase's test targets.

4a made the key space something a store can hold several pieces of. 4b makes the pieces *move*: one
region becomes two, on every peer, atomically, while writes are in flight. Every ownership check 4a
built and could not make fail can now fail, which is the point.

### 12.1 Units (one commit each)

| # | Unit | Commit |
|---|---|---|
| 0 | This section | `6b6b608` |
| 1 | Approximate region size, published beside `term` and `applied_index` | `0eba75d` |
| 2 | Split-key selection, and its ADR (renumbered to 0012 in `0f846b8`) | `6c07bcb` |
| 3 | The `Split` command: proposed by the leader, applied on every peer | `5ca3ea1` |
| 4 | Routing after a split | fell out of 4a, as expected; asserted in unit 5 |
| 5 | The test battery | `2ec192f`, `d080cfb` |
| 6 | DESIGN §6's Split bullet, and this section closed | *(this commit)* |

### 12.2 The six decisions worth writing down before the code

**A split is a `Command`, not a new `EntryKind`.** `esker-raft` has `Normal` and `ConfChange` and is
another lane's crate; more to the point, invariant 4 says the core is byte-opaque, so an admin entry
it could *recognise* would be a leak of store semantics into consensus. `Command` gains tag 6.
Existing tags and their goldens are untouched — this is an addition to the payload format, not a
change to it.

**The parent stops serving keys ≥ `split_key` at apply, and the check that enforces it is at apply
too.** A write proposed before the split entry but ordered after it must not land in the parent.
Carrying the proposer's epoch in the payload would work and would be a format change; it is not
needed, because the *range* is the observable content of the epoch for this question. `apply::stage`
takes the region's **current** range and refuses a key outside it. Every peer applies the same
entries in the same order, so every peer's range at entry *N* is identical and the refusal is
deterministic — which is the only kind of refusal apply is allowed to make. The proposer's pending
notify fails with `EpochNotMatch`, the client refreshes, and the retry lands on the right half.

That has a consequence worth naming: `apply` must now tell a **deterministic refusal** from a
**corrupt payload**. The first completes one proposal with an error and moves on; the second is
still a hard failure of the driver, because a payload that cannot be decoded cannot be applied and
skipping it would make this peer's state machine differ from every other's.

**Replay is made idempotent by the range, not by a flag.** After a split, the parent is
`[start, split_key)` and `split_key` is no longer *strictly inside* it. So "is `split_key` strictly
inside my current range?" is the whole idempotence check: a replayed split entry answers no and is a
no-op. No marker, no second record, nothing to keep in step.

**Both halves' `'m'` records go in the same batch as `apply_index`.** A crash therefore has both or
neither — the same argument phase 3 made for `apply_index` travelling with the data it applied. The
in-memory region map is updated *after* the batch lands; if the process dies in between, the map is
gone anyway and `Store::open` rebuilds it from the records, which did land. The map update itself
replaces the parent and inserts the child **under one write lock**, so the two never overlap even
transiently.

**The child's log starts at index 0, and its `conf_state` is the split-time membership.** This is
the anchor rule of `91de89a` applied to a region that has no history: `InitialState::conf_state` is
specified as the membership *as of the index the log begins after*, and the child's log begins after
index 0 — so the split-time membership is exactly right, and `RaftLogStorage::open` writing it at
bootstrap is exactly the right write. The child's group then elects from scratch. The parent's
leader usually wins, because its peer is on the store with the parent's data and the others start at
the same index — but nothing may assume it, and no code here does.

**The child's peer ids are paired positionally with the parent's, sorted.** `Split` carries
`new_peer_ids`; every peer sorts the parent's peers by `peer_id` and takes `new_peer_ids[i]` for
`parent.peers[i]`. Deterministic on every peer, which is what matters — a store deciding "my child
peer id is the one PD gave *me*" would need PD in the apply path.

### 12.3 Approximate size — and the engine accessor that is missing

`prompts/04` asks for "approximate size from SST properties + memtable". **The engine exposes
neither per range.** `Db::property` offers `esker.mem-table-size.<cf>` (a whole column family, not a
key range) and `esker.num-files-at-level<n>.<cf>` (a count, not bytes); `Version::overlapping` and
`FileMeta::file_size` are exactly what is wanted and `Db`'s `versions` field is `pub(crate)`.
`Db::checkpoint` reaches them but only to copy files.

Reported rather than worked around, per this lane's brief. **What 4b does instead:** each peer keeps
an in-memory counter of the bytes its own apply has staged, published beside `term` and
`applied_index` for readers that must not wait on the driver. It is a *hint* and nothing
deterministic reads it — a counter that differed between peers would be a second state machine — so
being per-peer, never shrinking on delete, and resetting to zero on restart are all acceptable. Its
only jobs are to trigger the size check and to fill in `RegionHeartbeat::approximate_size`, which
4a had to report as zero.

The accessor `esker-engine` would need is one method:
`Db::approximate_size(cf, begin, end) -> Result<u64>`, summing `file_size` over
`Version::overlapping` plus the memtables' share. **Requested for 4c**, where snapshot transfer
wants the same number for a different reason.

### 12.4 Test list

| Area | Tests |
|---|---|
| size | the counter grows with applied bytes and is published without asking the driver; a restart starts it at zero and says so |
| split key | strictly inside `(start, end)` for a region with 1, 2 and many keys; a region whose keys cannot be split — one key, or all keys equal to `start` — is refused rather than split degenerately; the midpoint of a skewed distribution is still a legal boundary |
| apply | both halves' records and `apply_index` land in one batch; the parent's range narrows and its `version` bumps; the child's `conf_state` is the split-time membership; a replayed split entry is a no-op |
| the transient invariant | after the apply, the map's ranges are a contiguous partition with no overlap — asserted inside the split test, since the phase-4 simulator checks it globally only later |
| the parent's new bound | a write to a key ≥ `split_key` proposed before the split and ordered after it is refused at apply, and the proposer is told `EpochNotMatch` rather than left waiting |
| routing | a stale-epoch request to the parent gets **both** halves; the client's cache replaces the parent with the two and the next call to either is served from cache |
| the prompt's | continuous writes across a region that splits **ten** times at a low threshold: every write durable and readable afterwards, and a scan across every boundary in order |
| race: split × leader change | the leader is killed while a split is in flight; on reopen every peer shows both halves or neither, and the cluster converges |
| race: stale epoch after split | the epoch matrix extended — parent stale, child unknown to the client, child's id not yet in PD |

### 12.5 Non-goals for 4b

- **Merge.** Post-v1, as `docs/DESIGN.md` §6 says.
- **Snapshot transfer**, `AddPeer`/`RemovePeer`, replica repair. 4c.
- **Balance operators and `esker-cli region`.** 4d.
- **The simulator's split coverage** — 50 regions, random splits, 100,000 events per seed. That is
  phase-4 acceptance and belongs to the sim lane; these tests are store-level.

### 12.6 What 4b changed against §12.2

Six decisions were written down before the code; five held. The two entries below are what the
tests found, and both were found by the tests rather than by review — which is the argument for
writing the battery before believing the design.

1. **The apply-time refusal is `EpochNotMatch`, not `KeyNotInRegion`.** §12.2 said the proposer
   "fails with `EpochNotMatch`" and the code answered `KeyNotInRegion`, which is **terminal**. A
   write the split overtook would have failed its caller outright, when the caller had routed
   correctly and the region had simply moved underneath it. The hint carries only the narrowed
   parent, because the driver knows only its own region; the client evicts the stale entry, the key
   then misses, and one `GetRegion` finds the other half. One extra round trip on a race that
   happens once per split rather than once per request. The *request path* still answers
   `KeyNotInRegion`, because there the epoch matched and the range is what the epoch says it is.

2. **The parent's size hint is halved when it splits.** Nothing in §12.2 said what happens to the
   counter, and the answer turned out to matter: the hint counts bytes *applied*, and a split
   applies nothing it can subtract, so the parent stayed over the threshold and tried to split
   again on every tick until it ran out of boundaries. Halving is the honest approximation — the
   parent gave away roughly half its keys — and it is a hint, so approximate is what it is for.

Two smaller things worth recording:

3. **`MIN_SAMPLED_KEYS`.** The sampling scan halves its sample whenever it fills, so a cap of two
   collapses to the region's own first key and stays there — the one place a boundary must not be.
   A floor of 8 turns a bad configuration into a poor sample rather than a broken split.

4. **A boundary is never the first sample.** A region needs two keys before it has a middle;
   otherwise the left half holds nothing. `midpoint` starts its search at index 1, and a region
   with fewer than two keys is not split at all — which is not an error, just a region that is
   large because of one large value.

### 12.7 Still open after 4b

* **The size hint is per-peer and lost on restart** (§12.3). A leader that has just restarted will
  not split until it has re-applied a threshold's worth of writes. The fix is the engine accessor
  §12.3 asks for, not more bookkeeping here.
* **`esker-cli region split`** — an operator-triggered split. 4d, with the rest of the `region`
  subcommand.
* **Splits across a real three-store cluster.** `tests/split.rs` replicates with one peer, which
  exercises the log, the apply and the batching but not the network. `tests/cluster.rs` covers the
  network for phase-3's single region; the multi-store split belongs to the phase-4 simulator's
  battery, which is the acceptance lane's.
