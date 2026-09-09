# 0088 — A row lock that two nodes can see (draft)

*Status: **draft**, for the coordinator to rule on. No code. Numbered at `9d642ef0`, where 0087 is
the highest; a later committer renumbers.*

## Context — what happens today, measured

`debts-v1.1` #4 is written as *cross-node deadlock detection*, and `backend/locks.rs` says the same:
*"Node-local, which is every deadlock two sessions of one `esker-sql` process can make. A cycle
across nodes needs a graph both can see."* The measurement says the debt is larger than its name.

`crates/esker-sql/tests/cross_node_deadlock.rs`, two backends over one three-store cluster —
`StoreBackend::new` builds its own `RowLocks`, so two backends are two lock tables over one store,
which is what two `esker-sql` processes are.

**One node, the control.** Two sessions, two rows, opposite orders:

```text
A: BEGIN; SELECT … id = 1 FOR UPDATE
B: BEGIN; SELECT … id = 2 FOR UPDATE
A: UPDATE … id = 2     blocks
B: UPDATE … id = 1     40P01 deadlock detected  — one victim, one survivor
```

**Two nodes, the same sequence:**

```text
A crossed-write:  ok in 2.1 ms
B crossed-write:  ok in 1.5 ms
A commit:         ok
B commit:         ok
rows afterwards:  (1, 9), (2, 9)      -- both writes committed
pg_locks on A:    []
```

**Nothing waited, and nothing was detected, because nothing was locked.** A's `FOR UPDATE` on row 1
is a row in A's table and invisible to B; B takes row 1 without pause. PostgreSQL, given this
sequence, deadlocks and kills one transaction. This node commits both.

So the finding is not "a cross-node cycle goes undetected" — it is that **`SELECT … FOR UPDATE` is
not a cluster-wide lock**. There is no cycle to detect because there is no wait. The two writes here
touch *different* keys, so Percolator's first-committer-wins never fires either: prewrite is the
backstop for two transactions writing **the same** key, and mutual exclusion between a lock and a
*different* key's write is exactly what the row lock was for.

What is at stake is what a client asked for. `SELECT … FOR UPDATE` is how an application says "I am
going to write this row after reading it, hold it for me". Two application servers behind two
`esker-sql` nodes each get that promise and neither gets it.

## Options

### (a) The lock goes to the store

Every row lock becomes a request to the store that owns the key: take, release, and a wait-for edge
carried with it. The store — or PD — holds the graph and answers `40P01`.

* **Protocol**: a new `TxnKvReq` variant, so a new tag and an ADR of its own. Tags are additive and
  an older peer refuses an unknown one (the shape ADR 0067 used), so the format question is
  answerable but it is a wire change.
* **TSO and epoch**: the lock belongs to a key, so it moves with the region; a split must carry
  locks to the child or drop them, and a lock held across a split that is dropped is a promise
  broken silently. That is the hard part and it is the same problem the Percolator lock already
  solves — which is the argument for (a'): *reuse the Percolator lock itself* rather than invent a
  second lock with its own lifetime.
* **Victim choice**: deterministic if the graph is in one place — the youngest `start_ts` loses,
  which is PostgreSQL's rule in spirit and needs no coordination.
* **False cycles**: the graph is authoritative, so the phantom-edge class this crate hit twice
  (run 78's holder edge, and this week's waiter edge) becomes a store-side concern with the same
  shape. The mitigation is the one already learned: an edge names the key it waits on and dies with
  it.
* **Cost**: one RPC per lock and one per release, on a path that is currently a hash lookup. For a
  `SELECT … FOR UPDATE` of a hundred rows that is a hundred round trips unless it batches — and it
  can batch, since the rows are known before the loop.

### (b) The graph goes to PD

Each `esker-sql` node keeps its local table and periodically ships its wait-for edges to PD, which
unions them, looks for a cycle, and names a victim.

* **Protocol**: PD already takes heartbeats; the edges can ride one. No new tag on the store path,
  a new field on a PD message.
* **TSO and epoch**: nothing about the graph is keyed to a region, so a split changes nothing. This
  is (b)'s real advantage.
* **Victim choice**: PD decides, so it is deterministic and consistent — youngest `start_ts`.
* **False cycles**: **the risk lives here.** The edges are a *sample*: by the time PD sees them, a
  waiter may have acquired, timed out, or been cancelled, and a cycle assembled from three nodes'
  stale samples is a `40P01` for a deadlock that never existed. This crate has now produced that
  failure twice locally, where the graph is exact and lives in one process. Across a heartbeat
  interval it would be the common case, not the corner. Any (b) must therefore carry a
  *confirmation* round — PD names a suspected cycle, the nodes re-assert their edges, and only an
  edge still present on the second pass counts.
* **Cost**: nothing on the lock path; one heartbeat's latency on detection, plus the confirmation
  round. Detection is as slow as the heartbeat, which is what PostgreSQL's `deadlock_timeout` is
  anyway.

### (c) No detection: timeouts, and say so

Leave the lock node-local, document that `FOR UPDATE` does not exclude across nodes, and let
`lock_timeout` be the answer where a wait does happen.

* **Protocol**: none.
* **Cost**: none, and it is what runs today.
* **What it costs instead**: the measurement above. Today the sequence does not even *wait* — both
  transactions commit and one application's `FOR UPDATE` bought it nothing. (c) is only honest if
  the divergence is written where a user meets it, and "no cross-node deadlock detection" is not
  that sentence. The sentence is *"`SELECT … FOR UPDATE` excludes other sessions of the same node
  and not sessions of another node."*

## Recommendation

**(a'), reusing the Percolator lock, with (c)'s documentation landed first and immediately.**

The reasoning is that (b) detects a deadlock that this system does not currently have. There is no
cycle to find because there is no wait: fixing detection without fixing the lock leaves the measured
behaviour — both commit — exactly as it is. Detection is the second problem and it is only reachable
after the first.

Between (a) and (a'): a row lock and a Percolator lock already answer the same question about the
same key, and the Percolator lock is already replicated, already moves with a split, and already has
a lease and a resolver. A second lock with its own lifetime would be a second set of rules for one
question — the thing `backend/locks.rs`'s own opening paragraph says it avoided when it refused to
keep two wait-for graphs. `SELECT … FOR UPDATE` becoming a prewrite of a lock-only mutation is the
shape to price: ADR 0067 already added `Check` and `CheckRange` as mutations for a neighbouring
reason, so the mechanism exists and the question is cost, not novelty.

**What I would want before committing to it**: the cost of a lock-only prewrite for a hundred-row
`FOR UPDATE`, measured, against the hash lookup it replaces. If it is a round trip per *statement*
rather than per row, (a') is affordable; if it is per row, the batching has to come first.

**And (c) regardless**: the divergence is real today and users meet it today. It should be written
down in the same commit that decides anything else.

## Consequences of not deciding

Two application servers behind two nodes get a lock that does not lock. It is silent, and it is the
kind of silence that shows up as data that cannot be explained rather than as an error anyone can
route.
