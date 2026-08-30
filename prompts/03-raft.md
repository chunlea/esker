# Phase 3 — Raft as a pure state machine, then one replicated region

Implement `esker-raft` exactly as `docs/DESIGN.md` §5 describes: no threads, no clocks, no I/O; time
enters through `tick()`, messages through `step()`, effects leave through `Ready`. This purity is the
whole strategy — it lets the simulator and the model checker find the bugs that took other projects years.
Read the Raft dissertation's Figure 3.1 (the condensed spec) and keep a copy of it as
`docs/raft-spec.md` annotated with the function that implements each rule.

## Sub-phases

**3a — Core.** `RawNode`, `Message` enum (RequestVote, RequestVoteResponse, AppendEntries,
AppendEntriesResponse, Heartbeat/Response folded into AppendEntries with no entries, InstallSnapshot,
TimeoutNow, ReadIndex/ReadIndexResponse), `LogStorage` trait with an in-memory implementation, leader
election with randomized timeouts from the injected RNG, log replication with per-follower
`next_index`/`match_index`, commit index advancement (only entries from the current term commit by
counting, per §5.4.2 of the paper), `Ready`/`advance` bookkeeping. Tests: unit tests for every rule in
Figure 3.1; a proptest that random message interleavings never produce two leaders in one term.

**3b — Simulator.** In `esker-sim`, a discrete-event simulation of N `RawNode`s over the in-memory
`Network`: seeded RNG, `FaultPlan` (partitions, message drop/delay/dup/reorder, node crash/restart with
persisted `LogStorage`, slow disks). Run thousands of seeds per CI job. Check after every step:
election safety, log matching, leader completeness, state-machine safety (all applied logs are prefixes
of one another). Liveness: after faults heal, a leader is elected and a proposal commits within a bounded
number of ticks. On failure print the seed and a compact trace.

**3c — Model checking.** A `stateright` model of a 3-node cluster with a small message space and bounded
log length, checking the same four safety properties by exhaustive exploration. Keep it small enough to
run in CI in under two minutes; document its bounds.

**3d — Snapshots and membership.** Log compaction, `InstallSnapshot` (metadata only in the core; bytes
move through the store), single-server membership changes (add/remove one voter or learner; the
configuration is applied when the entry is *appended*, per the dissertation §4.1), learners, leadership
transfer, pre-vote and check-quorum. Extend the simulator: nodes join and leave while faults happen;
snapshots are installed mid-partition.

**3e — One replicated region in `esker-store`.** Three stores, one region, Raft log in the engine's
`raft` CF, the driver contract from DESIGN.md §5 (persist before send), apply loop writing data CFs plus
`apply_index` atomically, leader-only serving with `NotLeader{hint}` redirects, ReadIndex reads.
`esker-cli cluster start --nodes 3` starts three processes on localhost. The client's region cache now
learns leaders from redirects.

## Tests for 3e

- The phase-2 model and crash tests re-run against the 3-node cluster.
- Simulator-driven cluster test: the real `esker-store` code paths (apply loop, transport adapter) over
  the simulated network, killing the leader every few seconds; a Porcupine-style linearizability check of
  the single-key history must pass.
- Real-process chaos: start 3 processes, SIGKILL the leader 50 times under load; no acknowledged write
  lost, cluster converges each time within 5 s.

## Acceptance

Simulator runs 10,000 seeds with faults with zero safety violations; the stateright model passes; the
3-node cluster passes the linearizability check; bench numbers (replicated `fillrandom --sync`)
recorded in `docs/bench/phase-3.md`; `docs/raft-spec.md` maps every rule to code. Anything you decided
that the dissertation leaves open goes in an ADR.
