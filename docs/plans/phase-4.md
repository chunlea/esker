# Phase 4 plan — many regions, and the driver that places them

Status: **in progress** — 4a open, 4b–4e gated. Written before implementation; §9 records progress
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
| 4a | Regions and routing: many `RawNode`s per store, ownership checks, PD v1, the client's cache | now |
| 4b | Split: size check, split key, the `Split` admin entry through Raft, epoch bumps | after 4a |
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
| 2 | The region map: many `RawNode`s per store, keyed by region id | `esker-store/src/regions.rs`, `server.rs` | |
| 3 | Region metadata persisted at `'m' ++ region_id`; a restart recovers every region it hosted | `esker-store/src/meta.rs`, `raft_log.rs` | |
| 4 | Ownership checks on every request, with `EpochNotMatch` carrying **every overlapping local region** | `esker-store/src/region.rs`, `regions.rs` | |
| 5 | `PdClient` + the in-memory fake; bootstrap through it | `esker-store/src/pd.rs`, `server.rs` | |
| 6 | Tick-driven store and region heartbeats | `esker-store/src/heartbeat.rs` | |
| 7 | The client's region cache past one entry; a PD-backed resolver | `esker-client/src/region_cache.rs`, `raw.rs` | |
| 8 | CLI flags: a PD address, a store id, more than one region per store | `esker-cli/src/args.rs`, `cluster.rs` | |
| 9 | The multi-region test spine | `esker-store/tests/multi_region.rs` | |

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

- 4a unit 0 — this plan.

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
