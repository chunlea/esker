# Cross-node deadlock detection — what is actually there, and what closing it costs

Debt #4 (`docs/plans/debts-v1.md`), sized before starting. **It does not fit in a day**; the sizing
is at the end. What took most of the reading is that the three records describing this debt
disagree with each other and all three disagree with the code, so the first half of this note is
what is true.

## Three records, three different claims

| record | says |
|---|---|
| `debts-v1.md` row 4 | "The wait-for graph is node-local… A cycle *across* nodes needs a graph both can see… PD's job." |
| ADR 0057, *What unit 7 does not do* | "A cycle spanning nodes **waits until `lock_timeout`**." |
| `backend/store.rs`, `StoreTxn::lock` | "two sessions of different nodes do not see each other's locks, and their conflict resolves… **at prewrite, with the loser told `40001`**." |

The first is right about the shape and the owner. The other two are describing different layers as
if they were one, and neither describes what happens.

## What is true

**There are two lock layers, and only one of them can produce a cross-node wait.**

*The row-lock table* (`backend/locks.rs`) is genuinely node-local: one `RowLocks` per `esker-sql`
process, keyed by transaction id, with a wait-for graph and a cycle walk that answers `40P01` to
the waiter. Two sessions of one node deadlock correctly. Two sessions of **different** nodes never
enter it at all — a key locked on node B is simply absent from node A's table, so `take` returns
`Lock::Taken` and node A proceeds. `lock_timeout` bounds *this* wait, which is why ADR 0057's
sentence does not apply: nothing here ever waits for another node.

*The Percolator lock* is where a cross-node wait exists. A transaction meeting another's lock gets
`TxnError::KeyIsLocked`, which `is_resolvable` reports as retryable — "resolve the lock and go
again" — and `esker-client`'s `Transaction::resolve` then classifies the owner. `Classified::Alive`
sleeps and the caller retries. **That sleep is the cross-node wait, and nothing records it
anywhere**, so no graph can see it.

The wait is bounded by `MAX_LOCK_RESOLUTIONS = 8` looks, the last of which spends the whole
remaining lease rather than a backoff step. On exhaustion the client answers
`Error::LockNotCleared`, which `backend/store.rs` maps to `SqlError::SerializationFailure` —
**`40001`**. So the code store.rs names is right; the mechanism is not a prewrite race, it is the
resolution budget running out.

**A fourth gap, and it is load-bearing for how bad this is.** The resolution loop's comments guard
against "a heartbeating owner that keeps extending its lease", and `esker-txn`'s codec documents a
TTL that "a live client extends by heartbeat". **No such client exists**: there is no heartbeat
sender anywhere outside `esker-txn` and `esker-store`, which hold the handler for an RPC nobody
sends. A lock's TTL is therefore fixed at prewrite and always lapses.

That leaves one fork, and it decides the severity:

* **If nothing extends a lease** — what the grep says — then in a two-node cycle the older lock
  lapses first, its waiter rolls that owner back, and **one transaction dies while the other
  proceeds**. Accidentally the right shape, with the wrong SQLSTATE, after a TTL-length stall, and
  with the victim chosen by prewrite order rather than by anything principled.
* **If a heartbeat is ever added** — which the TTL exists for — neither lock ever becomes
  settleable, both budgets run out, and **both transactions die with `40001`** where PostgreSQL
  kills exactly one with `40P01`.

Settling it is a ~40-line probe on the existing cluster harness (below), and it is the first thing
to run. It does not change the sizing: a detector is needed either way, and the second branch
arrives the day somebody adds the heartbeat the TTL was designed for.

## The harness, corrected

**`esker-sim` cannot drive this.** It has `clock`, `fault`, `net`, `lin`, `mech` and `raft`, and no
client or transaction surface at all — its tests are Raft, placement and a linearizable register.
Driving a two-node Percolator cycle there would mean teaching it `esker-client`, which is its own
project.

What can drive it today is `esker-sql/tests/cluster`: `Cluster::another_client()` is documented as
"what a **second SQL node** holds — its own connections and its own region cache… over the same
oracle", and `backend_for` builds a second `StoreBackend` on it. Two nodes over one real
three-store cluster, over real sockets, deterministic enough for `store_locking.rs` already.

## Designs, and which keeps the Raft core untouched

All three leave `esker-raft` alone — this is client and PD state, not a replicated state machine —
so that criterion does not choose between them.

1. **Probe along the wait chain.** A waiter knows the `start_ts` of the lock it met. To learn
   whether that owner is itself waiting, it must ask *somebody*: nothing records "transaction X is
   waiting for Y". Needs a place to publish waits, so it collapses into (2) or (3).
2. **PD holds the graph.** Each waiter reports `(mine, theirs)` on entering the `Alive` arm and
   clears it on leaving; PD walks for a cycle and names a victim. Reuses the shape of
   `RowLocks::deadlocks`. Needs two PD RPCs, PD-side state with expiry (a client that dies without
   clearing must not hold a phantom edge), a new client error, and the `40P01` mapping.
3. **The store records the waiter on the lock.** No new RPC, but a write per wait and a change to
   the lock record — a wire and on-disk format change, which is the thing `StoreTxn::lock`'s
   comment says the ADR deliberately did not half-answer.

(2) is the one to build: no format change, no Raft change, and the graph lives where the debt
record already put it.

## Sizing — not a day

* `esker-proto`: two PD RPCs, hand-rolled framing both ways, round-trip tests. **A wire change, so
  an ADR.**
* `esker-pd`: the graph, expiry, the cycle walk, victim choice, and a decision about whether it is
  replicated state or a volatile hint (it should be a hint).
* `esker-client`: hooks in the `Alive` arm, on the hot contention path, plus a new error.
* `esker-sql`: map it to `SqlError::Deadlock`, which already exists and already answers `40P01`.
* Tests: the two-node cycle over the cluster harness, and a PD unit test for the walk and expiry.

Several days, an ADR, and it spans `esker-proto` and `esker-pd` — pdha's ground, which is where
`debts-v1.md` already assigns it. What belongs to this lane is the SQL end: the mapping, and the
two-node test that says exactly one victim gets `40P01`.

## Worth fixing before any of it

The heartbeat gap is its own small item and is not this debt: an RPC handler that exists on the
store, documented as the thing keeping a live transaction's lock alive, with nobody sending it. It
should either be sent or the comments that assume it should stop saying so.
