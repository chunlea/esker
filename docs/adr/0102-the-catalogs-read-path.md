# ADR 0102 — The catalog's read path (draft, no decision)

Status: **proposed — a draft for a milestone conversation, not a decision** · Number 0102 reserved
by the coordinator; 0101 is the highest on `main`. **No code changes with this file.**

## Context — one region is on the path of every statement, and that is the key space, not a bug

The reserved layout (`docs/DESIGN.md` §3) orders the namespaces by their first byte:

```text
'm' ++ …   cluster metadata — the catalog lives here ('m' ++ "sql" ++ 't'/'v'/'n' ++ tenant ++ …)
'r' ++ …   RawKV
't' ++ …   the SQL logical key
'x' ++ …   TxnKV — where every SQL row is actually written
```

`'m'` sorts below `'x'`, and a split boundary is chosen inside the data, so **the catalog stays in
the left-most region for the life of the cluster**. It is region 1 on every cluster this project
has run.

`Catalog::view_at` is on the path of every statement and reads **two counters** from that region —
the tenant's `catalog_version` and the cluster's, summed — plus a layout check, **once per
transaction**. So every statement on every SQL node makes at least two point reads of one region,
and its answer must come from the leader of that region.

**The measured consequence** is `docs/plans/debts-v1.1.md` #34: when that one region has no leader
for a moment, *every* statement on the node is refusing, and seven of fourteen sightings of a
leaderless region named it — not because region 1 is special to Raft, but because it is the only
region every statement touches. #40 is the mechanism behind those moments and
[ADR 0101](0101-a-batch-of-ticks-never-carries-a-whole-election.md) removes one of its causes; none
of that changes the shape this file is about.

## What any answer has to keep

Three properties, all from `crates/esker-sql/src/catalog/mod.rs`'s own module docs, and each one is
a correctness rule rather than a preference:

1. **One version for a whole transaction.** A statement cannot see two shapes of one table, and a
   transaction cannot see a table appear halfway through it.
2. **A transaction that has run DDL reads its own writes and publishes none of them.** Its view
   sits at a version no committed transaction has reached, so that view is never cached.
3. **A cache may not serve a definition from a transaction's own future.** A transaction whose
   snapshot is older than the cached version reads through to the store.

## The three shapes

### (a) A cached version with an invalidation the store pushes

The node keeps the version it last read and uses it without asking, until the store tells it the
version moved.

* **Consistency**: an invalidation that is *late* serves a stale definition, which rule 3 exists to
  forbid — so the push has to be ordered against the DDL's commit, not merely sent after it. The
  honest version of this is a lease: the node may use the cached version for as long as the store
  has promised not to acknowledge a DDL without telling it, and the fallback when the lease lapses
  is the read we do today.
* **The once-per-transaction contract** survives untouched: it becomes once-per-transaction *from
  the cache*, and the version is still a single value the whole transaction is answered at.
* **What it needs**: a subscription in the wire protocol — a new tag, since nothing today lets a
  store push to a SQL node — and a lease with a clock, which is the part that needs an ADR of its
  own rather than a paragraph in this one.
* **What it saves**: the two point reads per transaction, in the common case where no DDL has run.

### (b) A lease read, or a follower read, of the version

Keep the read; stop requiring the leader to answer it. A learner already answers fragments this way
(`Store::catch_up` runs a `ReadIndex` round through `RaftPeer::read_index_as_learner`), and the
same machinery would answer a catalog counter from a replica.

* **Consistency**: a `ReadIndex` round is linearizable — the replica establishes the leader's commit
  index and waits to apply through it — so rules 1 to 3 hold unchanged. It does not remove the
  dependency on the region *having* a leader, though: the round asks one.
* **The contract**: unchanged. This is the same read from a different peer.
* **What it needs**: no new tag — the round already exists — but the SQL node must be able to
  address a *replica* of the catalog region, which today it cannot: routing hands out the leader.
  That is a client and PD change rather than a protocol one.
* **What it saves**: nothing on a healthy cluster, and a great deal when the leader is busy: the
  read stops queueing behind whatever else that one region is doing.

### (c) A split exemption: the catalog gets a region nothing else can make busy

Cut a region at the `'m'`/`'r'` boundary at bootstrap and never split or merge it, so the catalog's
region holds the catalog and nothing else.

* **Consistency**: none of the three rules moves. This changes *what else shares* the region, not
  how the version is read.
* **The contract**: unchanged.
* **What it needs**: a PD change — a region PD creates and never chooses as a split candidate — and
  a rule in `esker_store::split` that refuses a boundary inside `'m'`. No wire change at all, which
  makes it the cheapest of the three by a wide margin.
* **What it saves**: it removes the *coupling*, not the round trip. Every statement still reads the
  version over the network; what it stops is a table's load, a table's splits and a table's
  elections deciding whether the catalog can be read. On the evidence in #34 that coupling is the
  expensive half.

## What to measure before deciding

* **The round trip's real cost**: two point reads per transaction to one region — measured against
  a statement that does no other work, which is where it is the whole of the latency.
* **How often it is the leaderless one**: #34's instrument already counts sightings per region; the
  interesting number is what share of a node's refusals come from the catalog's region rather than
  from the region the statement is actually about.
* **For (a) only**: how often a DDL invalidation would arrive, which is what decides whether a
  lease is nearly free or a constant interruption.

## Not decided here

This file exists so the choice is made against the same facts by whoever makes it, and so that the
consequence recorded in #34 has somewhere to point. **It decides nothing**, and each of the three
shapes above needs its own ADR when it is chosen — (a) in particular is two decisions wearing one
name, since the lease is where its correctness lives.
