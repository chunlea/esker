# ADR 0067 — The `Check` mutation, and the question a waiter asks instead of guessing

Status: accepted (approved 2026-09-04) · Date: 2026-09-04 · Phase 9, the locking family ·
**Numbered 0067, not 0066**: both this lane and c7 claimed 0066 at their own HEAD before either
landed, and the later lander renumbers ·
Implements [ADR 0062](0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md) §2 and
closes `docs/plans/debts-v1.md` #1 and #2 · Builds on
[ADR 0057](0057-read-committed-waits-for-the-writer-in-front-of-it.md) §4

## Context

Two units were waiting on one ruling, and they were waiting for the same reason: the store knows
something the wire cannot carry.

* **ADR 0062 unit 8b.** SERIALIZABLE validates its read set at commit. In process that needs no lock,
  because validation and the version writes happen inside one critical section; across a network they
  are separate messages and a concurrent commit fits between them, so the checked keys must be
  **locked** for the length of the commit (§2 there). A range needs a shape a key cannot spell.
* **Unit 9 (debt #1).** `changed_since_statement` — *has this key a commit newer than my statement's
  snapshot?* — answers `false` on the store path because nothing can ask. The store already computes
  it: `TxnSnapshot::newest_write_after`, which `check_prewrite` calls.

Approved 2026-09-04. This ADR records **what** was approved and, more importantly, one thing in the
approval that cannot be true as written.

## Decision

### 1. `Check` is tag 5 and `CheckRange` is tag 6, on the mutation

`TxnMutation` gains two variants and `TxnWrite` — the **Raft log**'s mutation — gains the same two,
because `TxnCommand::Prewrite` is a replicated command. That is the shape ADR 0057 §4's tags 3 and 4
established and the human approved again here:

* every existing golden stays **byte-identical**, because a put and a delete still encode under tags
  1 through 4 exactly as they did;
* an older peer meets an unknown tag and **refuses**, rather than reading a longer message under a
  known tag as a short one with trailing bytes.

A `Check` names a key and a `CheckRange` names `[start, end)`. Neither carries a value, and at commit
each writes a `write` record that changes nothing — the uniform path, so a crash between the data
keys and the checks leaves locks the existing resolver already understands (ADR 0062 §4).

### 2. `LatestCommit` is a read-only method, and it does **not** take a lock

The approval says the cluster lock "returns the key's newest commit timestamp as part of acquiring
it… read-only, no Raft log change, no `TxnWrite` variant".

**Those two halves cannot both hold, and the ADR says so rather than implementing a contradiction.**
A lock another node can see is replicated state: acquiring one *is* a write, needs a log entry, and
needs a `TxnWrite` variant — that is what TiKV's `AcquirePessimisticLock` is. A read-only method with
no log change is exactly what it says: a **question**, not an acquisition.

What was actually needed turns out to be the question alone, and the reason is §1:

> **The check mutations already provide the lock.** A `Check` written at prewrite leaves a lock
> record on the checked key by the path prewrite already has, so a concurrent writer meets it and
> waits. ADR 0062 §2's atomicity requirement is met by tag 5, not by a second locking RPC.

So this ADR takes the read-only half and drops the acquisition half, which is the half nothing needs:

```
TxnKvReq::LatestCommit  { key }        ->  TxnKvResp::LatestCommit { newest: Option<u64> }
```

`newest` is the `commit_ts` of the newest `write` record for the key, or `None` for a key never
committed. Read-only: no log entry, no `TxnWrite` variant, no lock. An older peer meets an unknown
**method** and refuses it exactly as it refuses an unknown tag.

**If a cluster-wide pessimistic lock is wanted later** — for `SELECT … FOR UPDATE` across nodes,
which ADR 0057 §5 still declares node-local — it is a *different* change with a log entry behind it,
and it should be asked for as one rather than smuggled in under this sentence.

### 3. What each unit does with it

* **8b**: the client records the read set (keys and ranges), and at commit sends one `Check` per key
  and one `CheckRange` per range beside the mutations. The store validates each against the request's
  `start_ts` — `newest_write_after` for a key, a range scan of the `write` column family for a range
  — and refuses the whole prewrite if any has moved, which is `40001` with ADR 0062's sentence.
* **Unit 9**: `StoreTxn::changed_since_statement` asks `LatestCommit` and compares to the statement's
  read timestamp. One round trip per locked key per statement, and only for a write statement under
  READ COMMITTED whose lock was taken without waiting.

## Consequences

* Two mutation tags and one method. Existing goldens unchanged; new goldens are **additive rows**.
* The `Check` path costs one lock record per checked key for the commit's duration, which is what
  makes a reader block a writer under SERIALIZABLE — priced in ADR 0062 §2 and unchanged here.
* `changed_since_statement` stops being a declared window on the store path. What remains declared
  there: a deadlock spanning two nodes (the wait-for graph is one node's) and the node-local scope of
  `FOR UPDATE`.
* **The half not built is named**: no cluster-wide lock acquisition. Anyone reading the approval and
  looking for it will find this paragraph instead of a gap.
