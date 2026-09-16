# 0116 — A single-node oracle needs a physical half, or its locks never expire

Status: **Proposed**, 2026-09-16 — debt #84, the number issued by the coordinator. Builds on
[ADR 0088](0088-a-row-lock-across-nodes.md) (the lease a row lock carries) and
[ADR 0021](0021-time-machine.md) decision 1 (a timestamp's high bits *are* milliseconds).

## Context

A Percolator lock carries a lease: `ttl_ms` from its `start_ts`, after which any transaction that
wants the key may settle its owner and take it. That lease is the only way out for a lock whose
owner is gone — a store killed inside the prewrite window, a client that died — because every other
route asks the owner.

The lease is judged in the timestamp's **physical** half, in two mirrored copies of one rule:

```rust
pub fn physical_ms(ts: u64) -> u64 { ts >> TSO_LOGICAL_BITS }        // 18 bits of logical counter
pub fn is_expired(start_ts: u64, ttl_ms: u64, now_ts: u64) -> bool {
    physical_ms(now_ts) > physical_ms(start_ts).saturating_add(ttl_ms)
}
```

(`esker_client::physical_ms` and `esker_client::is_expired`, mirrored byte for byte in `esker_txn`,
with `esker_client::LOCK_TTL_MS = 3_000`. Cited by name: line numbers in this repository drift — see
the note on #84's own row below.)

`esker_client::CountingOracle` hands out **consecutive integers from a small start**
(`starting_at(1)`, an atomic `fetch_add`). Every timestamp it will mint for the first 262,144 calls
therefore reads as physical millisecond **zero**, so `is_expired` is false for ever,
`Transaction::classify` answers `Alive` about an orphan for ever, and every later statement touching
that key spends the client's resolution budget and answers
`40001 … a lock from the transaction at N could not be cleared for a read`. **Restarting the node
does not help**: the counter starts again, still at physical zero.

The single-node `esker-sql` binary ships exactly that oracle — `crates/esker-sql/src/bin/esker-sql.rs`,
the `if pd.is_empty()` arm, which is the production path for a node started without `--pd`:

```rust
Arc::new(esker_client::StaticRegion::replicated(1, &store_ids)) as Arc<_>,
Arc::new(esker_client::CountingOracle::starting_at(1)) as Arc<_>,
```

**The comment at that site — and at two others — argues about *ordering* and is silent about
*leases***, which is how one omission reached three places. It cost `statement_across_a_leader_kill`
a night of red gates (120 attempts, 632 s, the same message every time) before the arithmetic was
read. `MemoryBackend`'s frozen clock was the same class and is already repaired — `backend::Versions`, the
in-memory backend's stand-in for the oracle, whose mark is *"the greater of the last timestamp handed
out and the wall clock"* — which is evidence the class gets built rather than another open instance.
(Debt #84's row cites that repair as `backend/mod.rs:466-472`; those lines are `read_set_covers`
today, because units K, L, M and O all edited the file. The row's line numbers want correcting when
it closes; the name does not drift.)

Heartbeats do not rescue it: `crates/esker-client/src/renew.rs` extends a lease in the same physical
half, so a mode with no physical half has no renewal either.

## Options

### (a) Give the single-node oracle a physical half, from the clock seam that already exists

`esker-client` has `Clock` / `SystemClock` / `FakeClock` (`crates/esker-client/src/clock.rs`) — the
seam its retry loops are tested through. A single-node oracle mints `ts = now_ms << 18 | logical`
from it, with the logical counter restarting each millisecond, which is the layout PD already mints.

* **No new crate edge**: `esker-sql` links `esker-client` today.
* **No format change and no wire change**: a timestamp is a `u64` everywhere it is written or sent;
  only which numbers get minted changes.
* Tests can drive it, because `FakeClock` is the same seam the rest of the client is tested through.
* Monotonicity across restarts has to be decided explicitly: taking the clock alone repeats
  timestamps after a backwards jump. Either keep PD's rule — a persisted high-water mark, and start
  at `max(clock, mark)` — or accept the clock and say so.

### (b) Reuse PD's oracle in-process

`esker_pd::tso::Oracle` is a **pure state machine**: `load(record, now_ms, save_interval_ms)` and
`allocate(count, now_ms, persist)`, where `persist` is a closure the caller supplies. No socket, no
disk of its own — so a single-node binary can hold one without running a placement driver, and the
`max(clock, mark)` restart rule and the "mark is fsynced before the first timestamp above it" rule
come with it, already crash-tested (`esker-pd/tests/crash_kill.rs`).

* **Cost: a new dependency edge `esker-sql → esker-pd`** — the SQL crate would link the placement
  driver, which it does not today (its dependencies are base, client, columnar, keys, proto).
  `esker-cli` already links both and could host such an oracle without a new edge, but the
  single-node SQL node is `esker-sql`'s binary, not the CLI.
* Also needs somewhere to persist the mark for a node whose store lives in a data directory.

### (c) Declare that single-node mode has no lock expiry

Say it where the oracle is chosen, and in `docs/DESIGN.md` §8: without `--pd`, an orphaned lock lives
until the data directory is cleared.

* Costs nothing to build and is honest.
* Leaves the failure exactly as it is: one killed client makes one row permanently unreadable, and
  the message blames serialization.

## Recommendation

**(a)**, with the monotonicity question answered rather than left open: mint from the `Clock` seam,
and keep a high-water mark beside the node's data when there is a directory to keep it in, starting
at `max(clock, mark)` as PD does. It is the only option that changes no format, no wire and no crate
graph, and it makes `is_expired` mean what it says in every mode the binary offers.

(b) is the better engineering if the edge is acceptable — it reuses rules that are already
crash-tested rather than restating them — and this ADR is happy to be overruled that way. (c) is
what we are doing now by accident; adopting it deliberately would at least stop the message lying.

## What this does not decide

A physical half does **not** make a local counter a cluster oracle. Two `esker-sql` processes against
one store still hand the same `start_ts` to different transactions, which is `CLAUDE.md` invariant 6
and what the existing comments are about (`tests/two_nodes_one_clock.rs`). The single-node arm stays
single-node.

## Tests this would need

* An orphaned lock that **expires**: a lock minted at `start_ts`, the fake clock advanced past
  `LOCK_TTL_MS`, and `classify` answering settled rather than `Alive` — red today on the single-node
  path, and the shape `statement_across_a_leader_kill` met on a real cluster.
* Two timestamps inside one millisecond stay ordered, and a batch never straddles a millisecond.
* A restart hands out nothing it handed out before (the `max(clock, mark)` rule, if a mark is kept).
