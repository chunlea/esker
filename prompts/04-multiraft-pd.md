# Phase 4 — Multi-Raft, regions, and the placement driver

Now the store hosts many regions and the cluster scales horizontally. This is where most distributed
KV projects die: split, snapshot transfer, and rebalancing all interact with Raft membership and
each other. Work in the sub-phases below and do not combine them. Write `docs/plans/phase-4.md` with an
explicit list of the race conditions you intend to test (split vs. leader change, split vs. snapshot,
remove-peer vs. crash, stale epoch after split, PD restart mid-operator).

## Sub-phases

**4a — Regions and routing.** `Region`/`Peer`/`Epoch` types; a store hosts many `RawNode`s keyed by
region id with one apply worker (sharded later); key-range ownership checks on every request
(`KeyNotInRegion`, `EpochNotMatch{current_regions}`); `esker-pd` v1 as a single durable process:
bootstrap, `AllocId`, store/region heartbeats, `GetRegion`, the routing table persisted in its own
engine; the client's region cache keyed by range with invalidation on epoch errors.

**4b — Split.** Size check per region (approximate size from SST properties + memtable), split key
chosen at the midpoint of the region's data, `Split` admin entry proposed through Raft with ids from PD;
applied on every peer under the region lock; the right half starts a new Raft group with the same peers;
epoch `version` bumps on both. Clients see `EpochNotMatch` and refresh. Test: continuous writes across
a region that splits ten times; every write is durable and readable; the simulator kills leaders during
splits.

**4c — Snapshot transfer and peer movement.** `engine.checkpoint(range)` on the leader, streamed to the
target store as `Stream` frames in checksummed chunks, `ingest`ed there, Raft snapshot metadata applied; `AddPeer`
(learner first, promote to voter when caught up) and `RemovePeer` operators; PD schedules
replica repair when a store is down longer than `max_store_down_time` (30 s default). Test: kill a
store permanently; PD repairs every region to 3 replicas; data verified.

**4d — Balance.** Leader balance and region-count balance operators in PD with in-flight limits and
timeouts; `esker-cli region ls/split/transfer-leader`. Test: start with 1 store, add 4; regions
and leaders spread out within a bounded time; add 20 GB of data with the bench tool and confirm the
split count and distribution.

**4e — PD high availability.** Run PD as a 3-node group replicated with `esker-raft`; TSO high-water
mark persisted through Raft; leader election for PD; clients discover the PD leader. (If time is short,
ship 4a–4d with single PD and record this as the next milestone; never ship TSO without the persisted
high-water mark.)

## Tests

- Simulator: 5 simulated stores, 50 regions, random splits, peer moves, partitions and crashes for
  100,000 events per seed; per-region linearizability; the "no two peers of one region on one store" and
  "key space is a contiguous partition" invariants checked after every event.
- Real processes: `esker-cli cluster start --stores 5 --pd 3`; the chaos script from phase 3 extended
  with store additions/removals and network partitions via the simulator's `Network` fault plan
  (real processes use `tc`/`iptables` in a docker-compose variant — provide the compose file, it may run
  outside CI).

## Acceptance

Linear scale-out demonstrated: `fillrandom` throughput with 1, 3, and 5 stores recorded in
`docs/bench/phase-4.md` with the region counts; all invariant checks pass across 1,000 seeds; a store can
be removed and added back with no data loss; DESIGN.md §6–7 match the code; ADRs written for split-key
selection and the learner-first promotion policy.
