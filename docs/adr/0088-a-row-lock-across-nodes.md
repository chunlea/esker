# 0088 — A row lock that two nodes can see

Status: **accepted** (ruled 2026-09-09) · Date: 2026-09-09 · Phase 9, the locking family ·
Numbered at `9d642ef0`, where 0087 is the highest; a later committer renumbers ·
Supersedes [ADR 0057](0057-read-committed-waits-for-the-writer-in-front-of-it.md) §5's *node-local*
declaration and closes the half [ADR 0067](0067-the-check-mutation-and-the-latest-commit-question.md)
§2 named as not built · Ruled **(a')**: the row lock becomes a Percolator lock

*The measurement and the options below are the draft this was ruled from, unchanged. The
[decision](#decision--a-ruled-2026-09-09) follows them.*

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

## The record this ADR joins

Half of #4 was already worked, from the other end. `docs/plans/cross-node-deadlock.md` (`28ec2474`)
argues by code read that a cross-node cycle **on the write path** cannot be built: a transaction's
write buffer is a `BTreeMap`, its `primary()` is `self.buffer.keys().next()` — the smallest key it
writes — and the secondaries follow sorted, so two transactions acquire in one total order; and
`prewrite_or_roll_back` answers a definite failure with `undo`, so a prewriting transaction holds
all its locks or none and is never in the "holds X, waits for Y" state a cycle needs.
`crates/esker-client/tests/prewrite_ordering.rs` (`758e7223`) pins both halves, red-first, with the
lock-resolution budget at zero so that "exactly one wins" is decided by the acquisition order rather
than by a stopwatch.

**That argument stands, and this measurement does not touch it** — they are different mechanisms.
There, Percolator's lock, taken at commit, on keys the transaction *writes*. Here, the SQL row lock,
taken at `SELECT … FOR UPDATE`, on a row the transaction has only *read*. Read together they say the
same thing twice: on the write path there is no cycle because acquisition is ordered, and on the
row-lock path there is no cycle because there is no lock.

Which is why the register's row is aimed one problem too far along. **`debts-v1.1.md` #4 — "large —
needs a PD-held graph", owner PD / pdha — is a sizing of option (b)**, the one this ADR recommends
against. If (a') is ruled, the row belongs to whoever owns the lock, and the graph PD would have
held has nothing to hold.

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

## Decision — (a'), ruled 2026-09-09

**`SELECT … FOR UPDATE` acquires a Percolator lock on each locked row's key, at the moment the
statement runs**, and the node-local table stops being the whole answer. Deadlock stops being a
graph question and becomes what it is for every other key in this system: two transactions waiting
on the same key, which the store and the client already have a mechanism for.

(b) was refused for the reason the measurement gives. It detects a deadlock this system does not
have — there is no cycle because there is no wait — so detection is the second problem, reachable
only after the first. And its edges are a sample: a cycle assembled from stale samples is a `40P01`
for a deadlock that never existed, which is a class of failure this crate has produced twice locally
where the graph is exact and in one process.

(c) lands regardless and lands first, because the divergence is real today and users meet it today.
The sentence is not "no cross-node deadlock detection" — it is **"`SELECT … FOR UPDATE` excludes
other sessions of the same node and not sessions of another node"**, and it goes in `DESIGN.md` §8
and §13, in `debts-v1.1.md`'s register, and in `phase-9-rails.md`'s divergence table, all of which
said something else before this ADR.

### What it costs on the wire: nothing

The lock this needs is already on the wire, and it is already the user's decision.
[ADR 0067](0067-the-check-mutation-and-the-latest-commit-question.md) §1 added `TxnMutation::Check`
as **tag 5**, approved 2026-09-04, and says what it is for in the approved text:

> **The check mutations already provide the lock.** A `Check` written at prewrite leaves a lock
> record on the checked key by the path prewrite already has, so a concurrent writer meets it and
> waits.

`Op::Check` commits as `Kind::Lock` — "the record kind this format has always reserved for a key
held but not written" — and `check_prewrite` gives it the same lock record a `Put` gets. So the
acquisition is an **ordinary `Prewrite`, sent early**: same method, same tag, same golden bytes, no
`TxnWrite` variant that does not already exist.

That is worth being exact about, because ADR 0067 §2 refused to smuggle this in and said so:

> **If a cluster-wide pessimistic lock is wanted later** — for `SELECT … FOR UPDATE` across nodes,
> which ADR 0057 §5 still declares node-local — it is a *different* change with a log entry behind
> it, and it should be asked for as one rather than smuggled in under this sentence.

It *is* being asked for as one, here, and the answer to the question 0067 raised is that the log
entry it was worried about already exists: `TxnCommand::Prewrite` is the replicated command, and a
`Check` mutation inside it is already replicated today under SERIALIZABLE. What changes is **when
the client sends it**, which is client behaviour and not a format. This ADR supersedes ADR 0057 §5's
"node-local" declaration and closes the half ADR 0067 named as not built.

**If the build finds a shape that needs a new tag, a new method, or a changed golden, it stops and
asks the human** — that is the charter's rule and nothing here weakens it.

### What it costs, measured

The draft asked for one number before anything was built — *"the cost of a lock-only prewrite for a
hundred-row `FOR UPDATE`, measured, against the hash lookup it replaces. If it is a round trip per
**statement** rather than per row, (a') is affordable; if it is per row, the batching has to come
first."* `crates/esker-sql/tests/lock_cost.rs` is that measurement, against a real three-store
cluster, medians of five rounds, and the answer is that **the question was aimed at the wrong cost**.

| | 10 rows | 100 rows | 200 rows |
|---|---|---|---|
| lock-only prewrite, one region | 0.6–2.3 ms | 7.1–10.8 ms | 15.0–18.5 ms |
| the same 100 keys spread over **three** regions | | 10.9–12.6 ms | |

* **It is linear in rows and flat in regions.** A hundred keys in three regions cost what a hundred
  keys in one region cost, and two hundred keys cost twice what one hundred do — about **0.1 ms per
  locked row**. So the round trips are not the price: `commit` already groups checked keys by region
  exactly as it groups writes, which is the batching the draft was asking whether it needed, and it
  is already written. What is left is the lock **record**, one replicated write per key, and that is
  inherent — it is what a lock another node can see *is*.
* **A hundred sequential round trips cost 9.4–11.8 ms**, 0.1 ms each. That is the same order as the
  batched prewrite of the same hundred keys, which says the same thing from the other side: the
  network is not where this goes.
* **The statement goes from ~3.5 ms to ~13 ms.** Today `SELECT … FOR UPDATE` over a hundred rows is
  3.5–4.2 ms, of which the in-process hash table is 0.0–0.5 ms — at or below the statement's own
  noise floor. The lock is what stops being free.
* **And it is about a tenth of the write it precedes.** The `UPDATE` those hundred rows were locked
  *for* costs 85–100 ms on the same cluster. A lock-only prewrite stages no value, so it is the
  cheap half of a transaction that was always going to pay the expensive half.

**So: affordable, per statement, and no batching work comes first.** The ADR's condition is met by
the code that already exists.

Two honesty notes on the numbers. They are a **debug build** — `--release` would move all of them
and would not change a ratio. And one round in four landed on a busy box and reported three times
the cost for the same arm (65 ms for 200 keys against 15–18 ms in the other three), which is what
these numbers are worth: an order of magnitude and a shape, not a precision. The shape is what the
decision needs, and it was the same in every round.

### The one thing (a') adds that is not free

Today no transaction is ever in the state "holds a lock and waits for another", and that is not an
accident — `docs/plans/cross-node-deadlock.md` and
`crates/esker-client/tests/prewrite_ordering.rs` are an argument and a test that it cannot happen:
locks are taken at commit, in one batch, in ascending key order, and `prewrite_or_roll_back` undoes
rather than waits.

**An eager lock ends that.** A transaction that locks row 1 at its first statement and row 2 at its
third holds one lock while asking for another, in the order the *application* named the rows — which
is the exact state a cycle needs, and the reason PostgreSQL has a detector at all. So (a') does not
remove the deadlock; it moves it from "cannot happen, and `FOR UPDATE` does not work" to "can
happen, and something must pick the victim".

The mechanism that catches it today is a waiter's resolution budget, and **on its own it picks the
wrong number of victims**. `resolve` rolls back another transaction's lock only when
`is_expired(start_ts, ttl_ms, now)` — a live holder is never rolled back — so two live transactions
in a cycle each exhaust `max_lock_resolutions` and *both* fail. One node answers `40P01` and kills
exactly one; PostgreSQL kills exactly one; "both abort" is closer than today's "both commit" and is
still a divergence.

**The recommendation, to be settled with the build rather than here: the youngest `start_ts` loses.**
A waiter whose own `start_ts` is *older* than the lock's owner may roll that lock back; a younger
waiter waits. It is wound-wait, it needs no graph and no second round trip, it is deterministic
across nodes because `start_ts` comes from the TSO (invariant 6), and it can be built out of the
rollback path that already exists — what it changes is *when* a resolver is allowed to roll a lock
back, which is the resolver's rule and not the format. The alternative — leave the budget as it is
and accept "both abort" — is cheaper and is a worse answer to the same question, so it should be
measured against, not assumed.

## Consequences

* **Until it is built**, two application servers behind two nodes get a lock that does not lock. It
  is silent, and it is the kind of silence that shows up as data that cannot be explained rather
  than as an error anyone can route. That is why (c)'s sentence lands first and alone.
* **A `FOR UPDATE` over a hundred rows costs about 10 ms** where the hash table cost less than the
  statement's own noise, and the cost is linear in rows rather than in round trips — measured, above.
  It is a tenth of the `UPDATE` those rows were locked for, and it is the price of a lock a second
  node can see.
* **The ordering property stops being a proof.** `prewrite_ordering.rs` keeps its two tests and they
  keep passing — the primary is still the smallest key of the write set — but the sentence that
  hangs off them, *"a wait-for chain that only ascends cannot come back to where it started"*, stops
  covering a transaction that locked before it wrote. `docs/plans/cross-node-deadlock.md` says so
  now, at its head.
* **`pg_locks` becomes a partial view.** It reads one node's table; a lock this node holds in the
  store for another node's benefit is the same fact, and the view has to learn where to look or say
  which half it is showing.
