# Phase 4 plan — many regions, and the driver that places them

Status: **4a–4d accepted; 4e deferred past v1** (§15). Written before implementation; §9 records progress
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
| 4c | Snapshot transfer and peer movement: the region streamed, `AddPeer`/`RemovePeer` | **done** (§13) |
| 4d | Balance: leader and region-count operators, `esker-cli region ls/split/transfer-leader` | now (§14) |
| 4e | PD high availability: three PDs replicated with `esker-raft`, TSO high-water mark through Raft | **deferred** (§15) |

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
- **PD HA.** One PD with durable state, as `docs/DESIGN.md` §7 permits — and, from the 4d gate on,
  past v1 as well (§15).
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

## 13. Sub-phase 4c — snapshot transfer and peer movement

Spec: `prompts/04-multiraft-pd.md` 4c, `docs/DESIGN.md` §6's Snapshots bullet — whose phase-4
qualifier comes off in the commit that implements it. The races it targets are §6's rows 2 and 3:
split vs. snapshot, and remove-peer vs. crash.

4b divided a region. 4c **moves** one: a peer that has fallen too far behind, or one that has just
been created on a store with none of the data, is caught up by shipping the region's files rather
than its log. That is the first time bytes cross the wire other than as a Raft entry, and the first
time a region exists on a store that never had it.

### 13.1 The thing that has to be true, and the shape it forces

**A snapshot is never half-visible.** A region whose data is partly the snapshot's and partly its
own is a stale-read machine: it answers from a state no peer ever had. Every design decision below
is downstream of that one sentence.

The receive is therefore staged so that a crash lands in one of three places, each of which a
restart can name:

| Step | Durable after it | A crash here leaves |
|---|---|---|
| 1. Announce | `'p' ++ region_id`: *a snapshot for index N is being applied* | a marked region that is not started and not served |
| 2. Stage | the files, in a temp directory under the store's | the same, plus files nothing references |
| 3. Ingest | one manifest edit — atomic, all files or none | the same, plus data in the range |
| 4. Adopt | `'m'` record + the peer's raft state, and the `'p'` record gone | the region, complete |

On open, a region with a `'p'` record is **not started**. Whether its ingest landed is one seek: the
range was empty when the snapshot was announced (step 1 refuses otherwise), so *any* key in it means
step 3 completed and step 4 is all that is left. Both branches resume without ever serving a
half-state.

`'p'` is a **new prefix**, not a field added to the `'m'` record. The `'m'` format has a golden test
one sub-phase old, and an addition beside it costs nothing while a change to it would need an ADR
and a version bump — the same reasoning as `Command`'s tag 6 in 4b.

### 13.2 The two engine and core gaps this ran into

Reported rather than worked around, per this lane's standing instruction.

**`Db::ingest` refuses any overlap, tombstones included** (`crates/esker-engine/src/db/ingest.rs`,
and it says so: sequence-number rewriting is a v2 feature). A receiver that already held keys in
the range cannot clear them and then ingest — the deletes leave tombstones, which are keys in the
range, which ingest refuses. **So 4c only applies a snapshot into a range this store holds nothing
in**, which is exactly the case 4c creates: a peer added to a store that never had the region. A
peer that does hold data is refused with a typed error and stays behind; PD sees it in the
heartbeats. `TODO(post-v1)`: the engine's own v2 sequence-number rewriting removes the restriction.

**`esker-raft` exposes no per-peer `Progress`.** `RawNode::status()` gives the leader its own
commit, applied and last index, and `ConfState`; there is no `match_index` per follower. The
learner-promotion criterion `prompts/04` asks for — "promote to voter when caught up" — is a
statement about a follower's match index, and the store cannot see one. 13.4 says what is used
instead and why it is sound; the accessor is requested for 4d, where the balance operators want the
same number to decide whether a transfer is safe.

### 13.3 Units (one commit each)

| # | Unit | Files | Status |
|---|---|---|---|
| 0 | This section | `docs/plans/phase-4.md` | `bea2207` |
| 1 | `Db::approximate_size`, and 4b's byte counter replaced by it | `esker-engine/src/db/**` (granted), `esker-store/src/split.rs`, `server.rs` | `4c28990` |
| 2 | Raft log compaction: `'s'`'s truncation fields written, a real `LogStorage::snapshot` | `esker-store/src/raft_log.rs`, `peer.rs` | `8005736` |
| 3–4 | Send and receive: the stream, and the four steps of §13.1 | `esker-store/src/snapshot.rs` (NEW), `meta.rs`, `server.rs`, `esker-proto` | `8e67246` |
| 5–6 | `AddPeer`/`RemovePeer` and the operators that ask for them | `esker-store/src/apply.rs`, `peer.rs`, `regions.rs`, `heartbeat.rs`, `pd.rs` | `5cddb25` |
| 7 | The test battery | `esker-store/tests/snapshot.rs` (NEW) | `33e95e9` |
| 8 | DESIGN §6 and §14, and this section closed | `docs/DESIGN.md`, this plan | *(this commit)* |

Units 1 and 2 are the foundations the rest stands on and neither existed before: there was no way
to ask the engine how large a key range is, and **nothing ever compacted a Raft log**, so
`LogStorage::snapshot` always answered "nothing to send" and the whole `InstallSnapshot` path was
unreachable code. A follower can now fall behind the start of the log, which is the only way a
snapshot ever becomes necessary.

### 13.4 Decisions worth writing down before the code

**The receiver pulls; the sender does not push.** The leader's `InstallSnapshot` message travels
as an ordinary `RaftBatch` message — ADR 0009 already has the wire carrying the real
`esker_raft::Message` — and it is the *announcement*. The follower answers it by making its own
request back to the leader's store, `RaftTransport::Snapshot { region_id, index }` (method
`0x0402`), and reads the reply as a run of `Stream` frames.

Pull rather than push for two reasons, one structural and one operational. `esker-proto`'s
streaming is a *reply* shape: a `Service` answers with `Reply::Stream` and the client's
demultiplexer reassembles it by request id. A push would need inbound `Stream` frames on the
server side, which the framing does not have and which would be a protocol change to add. And a
pulling receiver controls its own concurrency and its own retries — it can refuse to start a second
transfer for a region it is already receiving, which is where §13.1's `'p'` record gets its meaning
— while a pushing sender would have to track what each follower was in the middle of.

**The Raft message carries the metadata; the files travel beside it.** `InstallSnapshot` goes out
with `Snapshot::data` **empty** and its `meta` real. `esker-raft` reads only the meta — that is why
it can handle snapshots in a crate that does no I/O (`types.rs`) — so nothing is lost, and a 96 MiB
Raft message is not something the frame limit or the message queue should ever see. The receiver
does not `step` the message until the files are in: the core restoring first would leave a peer
claiming an index whose data had not arrived.

**A chunk that fails restarts the whole snapshot.** No resume, no per-chunk retransmit. A snapshot
is idempotent and cheap to redo relative to the bookkeeping that resuming needs, and a partial
transfer is already discarded by §13.1's staging. Stated here because it is a deliberate v1
simplification rather than an oversight.

**The checkpoint pins one apply index.** `Db::checkpoint` flushes and then pins a version, so the
files describe the state at some sequence number. The snapshot's `meta.index` must be an apply
index that state actually includes — so the driver reads its own `applied_index` **before** the
checkpoint runs and the checkpoint is taken on the driver's thread, where nothing else can apply in
between. A meta ahead of the data would let a follower skip entries it never received.

**Learner first, and the promotion criterion the store can actually evaluate.** `AddPeer` adds a
**learner**: it receives the log and the snapshot without voting, so adding one never makes a
quorum harder to reach while it catches up — which is the whole reason for the two-step. Promotion
to voter needs "caught up", and §13.2 says the match index is not visible. What *is* visible, and
is a sound sufficient condition, is: **the snapshot transfer to that learner completed, and the
leader has since committed an entry at a term it still leads in.** The first says the learner holds
everything through `meta.index`; the second says the leader is still the leader and the log has
moved on with the learner in it. It is more conservative than a match index — a learner that is
catching up by log replay alone is promoted later than it needs to be — and being late costs a
delay, while being early costs a quorum that cannot be reached.

**`RemovePeer` tears down the raft state, not the data.** The region's `'l'` entries, `'s'` state
and `'m'` record go; the keys in `default` stay. Deleting them would mean point deletes over the
whole range (the engine has no range tombstones in v1, ADR 0006), leaving tombstones that block a
later `ingest` into the same range — the very thing §13.2 describes. So the data is left, is served
by nothing, and is reclaimed when the store is next asked to hold a region overlapping it, which is
when it becomes a problem worth solving. Documented, not silent.

**The anchor rule, for a peer built from a snapshot.** Its `conf_state` comes from the snapshot's
`meta.conf` — the membership as of `meta.index`, which is the index its log now begins after. Same
rule as `91de89a`, same reason, third place it applies.

### 13.5 Test list

| Area | Tests |
|---|---|
| compaction | the log truncates behind the apply index; `first_index` moves; `term` still answers at the boundary; the configuration written with the truncation is the one as of it |
| snapshot meta | a compacted log offers a snapshot whose `index`/`term`/`conf` match the truncation record; an uncompacted one still offers nothing |
| send | the chunk stream is the checkpoint's files, in order, each checksummed; a region's range and nothing else |
| receive | the four steps land in order; a chunk with a bad checksum aborts without touching the database |
| the crash matrix | killed after each of §13.1's steps: the restart shows the peer pre-snapshot or fully post-snapshot, and a resumed receive reaches the same state as an uninterrupted one |
| conf change | `AddPeer` adds a learner; a learner's acknowledgement never counts toward a quorum; promotion happens only under 13.4's criterion; `RemovePeer` leaves no raft state and no route |
| operators | a heartbeat response's operator is proposed once; the same operator seen twice is proposed once; one against a stale epoch is dropped with a trace |
| cluster | three stores, a fourth added: learner → snapshot → voter → an old peer removed, with data verified at every step |

### 13.6 Non-goals for 4c

- **Replica repair driven by PD** — the scheduler that notices a store is down and issues the
  operators. That is the placement-driver lane's; this lane consumes the operators it sends.
- **`TransferLeader`.** The operator variant is reserved in the contract and ignored here. 4d.
- **Merge, balance, `esker-cli region`.** Post-v1 and 4d.
- **Resuming a partial snapshot.** See 13.4.

### 13.7 What 4c changed against §13.4

Five decisions were written down before the code. Three held exactly. The other two were wrong in
ways only the battery could show, and both are worth reading.

1. **Key-value pairs, not `checkpoint` + `ingest`.** §13.2 knew `ingest` refuses overlap; what it
   missed is that a checkpoint links *whole files*, and a file straddles a region boundary. The
   receiver would be handed its neighbour's keys — data it has no claim to, and which `ingest`
   itself refuses if that neighbour lives on the same store. Range-precision is not something file
   granularity can offer, so the stream is pairs. DESIGN §6 is updated with the reasoning and the
   `TODO(post-v1)` that makes the link-only transfer possible again.

2. **The promotion criterion in §13.4 was unsafe.** It said: promote once "the snapshot transfer
   completed and the leader has since committed an entry in a term it still leads in". The second
   half is a fact about the *leader* and says nothing about the learner, and the first was never
   wired to the promotion at all — so what shipped promoted immediately. A learner with no data
   entered the quorum, the group stopped committing, and the conf change waited for an apply that
   could not happen, wedging the heartbeat round that carried it.

   **Promotion is PD's call now**, on the contract as it stands: `AddPeer` for an unknown peer adds
   a learner, the same operator for a peer already a learner promotes it. That is where the
   information is — PD sees every store's region heartbeats, including the learner's own
   `applied_index`, and the leader sees neither. The `Progress` accessor §13.2 asked for would let
   a leader decide for itself; until then this is not a workaround but the better placement, and
   4d should weigh keeping it there.

3. **A store with no region cannot ask Raft for a snapshot.** The intended flow was the ordinary
   one: the follower rejects an `AppendEntries`, the leader backs off past its own log start and
   offers a snapshot. A store that does not host the region has no peer to reject *with*, so it
   drops the message, the leader sees no rejection and offers nothing. **The traffic itself is the
   signal**: any Raft message for an unhosted region makes the store ask its sender for that
   region. A store legitimately removed from a region asks too and is refused, by the sender's
   membership check — which is the right place for it.

4. **`RaftOptions::peers` was doing two jobs.** It is the address book — where a peer id can be
   found — and it was also the bootstrap membership, so a store could not know how to reach a peer
   it was about to be told it had. `bootstrap_voters` separates them.

5. **An operator's proposal is bounded by a timeout.** Nothing said what happens when a membership
   change cannot reach a quorum; the answer was "the heartbeat schedule stops", which closes the
   only channel PD has to correct its own mistake.

### 13.8 Still open after 4c

* **Catching up an existing peer by snapshot.** Only a range this store holds nothing in can
  receive one (§13.2). A peer that fell behind *and* has data is refused and stays behind. The
  engine's v2 sequence-number rewriting is what removes the restriction.
* **`esker-raft` has no per-peer `Progress`.** Requested for 4d, where the balance operators want
  the same number to decide whether a transfer is safe.
* **Replica repair end to end** — a store killed for good, PD noticing and issuing the operators,
  three replicas restored. The store side is complete and tested against a fake driver that issues
  operators; the scheduler that decides to issue them is the placement-driver lane's.
* **`RemovePeer` leaves the region's data.** Reclaiming it needs a range delete the engine does not
  have.

## 14. Sub-phase 4d — balance (store half)

Spec: `prompts/04-multiraft-pd.md` 4d. The placement-driver lane builds the schedulers; this lane
builds what they act on, plus the two things 4a–4c deferred to here.

### 14.1 Units

| # | Unit | Files |
|---|---|---|
| 0 | This section | `docs/plans/phase-4.md` |
| 1 | The driver worker pool: regions pinned to a fixed set of threads | `esker-store/src/driver.rs` (NEW), `peer.rs`, `server.rs` |
| 2 | `TransferLeader` operator consumption | `esker-store/src/server.rs`, `peer.rs` |
| 3 | `esker-cli region ls / split / transfer-leader` | `esker-cli/src/region.rs` (NEW), `args.rs`, `main.rs` |
| 4 | Per-peer `Progress`, read-only, in `esker-raft` (granted) | `esker-raft/src/raw_node.rs` |
| 5 | The distribution test | `esker-store/tests/balance.rs` (NEW) |
| 6 | DESIGN §5, §6, §9, §12 and §14, and this section closed | `docs/DESIGN.md`, this plan |

### 14.2 The threading decision, finally taken

`docs/DESIGN.md` §6 said "one apply worker per store (sharded by region id later)"; 4a shipped
**one driver thread per region** and 4c's plan §11.1 recorded why neither extreme is right. The
ruling for 4d is the middle: **a fixed pool of driver threads, each region pinned to one of them by
its id**.

Pinning and not scheduling, and that is the whole of the correctness argument. A region's messages
all reach one worker through one channel and are handled in the order they arrive, so per-region
ordering is exactly what it was when the region had a thread to itself — the property `apply_index`
and the `Ready` contract both rest on. What changes is only that a worker holding several regions
interleaves *between* them, which nothing depends on: two regions share no state, no batch and no
apply index.

**By modulo, not by hash.** Region ids come from PD's allocator in order, so `id % workers` spreads
them exactly evenly; a hash would only add variance. It is also stable across restarts without
being written down, which matters because a region that moved workers between opens would be a
region whose ordering guarantee spanned two threads.

What it buys: an `fsync` for one region no longer holds up every other region's consensus, which
one-worker-per-store would have made worse than the per-region threads it replaced; and fifty
regions cost four threads rather than fifty.

### 14.3 Test list

| Area | Tests |
|---|---|
| pinning | a region is always given to the same worker; the mapping is stable across a restart |
| concurrency | N regions on a **2**-worker pool make progress at the same time — one region's slow apply does not stop another's |
| ordering | every region's entries apply in index order under a pool that is interleaving them |
| shutdown | retiring one region leaves its worker serving the rest; stopping the pool fails everything outstanding |
| transfer | a `TransferLeader` operator moves leadership; one against a stale epoch is dropped; one naming a peer the region does not have is dropped |
| cli | `region ls` against a live cluster; `region split` at a chosen key; `region transfer-leader` |
| distribution | one store, then three: a few hundred MiB at a low split threshold, and regions **and** leaders spread within a bounded time with no region orphaned |

### 14.4 What 4d changed against §14.2

The threading decision held exactly as written: pinned by modulo, ordering unchanged, and the pool
is what `tests/balance.rs` runs a two-worker cluster on. Three things around it did not.

1. **A driver error must retire one region, not the worker.** The per-region threads had nowhere
   to put this question: a thread that died took its only region with it. A worker holding a dozen
   regions cannot exit on one region's failure, so a `drive()` error retires that region and the
   worker goes back to the queue.

2. **`retire` has to be acknowledged.** A caller that retires a region and immediately flushes the
   next thing — a split installing the child, a snapshot replacing the peer — must know the worker
   has finished with the old core. `Job::Retire` carries a `SyncSender` and the caller waits on it
   (5 s), which is the one place the pool is synchronous.

3. **The transfer refusals are the store's, not PD's.** §14.2 said nothing about who checks. The
   store drops a `TransferLeader` for a peer the region does not have, for a learner, and for a
   peer more than `TRANSFER_LAG_ALLOWANCE` entries behind — silently, because a refusal a scheduler
   cannot act on is noise it would only retry. The third check is the whole reason unit 4 asked for
   `RawNode::progress()`.

### 14.5 The two bugs the distribution test found, and the core gap behind them

Both are the same confusion in different clothes: **a peer id names a replica, not a store.** It has
now produced three bugs across 4c and 4d, which is why DESIGN §6 and §9 both say so explicitly.

1. **A snapshot was asked for by peer id.** A store receiving traffic for a region it does not host
   asks the sender for it, and resolved the sender's address out of the store's address book by the
   *peer* id. PD allocates a peer id per replica, so after a split the offering peer is in no
   address book anywhere. `esker_proto::RaftMessage` now carries `from_store` — one varint, and a
   **golden-affecting wire change**, made on the same reasoning as 4b's `Command` tag 6: the format
   has no deployment to be compatible with, and the alternative was a lookup that cannot be made to
   work. `raft-timeout-now` and `raft-append` were updated in the same commit.

2. **A conf change was routable only after it applied.** §4.1 puts a configuration in force at the
   **append**, so the leader addresses the peer it adds in the same `Ready` that carries the entry —
   a round trip before apply moves the `'m'` record the transport routed by. The first message was
   therefore *always* dropped. Usually invisible: the next heartbeat gets through and the receiver
   asks for the region. Not invisible when the region's log had been compacted, because then the
   first message is an `InstallSnapshot`, and that is the gap below. Routes are now learned from
   every conf change at persist time, before the send, out of the store id its context already
   carried; `peer.rs`'s `Auditor` audits it as its second ordering rule.

**The core gap, reported and not worked around** (the third for this lane, after §13.2's two).
`ProgressState::Snapshot` is paused unconditionally, and nothing leaves it but a message *from* the
follower. A single lost `InstallSnapshot` therefore strands a replica for ever: the leader believes
a snapshot is in flight, sends nothing further — `send_heartbeat` for a follower below the
compaction boundary delegates to `send_append`, which is paused too — and the follower never learns
it should ask. etcd/raft closes this with `ReportSnapshot(Failure)` from the transport plus an abort
when `pending_snapshot <= matched`; `esker-raft` has neither. Fixing #2 removes the only *systematic*
way to lose that message, so nothing here is left broken — but a network that drops one still
strands a replica, and a `report_snapshot` (or a resend after an election timeout) is what would
make it recover. Out of 4d's grant, which is read-only in the core.

### 14.6 Still open after 4d

* **A lost `InstallSnapshot` strands a replica** — the core gap above.
* Everything §13.8 lists that 4d did not touch: catching up an existing peer by snapshot,
  `RemovePeer` leaving the region's data, and replica repair end to end against the real scheduler
  rather than a fake driver.
* **`region ls` walks the key space one `GetRegion` at a time.** Correct and O(regions) round trips;
  a `ScanRegions` on the Pd service would make it one. Not this lane's to add.


---

## 15. 4e — PD high availability, deferred past v1

**Ruling at the 4d gate: phase 4 ships the single durable PD.** This is the escape hatch
`prompts/04-multiraft-pd.md` writes into 4e itself — *"if time is short, ship 4a–4d with single PD
and record this as the next milestone; never ship TSO without the persisted high-water mark"* —
taken deliberately, with the milestone recorded here rather than left as an unwritten intention.

The condition attached to the hatch is met. The oracle has persisted its high-water mark since 4a:
the mark is fsynced 3 s ahead of what is handed out, every timestamp given away has
`physical < mark`, and a restart resumes at `max(clock, mark)` — so a PD that dies and comes back,
even on a machine whose clock has gone backwards, cannot repeat a timestamp
([ADR 0010](../adr/0010-pd-durable-state.md), `crates/esker-pd/tests/crash_kill.rs`). That is the
part 4e could not have been allowed to substitute for, and it is done.

### What deferring costs

While PD is down, a cluster keeps serving. Stores hold their own regions, their own Raft logs and
their own membership on disk (§6), and a client's region cache is a hint the store checks against
its epoch — so a stale cache costs a redirect, never a wrong answer (`CLAUDE.md` invariant 5).
What stops is everything that needs PD to *decide*:

| Down | Effect |
|---|---|
| `Tso` | no new transaction can start or commit — phase 5's hard stop |
| `AllocId` | no split, no new peer: the cluster cannot grow or repair |
| `Bootstrap` | a new store cannot join |
| `GetRegion` | a client with a cold cache cannot route; a warm one carries on |
| heartbeats | repair and balance stop being scheduled; existing operators are forgotten |

Reads and writes through a warm client against a healthy cluster are unaffected. The exposure is
therefore *availability of change*, not availability of data — which is what makes it a milestone
rather than a blocker.

### What 4e would add

Nothing that changes the rules above it — only where their state lives:

1. **Three PDs, replicated with `esker-raft`.** The cluster record, the store and region records,
   the range index and the allocator become entries in a Raft log rather than writes to one
   engine, with the same records and the same key space (ADR 0010) behind it.
2. **The oracle's mark through the log.** The rule is unchanged — persist ahead, fsync before the
   timestamp leaves, resume at `max(clock, mark)` — and only the meaning of "persisted" moves,
   from one fsync to a committed entry. `Oracle` and `Allocator` take a persist *callback* for
   exactly this reason: the *what* can change without the *when* moving.
3. **Leader election for PD, and discovery for clients.** A `PdChannel` that follows a redirect to
   the current leader, which is the same shape as `NotLeader` on the KV path.
4. **Nothing for the scheduler.** The in-flight set is already memory that a restart re-derives
   from heartbeats, and the repair and balance rules are already pure functions of the routing
   table and store liveness ([ADR 0013](../adr/0013-repair-operators-are-requests-not-commands.md),
   [ADR 0018](../adr/0018-balance-moves-the-spread-by-two.md)). A PD that loses leadership is, to
   the scheduler, a PD that restarted — a case that is already tested.

That last point is why deferring is cheap: the parts of PD that would have been hardest to make
highly available are the ones deliberately built with no durable state of their own.

### The one thing to check first, when 4e opens

Every timestamp handed out must still have `physical < mark` when the mark is a committed Raft
entry rather than a completed fsync — which means the mark's commit must be observed, not merely
proposed, before a timestamp above the old mark leaves. `crates/esker-pd/tests/crash_kill.rs` is
the test to point at the new implementation, and its own limit still applies: a `SIGKILL` proves
the ordering and the restart rule, not the durability of the write underneath.

## 15. The `ProgressState::Snapshot` strand (post-4d, gate blocker)

§14.5 reported this as a core gap and left it. The ruling made it a **phase-4 gate blocker** — the
acceptance simulator drops messages by design, so a strand that needs one lost message is a strand
the acceptance run will hit — and granted the core fix. This is that unit.

### 15.1 The failure, exactly

A leader that has offered a snapshot sends that peer nothing else: `ProgressState::Snapshot` is
paused unconditionally, and only an `AppendEntriesResponse` from the follower ends it. Heartbeats
are no escape, because a follower below the compaction boundary has no index the leader can anchor
a heartbeat at, so `send_heartbeat` delegates to the same paused `send_append`.

So a follower that never received the offer never answers, and never will. The replica is stranded
for the rest of the leader's term. Three ways in, all of them ordinary:

* the offer was lost — the case `tests/balance.rs` hit, where the transport had no route yet;
* the transfer failed after it began — the receiver hung up, the walk errored;
* the store serving it died mid-stream, so nobody was left to say anything.

### 15.2 Three rules, in the order they fire

**The driver reports** (`RawNode::report_snapshot`, `SnapshotStatus::{Finished, Failed}`). The core
does no I/O and never saw a byte, so the driver is the only party that can know: this is a new rule
of the `Ready` contract (`docs/raft-spec.md` D6), not an optimisation. `Finished` probes from the
snapshot's index — the follower holds at least that much, even though it has not said so.
`Failed` forgets the index first and probes from `matched + 1`, because probing from an index the
leader just failed to deliver would only earn a rejection. Neither is a claim the follower is caught
up; that stays the acknowledgement's job.

**An acknowledgement at or past the pending index ends it**, whatever became of the transfer, and
does so *inside* `maybe_update` — before the response handler reads the state. That ordering is the
whole of its value: the follower has just named the exact index it holds, so it goes straight to
`Replicate` rather than spending a round trip proving it again through `Probe`. An acknowledgement
*short* of the pending index still takes the slow path, because it genuinely has not answered the
question.

**`SNAPSHOT_TIMEOUT_TICKS` covers what no report can.** Not the second-guess of a report — the case
where nobody owes one: the offer was lost before any transfer began, so no stream was ever served.
100 ticks, one store-heartbeat interval, so a lost offer is repaired before a scheduler could see
the stall. Re-offering a transfer genuinely in progress is not the hazard it looks like: an offer
carries no data, and the receiving store already de-duplicates by region.

### 15.3 Why the store can report at all

The store's snapshot path is receiver-pull, so the **leader serves the stream** and therefore knows
how it ended. `Store::send_snapshot`'s walk was extracted into `stream_snapshot`, which returns
whether every byte got there — extracted for exactly one reason: every way out of the walk now has
to produce an answer, and an early `return` that skipped one is the bug this whole section exists
to fix.

### 15.4 Tests, and what proves they test something

Six unit tests in `esker-raft/src/snapshot.rs`, one sim test, and the mutation check the ruling
asked for. Each rule was removed in turn and the suite re-run:

| Mutation | Went red |
|---|---|
| `report_snapshot`'s match dropped | `a_failed_report_probes_from_what_the_follower_has`, `a_finished_report_probes_from_the_snapshot_it_delivered` |
| the moot rule dropped from `maybe_update` | `an_acknowledgement_past_the_pending_index_ends_the_snapshot` |
| `expire_pending_snapshots` dropped from the tick | `a_snapshot_nobody_reports_on_expires_and_the_leader_probes_again`, and `esker-sim`'s `a_snapshot_the_network_loses_is_offered_again` |

`a_leader_waiting_on_a_snapshot_sends_that_follower_nothing` states the strand itself: nothing the
leader does on its own ends the wait. It is what the other tests are the ways out of.

The sim test earns its place. The existing compaction sweeps sum `snapshots_installed` across
seeds, so they pass with a few followers stranded; this one heals a cut-off follower onto a link
that drops one message in three and requires **every** seed to repair it. In the simulator no
driver streams bytes, so no report is possible — the only mechanism that can satisfy it is the tick
timeout, which is why dropping that one line turns it red. It costs a second.

## 16. The PD→store operator seam, checked

The placement-driver lane's close-out flagged `pd_remote.rs` as dropping the operator from a
region-heartbeat response, which would mean no decided operator ever reaches a store over a real
wire. **It does not**, at HEAD: `tests/pd_wire.rs` stands a placement driver up on a socket, has it
answer a region heartbeat with `AddPeer`, and requires the region's own peer list to gain the peer
— which happens only after the conf-change entry has been proposed, committed and applied.

The test is worth keeping whatever the report was about, because the seam it covers is the one an
in-process fake skips entirely. Every other operator test hands the store a `FakePd` directly, so
`PdResp::RegionHeartbeat`'s encoding, `PdChannel`, the blocking bridge in `pd_remote.rs` and the
heartbeat schedule that collects the answer are all unexercised — and an operator dropped anywhere
along that path is a placement driver whose decisions silently never happen, with nothing failing.
Mutation-checked: making `RemotePd::region_heartbeat` return `Ok(None)`, which is the reported bug
exactly, turns it red.

The other half was already covered from the far end: `esker-pd/tests/loopback.rs` has a real
placement driver decide an `AddPeer` and reads it back over a socket through a `PdChannel`. The two
tests meet at `PdChannel`, so the path from a scheduler's decision to a Raft proposal is now
covered end to end by tests on both sides of it.

## 17. The phase-4 acceptance promotion stall

Acceptance reported two findings: learner→voter promotion never completes (repair reached no
region at 3 voters in 4 minutes, with zero data loss; balance did not converge in 240 s), and
scale-out inverted — 255 → 194 → 176 ops/s at 1 → 3 → 5 stores.

### 17.1 Load or logic: the logs answer without a re-run

The acceptance box was CPU-contended, so the first question is whether this is a timeout-abort-retry
loop (load-sensitive) or a criterion that never fires (logic). The run's own artefacts settle it,
and the answer is **logic**:

* the balance run's final listing has **every** region at `epoch=(3,5)` with exactly one voter, on
  store 1, plus two learners. `conf_ver` 3 is two conf changes — the two `AddLearner`s — and then
  nothing. A load-sensitive retry loop churns `conf_ver`; this is the minimum possible value and
  every region holds it;
* the repair run went `(3,4)` → `(5,4)`: remove the dead store's learner, add the new store's
  learner. Again the minimum, again no promotion, again exactly one voter per region throughout;
* across both runs the store logs carry **52 operators applied and not one** "an operator did not
  commit", so the store's 5 s `OPERATOR_TIMEOUT` never fired; 45 snapshots arrived, 2 did not and
  were retried; zero `WARN` or `ERROR` on store 1 in either run.

Nothing was aborting and retrying. Nothing was slow. The second step was never *asked for*.

### 17.2 Why it was never asked for

Two contracts, each sound alone, that compose into a deadlock:

* **the store's, from 4c:** an `AddPeer` for a peer that is already a learner *is* the promotion —
  so the store needs the operator **re-sent** to take the second step;
* **PD's:** `InFlight::advance` returns `None` for `Progress::Started`, because an operator whose
  learner PD can already see "has demonstrably started, and asking again would only earn a refusal"
  — so PD **stops sending** the moment the learner appears.

Nobody asks, nobody promotes. After `operator_timeout` PD re-derives, `repair_for` counts the
learner among `region.peers` and sees no under-replication, and the region stays where it is.

Behind that is a plain factual error, written into `server.rs` in 4c and quoted in §13.7: that PD
"sees every store's region heartbeats, including the learner's own `applied_index`". **A region
heartbeat comes from a region's leader and only from its leader** (§7). A learner leads nothing, so
it is invisible to PD entirely — PD could see that a learner had appeared and never that it had
caught up. The criterion 4c placed with PD had no input and never could have had one.

This also explains finding 2 without any separate cause. A learner cannot take office, so leader
balance can never move anything: every region's leader stayed on store 1 in all three
configurations. Added stores took no leadership and served no reads, while store 1 fed each of them
the log and a snapshot per region. More stores, strictly more work for the one that does everything.

### 17.3 The fix: the leader promotes, on the number only it can see

`Store::promote_caught_up_learners`, on the region-heartbeat schedule. `AddPeer` now means "put a
replica here" and the store fulfils it in two steps. The criterion is **progress, not a clock** —
which matters on a loaded box, where a clock is the thing that lies: the learner is within
`PROMOTION_LAG_ALLOWANCE` of the leader's own `matched`, is not waiting on a snapshot, and has
acknowledged something at all. Each of the three is a way the 4c promotion deadlock came back.

Measured on `tests/promotion.rs`: **0 of 63 learners promoted before, 62 of 63 after**, and 14 of 14
in the smaller configuration.

### 17.4 A second defect the first was masking

With promotion working, a residual appears: some learners caught up by snapshot are never replicated
to at all — `matched` 0, `recent_active` **false**, `pending_snapshot` 0, `next` = the leader's last
index + 1. The leader has heard nothing from them, ever.

One mechanism found and fixed, in the core, under the same gate-blocker grant. A follower below the
compaction boundary cannot be heartbeated — there is no index to anchor an empty append at — so
`send_heartbeat` delegates to `send_append`, which is **paused** after its one outstanding probe.
`probe_sent` clears only on an answer, so if that single probe is the message that goes missing, the
leader sends that peer nothing for the rest of its term. For such a follower the probe *is* the
heartbeat, and a heartbeat is not subject to flow control anywhere else in Raft; it is not here
either. `Snapshot` is deliberately left paused, since `SNAPSHOT_TIMEOUT_TICKS` paces those re-offers.
Mutation-checked. It reduced stuck observations by about a hundredfold.

**Still open.** `tests/promotion.rs` fails about three runs in four: a learner still occasionally
stalls in exactly that shape. One lead, recorded and *not* acted on: the regions that stall are
**exactly** the regions that log "dropped a Raft message from a stale epoch" — 14 regions, identical
sets. A peer caught up by snapshot adopts its record from the snapshot header, which is the leader's
record as of the transfer and can be a `conf_ver` ahead of what that leader's transport is still
stamping; every message from its own leader is then dropped as stale. Checking `version` alone was
tried and did **not** fix the stall, so the correlation is not yet a cause and the check is left as
it was. A second lead: one `a Raft snapshot arrived; streaming is phase 4` — a core-delivered
snapshot the store still ignores — appeared on a stalled region.

### 17.5 Chasing clue (b): who still called the stub

The ruling was to chase `a Raft snapshot arrived; streaming is phase 4` first, and it was a real
bug. `Ready.snapshot` is set only by `RaftLog::restore`, which only `handle_install_snapshot` calls
— so the only way to reach that stub is to **step an `InstallSnapshot` into the core**, and
`receive_raft`'s hosted branch stepped every message it was given.

The consequence is worse than the dropped log line. The core restored from the metadata alone: the
peer's log jumped to the snapshot's index and its membership to the snapshot's `conf_state`, while
this store wrote no data and did not move the region record. `drive` then logged and dropped the
`Ready`, and `advance` cleared it — leaving a peer holding a log position it had no data for and a
membership its own region record disagreed with. That is a peer that stamps an epoch nobody expects,
which is where the `conf_ver` half of the stale-epoch correlation came from.

An `InstallSnapshot` is an announcement in this design and is never stepped now. A hosted peer that
has fallen behind the boundary declines it, which is §13.2's documented v1 limitation happening
visibly instead of something that looked like a repair. The stub is kept as a loud invariant breach.

### 17.6 What the residual actually is, and what it is not

Counting messages in both directions for a stalled region: **333 dispatched leader→learner, 238
stepped by the learner, 238 back**, and the learner's own store reports it applying. The learner is
alive, replicating and answering. The leader's `matched` for it stays 0 regardless.

The loop is: the learner adopts a snapshot at some index; the leader writes on and compacts past it;
every probe is then rejected; the re-offered snapshot is declined because catching up a peer that
already holds data is the v1 limitation; repeat for ever. It is §13.2 reached in ordinary operation,
which 4c assumed could not happen — "4c only applies a snapshot into a range this store holds
nothing in, which is exactly the case 4c creates" — because the peer 4c creates falls behind before
it can be fed.

Two things ruled out rather than assumed, both of which had looked like the answer:

* **`recent_active = false` is not evidence the leader never heard from the peer.** Check-quorum
  clears it every election timeout, about ten times a second at this test's 5 ms tick. Reading it
  as "never active" is what pointed at the transport for two rounds.
* **The stale-epoch drops are a symptom.** They correlate exactly with the stalled regions — 14
  regions, identical sets — but checking `version` alone did not fix the stall, so that change was
  reverted rather than shipped, and so was removing the check entirely.

`hold_for_lagging_peers` keeps a leader from compacting past a peer it could still catch up cheaply,
bounded by `SLOW_PEER_LOG_ALLOWANCE` so a departed replica cannot hold the log open. It is right on
its own merits and unit-tested, and it did **not** clear the repro — recorded as hardening, not as
the fix. The first attempt at it silently did nothing, because it skipped peers with `matched == 0`,
which is the single case it existed for.

The repro is `crates/esker-store/tests/promotion.rs`, `#[ignore]`d with all of this in its doc
comment, per the ruling that a repro belongs in the tree.

## 18. Ruling (A): a snapshot can replace a range that already holds data

`snapshot::clear_range` is the piece 4c did not have, and `DeleteRange` landing in the engine
(`cc2c891`) is what made it possible. Three steps, each needed because of what the one before it
leaves behind:

1. **`delete_range`** over the region's range. Not enough on its own — a deletion is a *stored
   entry*, so the range now holds tombstones and anything the snapshot does not contain would be
   hidden rather than gone;
2. **`flush`**, which moves the tombstone out of the memtable into L0, the only level a range
   tombstone can live in (ADR 0017);
3. **`compact_range`** over the same range, which **discharges** it: the compaction applies the
   tombstone and drops the files it covers rather than propagating it.

The two things the ruling asked to be verified are both real and both handled.

**No engine snapshot may be held.** `Picker::can_discharge` requires every tombstone's `seqno <=
compaction_floor()`, and the floor is the oldest pinned snapshot. A reader pinned by this very path
would hold the floor below its own tombstone and block the discharge it is waiting for, so
`clear_range` takes `&Db` and pins nothing.

**The emptiness check is load-bearing.** If the discharge could not empty the range, refilling would
leave the region serving a mix of its own state and whatever survived — and the mix would look like
correct data. It refuses and says so, naming the floor as the thing to look at, rather than being
weakened.

Around it: `fetch_snapshot` **replaces** a region this store already hosts instead of refusing it
(the old peer is retired and waited for, so nothing is applying into the range while it is emptied),
and a snapshot announcement for a hosted region now starts a fetch rather than being declined.

### 18.1 The deadlock underneath, and a rule of Figure 3.1 corrected

Clearing alone changed nothing, because the leader never got as far as offering a second snapshot.
The core's own tracing showed why: **526 "backing off after a rejected append" and not one snapshot
offered.**

A follower caught up by a snapshot has a log beginning at the snapshot's index. An append from
below it cannot be verified, so it was rejected — and `Progress::maybe_decr_to` walks `next` *down*
on a rejection and by rule never back up, so a leader that had probed past the boundary probed there
for ever, against a follower answering every single one. The follower's hint was correct and the
leader was structurally unable to act on it.

The fix is etcd's rule, which this crate was missing: **an append below the follower's commit index
is answered, not rejected**, with the follower's own commit index. Everything at or below
`committed` is settled by quorum — no leader can contradict it — so there is nothing to verify and
the only useful answer is where this node actually is. `handle_install_snapshot` already answered
that way for exactly this reason; this is the same rule for the message that carries entries.

`an_append_below_the_installed_snapshot_is_refused` asserted the old behaviour and is now
`..._is_answered_from_where_the_node_is`. **The rule it encoded was the bug**, and raft-spec's N6
row is updated with it. Race 5's real content — the log is not rewound by an append from below it —
still holds and is still asserted.

### 18.2 Where this leaves the repro

`tests/promotion.rs` now **passes some runs** (one in 9.9 s) and still stalls in others. The
learners get far further than before — `applied` 58 where it used to be 18 — so what remains is
narrower than what was fixed. It stays `#[ignore]`d until it is reliably green.

Not yet chased: (B), which compaction pass bypasses `hold_for_lagging_peers`. The likeliest answer
is that `RawNode::progress` is empty on a peer that is not leader at that instant, so a pass during
a leadership flap compacts by the tail rule alone — and one such pass is permanent.
