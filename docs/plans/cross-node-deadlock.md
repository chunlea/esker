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

That leaves the question the whole debt rests on: **can a cross-node cycle be built at all?**

## It appears not to be reachable, and the reason is in two places

**Locks are taken in ascending key order.** A transaction's write buffer is a `BTreeMap`, its
primary is `self.buffer.keys().next()` — the *smallest* key it writes — and `commit` prewrites the
primary alone and first, then the secondaries from the same map, which yields them sorted. Two
transactions writing overlapping key sets therefore acquire in the same total order, and a
wait-for chain that only ever ascends cannot return to where it started. That is the classic
ordered-locking argument, and here it falls out of the data structure rather than being imposed.

**And a prewrite never waits while holding.** `prewrite_or_roll_back` answers a definite failure —
`KeyIsLocked` among them — by calling `undo`, which rolls back the primary and every key already
placed, and returns the error. A prewriting transaction therefore holds all of its locks or none
of them; it does not sit on some and wait for the rest. A cycle needs "holds X, waits for Y" on
both sides, and nobody is in that state.

Either reason alone prevents a cycle. The lock-resolution loop that *does* wait runs on the
**read** path — a get or a scan meeting somebody's lock — and a reader holds no locks, so it can
wait and cannot be waited for.

Worked through concretely, with two SQL nodes and the classic opposite-order shape:

```text
A: BEGIN; UPDATE row1   -- node A's row lock only; no Percolator lock yet
B: BEGIN; UPDATE row2   -- node B's row lock only
A: UPDATE row2          -- B's lock is on B's node table, invisible; A buffers it
B: UPDATE row1          -- likewise
A: COMMIT               -- prewrites row1 then row2
B: COMMIT               -- prewrites row1 then row2
```

Whoever reaches `row1` first takes it; the other meets the lock, undoes what it placed, and fails.
One transaction dies with `40001`, the other commits. No cycle, no hang, and the right number of
victims — with the wrong SQLSTATE only in the sense that PostgreSQL would never have called this a
deadlock either.

**This is a code-read argument, not a measurement**, and it is the one thing here that should be
pinned by a test rather than by prose. The test to write is not the one the brief asks for: it is
*"two nodes commit overlapping key sets in opposite application order, and exactly one wins while
neither hangs"* — a regression test for the ordering property, which is what would break if the
primary were ever chosen by insertion order instead of by minimum key. The ordering property
itself belongs in `esker-client`'s own tests, beside the code that guarantees it.

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

## Sizing — and the answer changed while it was being written

**Nothing to build, most likely.** If the two reasons above hold, debt #4 is not "large", it is
*not reachable*, and the work is a test that says so plus a correction to three records. That is
half a day.

If a cycle can be built after all — the ordering argument is only as good as `primary()` staying
the minimum key, and a future batched or reordered prewrite would break it — then what follows is
the sizing for building the detector, and it is not a day:

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
