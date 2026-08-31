# Percolator in Esker: the byte-level specification

What `docs/DESIGN.md` §8 states in one table, written out to the byte. This file is the contract
between `esker-txn` (which defines these bytes), `esker-store` (which writes them through Raft) and
`esker-client` (which drives the protocol); if it and the code disagree, one of them is wrong and
both change in the same commit (`CLAUDE.md`, "How to work").

Source: Peng & Dabek, *Large-scale Incremental Processing Using Distributed Transactions and
Notifications*, OSDI 2010, §2. The paper's Bigtable columns become Esker column families; its
`Bigtable::Write` becomes an atomic `WriteBatch` through one region's Raft group.

## 1. The mapping: paper column → CF → bytes

The paper stores four columns per row: `lock`, `write`, `data` and `notify`/`ack` (the observer
machinery, which Esker does not implement). Esker keeps the first three.

| Paper | Esker CF | Engine key | Value | Versioned by |
|---|---|---|---|---|
| `c:lock` | `lock` | `'x' ++ enc(k)` | [`LockRecord`](#3-lockrecord) | not versioned — at most one lock per key at a time |
| `c:write` | `write` | `'x' ++ enc(k) ++ !commit_ts` | [`WriteRecord`](#4-writerecord) | `commit_ts` |
| `c:data` | `default` | `'x' ++ enc(k) ++ !start_ts` | the value, unframed | `start_ts` |

where

- `'x'` is the TxnKV namespace byte of `docs/DESIGN.md` §3 (`esker_keys::prefix::TXN`);
- `enc(k)` is `esker_keys::encode_bytes` — the memcomparable group encoding, **not** the raw user
  key; §2 says why;
- `!ts` is `esker_keys::enc_ts` — the bitwise complement, big-endian, eight bytes, so newer versions
  of a key sort **first**.

A value of at most `SHORT_VALUE_MAX_LEN` = 255 bytes is inlined in the `lock` and `write` records
and **no `default` entry is written for it**. So a `default` entry exists exactly when some
`WriteRecord` (or the `LockRecord` that will become one) has `kind = Put` and no `short_value`.

Every key named here — in a record's `primary` field, on the wire, in a client call — is the **user
key**. The `'x'` prefix and the group encoding are applied on the way into the engine and stripped on
the way out, by `esker-txn::key`, on the same rule that makes the store rather than the client apply
`'r'` to RawKV keys (`docs/DESIGN.md` §10).

## 2. Why the user key is group-encoded first

`docs/DESIGN.md` §3 writes the layout as `'x' ++ user_key ++ enc_ts`. That is only correct when
`user_key` is prefix-free, which the SQL layouts are and which arbitrary TxnKV keys are not.

Take the raw form and two keys, `"a"` and `"ab"`, and write the versions out:

```
'x' "a"      !0    =  78 61 ff ff ff ff ff ff ff ff
'x' "ab"     !0    =  78 61 62 ff ff ff ff ff ff ff ff
```

The third byte decides, and `ff > 62`, so `"a"` at ts 0 sorts *after* `"ab"` at ts 0: one key's
versions are interleaved with another's. A seek for "the newest version of `a` at or below `ts`"
then lands inside `ab`'s versions, and the prefix check that is supposed to catch it — "does the
found key start with `'x' ++ "a"`?" — says yes, because `"a"` **is** a prefix of `"ab"`. The read
returns another key's value with nothing anywhere reporting an error.

Group-encoding first removes the case rather than checking for it: the encoding is prefix-free, so
no encoded key is a byte prefix of another and every key's versions form one contiguous run.

```
'x' enc("a")  !0  =  78 61 00 00 00 00 00 00 00 f8 ff ff ff ff ff ff ff ff
'x' enc("ab") !0  =  78 61 62 00 00 00 00 00 00 f9 ff ff ff ff ff ff ff ff
```

`esker_keys::prefix::txn_key` is the raw form and is left alone: it is correct for the callers it
has, and its own property test excludes the prefix case explicitly. `esker-txn` does not use it.

The cost is one byte per eight of key, and it is what TiKV pays for the same reason.

## 3. `LockRecord`

The value in the `lock` CF. Written by `Prewrite`, deleted by `Commit` and by `Rollback`.

```
kind         : u8         1 Put | 2 Delete | 4 Lock          (3 Rollback is never a lock)
start_ts     : varint
ttl_ms       : varint     milliseconds, from this lock's start_ts
primary_len  : varint
primary      : primary_len bytes                             the user key of the primary
short_value  : 0x00                                          absent
             | 0x01 ++ len:u8 ++ len bytes                   present, len ≤ 255
```

Rules a decoder enforces, each of them a rejected input rather than a tolerated one
(`CLAUDE.md` invariant 9):

- `kind` outside `{1, 2, 4}` is an error. **A rollback is not a lock**: a rolled-back transaction
  leaves a marker in `write`, never an entry in `lock`, and accepting kind 3 here would let a
  corrupt byte turn a tombstone into a live lock nobody can resolve.
- `short_value` present with `kind != Put` is an error — a delete has no value to inline.
- The presence byte is `0x00` or `0x01` and nothing else, so the encoding is canonical: one byte
  string per record.
- Trailing bytes after `short_value` are an error.
- A `primary` of zero length is an error: every transaction has a primary, and a key of no bytes is
  not a key anything can resolve against.

`short_value` carries its own length even though it is the last field, because "the rest of the
record" and "255 bytes of value" stop being the same thing the moment a field is added after it.

## 4. `WriteRecord`

The value in the `write` CF, at `commit_ts`. Written by `Commit` and by `Rollback`.

```
kind         : u8         1 Put | 2 Delete | 3 Rollback | 4 Lock
start_ts     : varint     which transaction wrote it — the link to the `default` entry
short_value  : 0x00 | 0x01 ++ len:u8 ++ len bytes
```

Same rules: unknown kind, a short value on a non-`Put`, a non-canonical presence byte and trailing
bytes are all errors.

The four kinds mean:

| `kind` | At `commit_ts` | Read at `ts ≥ commit_ts` sees |
|---|---|---|
| `Put` | the transaction wrote a value | the value: `short_value`, else `default[k, start_ts]` |
| `Delete` | the transaction deleted the key | nothing |
| `Rollback` | the transaction at `start_ts` was rolled back; `commit_ts == start_ts` | nothing — the reader skips to the next older record |
| `Lock` | the key was locked but not written (`SELECT … FOR UPDATE`-shaped) | nothing — skipped, as above |

A `Rollback` record sits at `commit_ts == start_ts`. Nothing else can: a real commit has
`commit_ts > start_ts`, so the slot is free and the marker is findable by the one number a resolver
has in its hand.

## 5. The four operations, as reads and mutations

Every operation below is a pure function in `esker-txn::percolator`: it takes a snapshot, returns a
decision and a list of mutations, and performs no I/O. The store applies the mutations as one
`WriteBatch` through Raft, which is where the atomicity in `CLAUDE.md` invariant 1 comes from.

The snapshot answers five questions (`TxnSnapshot`):

```
get_lock(k)                      → the lock on k, if any
seek_write(k, ts)                → the newest (commit_ts, WriteRecord) with commit_ts ≤ ts
newest_write_after(k, ts)        → the newest (commit_ts, WriteRecord) with commit_ts > ts
write_of_txn(k, start_ts)        → the record the transaction at start_ts left here:
                                   its commit, or its rollback marker
get_value(k, start_ts)           → the `default` entry
```

Four of the five are a point get or a single seek. `write_of_txn` is a **bounded scan**: a
transaction's own record is filed under some `commit_ts ≥ start_ts`, so the walk runs from the
newest version down to `start_ts` and stops. There is no index from `start_ts` to `commit_ts`, and
adding one would be a second structure to keep consistent with the first; TiKV's resolver does the
same walk for the same reason.

`seek_write(k, ts)` is one forward seek to `'x' ++ enc(k) ++ !ts` followed by a prefix check, and
that is the whole reason `enc_ts` is complemented. **The boundary is inclusive**: a version committed
at exactly `ts` is visible at `ts`, because `!commit_ts == !ts` makes the seek land on it.

### 5.1 Read at `ts`

1. `get_lock(k)`. A lock with `start_ts ≤ ts` blocks the read: it may commit at a `commit_ts ≤ ts`
   and the reader cannot tell yet. The answer is `Locked{lock_info}`, and the client resolves it
   (§5.5) and retries. A lock with `start_ts > ts` belongs to a later transaction and is ignored.
2. `seek_write(k, ts)`. Nothing → the key does not exist at `ts`.
3. `Rollback` or `Lock` → not a value; step back to `seek_write(k, commit_ts - 1)` and repeat.
   (`commit_ts == 0` ends the walk.)
4. `Delete` → the key does not exist at `ts`.
5. `Put` → `short_value` if present, else `get_value(k, record.start_ts)`. A missing `default` entry
   for a `Put` is **corruption**, reported as an error and never as an absent key: the two mean
   opposite things and a reader that confuses them silently loses a write.

### 5.2 Prewrite (key `k`, `start_ts`, primary `p`, mutation)

Two checks, and **both** are load-bearing:

1. `newest_write_after(k, start_ts)`. A commit newer than our snapshot is a **write-write
   conflict**: another transaction wrote what we read. Fail; the client aborts and may retry with a
   fresh `start_ts`. *(Skipping this check breaks snapshot isolation's lost-update guarantee.)*
   A record whose `start_ts` is *our own* is not a conflict with ourselves — see check 2.
2. `write_of_txn(k, start_ts)`. A `Rollback` means this transaction was already rolled back by
   someone who found its lock expired. Fail — resurrecting it would commit a transaction another
   party has already told a reader is dead. A *commit* there is our own, from an attempt whose
   answer was lost: succeed and write nothing. Neither is reachable from check 1, because a
   rollback marker sits at `commit_ts == start_ts`, below the range that check looks at.
3. `get_lock(k)`. Any lock with a different `start_ts` is a **lock conflict**: report it so the
   client can resolve it. A lock with *our* `start_ts` is our own earlier attempt: succeed and write
   nothing, which is what makes `Prewrite` idempotent and what makes an ambiguous `Prewrite` safe to
   resolve rather than fatal. *(Skipping this check lets two live transactions both hold the key.)*

A `Prewrite` is a **batch**, and it reports a status for every key rather than stopping at the first
refusal ([ADR 0016](adr/0016-txnkv-on-the-wire.md) decision 1). The batch is still one decision — if
any key is refused, none is written — but the client learns about every lock at once and clears them
in one round, instead of one round per contended key. A `Get` or a `Scan`, which ask about one thing,
still answer a lock through `Locked{lock_info}`.

Mutations on success:

```
lock    put  'x' enc(k)                  LockRecord{kind, start_ts, ttl, primary: p, short_value?}
default put  'x' enc(k) !start_ts        value          — only when the value is > 255 bytes
```

### 5.3 Commit (key `k`, `start_ts`, `commit_ts`)

With our lock present:

```
write   put     'x' enc(k) !commit_ts    WriteRecord{kind: lock.kind, start_ts, short_value?}
lock    delete  'x' enc(k)
```

Without a lock, the answer is decided by the `write` CF and never by guessing:

- a record with our `start_ts` at some `commit_ts` and `kind != Rollback` → we already committed;
  the call is a duplicate and succeeds.
- a `Rollback` at `commit_ts == start_ts` → we were rolled back; the commit **fails**.
- neither → `TxnLockNotFound`: the lock is gone and nothing says why. Fail.

**Primary first.** The primary's `write` record is the commit point of the whole transaction: it is
the single fact every resolver reads. A secondary committed before it would be a committed value
belonging to a transaction that no one can yet call committed, and a crash in that window leaves a
state the resolution rules of §5.5 classify *wrongly* — rolled back, because the primary is still
locked and eventually expires. `esker-txn` makes it unrepresentable rather than documented:
`commit_secondary` requires a `PrimaryCommitted` token, and the only way to obtain one is to consume
the primary's `PrimaryCommit` plan, which the caller does after the primary's batch is durable.

### 5.4 Rollback (key `k`, `start_ts`)

```
write   put     'x' enc(k) !start_ts     WriteRecord{kind: Rollback, start_ts}
lock    delete  'x' enc(k)               — only if the lock is ours
default delete  'x' enc(k) !start_ts     — only if the lock is ours
```

The marker is written **even when there is no lock**. That is the case that matters: a `Prewrite`
whose answer was lost may arrive at the store after the client gave up, and without a marker it
would succeed and lock a key on behalf of a transaction that has already reported failure. The
marker makes the late `Prewrite` fail (§5.2 check 2).

Rolling back a transaction that has already committed is an error, not a no-op.

### 5.5 Resolve (a lock, and the state of its primary)

A reader or a writer that meets a lock reads the *primary's* `write` CF at the lock's `start_ts`:

| Primary state | Resolution |
|---|---|
| a `write` record with `start_ts`, `kind != Rollback`, at `commit_ts` | **roll forward**: commit this key at `commit_ts` |
| a `Rollback` at `commit_ts == start_ts` | **roll back**: §5.4 on this key |
| still locked, `now - ttl_start ≤ ttl_ms` | **wait**: back off and look again |
| still locked, TTL expired | **roll back the primary first**, then this key |
| no lock and no record | **roll back**: the lock we are holding refers to a primary that never got one |

TTL is measured in the physical part of the timestamps, never against a node's wall clock
(`CLAUDE.md` invariant 6): `ts = physical_ms << 18 | logical`, so a lock is expired when
`physical(now_ts) > physical(lock.start_ts) + lock.ttl_ms`, and `now_ts` is a timestamp the caller
got from the oracle. Default TTL is 3 s (`docs/DESIGN.md` §14), extended by `Heartbeat` while the
owning client is alive.

Rolling the primary back before its secondaries is the mirror of committing it first, and for the
same reason: the primary is the fact, and it has to be settled before anything reads it as settled.

## 6. What snapshot isolation gives, and what it does not

**Guaranteed.**

- *Snapshot reads.* Every read in a transaction sees the database as of `start_ts`, so a transaction
  never sees another's partial work and never sees the same key change under it. This is the
  `seek_write(k, start_ts)` of §5.1 plus the lock check.
- *No dirty reads.* An uncommitted write lives only in the `lock` CF and in the writer's own buffer.
  A reader that meets the lock resolves it; it can never read through it.
- *No lost updates on a written key.* Prewrite fails if any commit happened after `start_ts`
  (§5.2 check 1). Two transactions that write the same key cannot both commit — this is
  first-committer-wins, and it is what makes `read k; write k+1` safe.
- *Atomicity.* The primary's `write` record is the commit point, and it is one key in one region, so
  it lands or it does not. Everything else is roll-forward from that fact.
- *Read-your-writes.* Enforced on the client, in the write buffer, before any request goes out.

**Not guaranteed: write skew.** Two transactions may read an overlapping set and write *disjoint*
keys, and both commit, even though no serial order produces the result. The classic case is the
on-call constraint "at least one doctor on duty": two doctors read `on_call = {A, B}`, each writes
only their own row, both prewrites see no conflict on the key they write, and both commit. The
invariant across the two rows is broken.

This is a property of snapshot isolation, not a bug in this implementation, and the reason is
visible in §5.2: prewrite checks conflicts on the keys a transaction **writes**, and a write-skew
pair conflicts only on the keys it **reads**.

Two things would fix it, and both cost:

- **Take a lock on the read.** `SELECT … FOR UPDATE` prewrites the read keys with
  `kind = Lock` (§4), which puts them into the conflict check without giving them values. The
  encodings here already carry it; the API does not expose it yet. This is the cheap, opt-in fix,
  and it is what the paper's users do.
- **Serializable snapshot isolation.** Track the read set of every transaction, and at commit
  detect an rw-dependency cycle (Cahill's SSI). That needs the read set on the server, a
  conflict-detection structure that is not the `write` CF, and an abort path for transactions that
  have read nothing wrong themselves. It is a phase of its own; `docs/DESIGN.md` §15 keeps it open.

**Also not guaranteed:** no read at a timestamp *older* than the GC safepoint (§7), and no
cross-transaction ordering beyond what the timestamps say — the oracle is the only clock
(`CLAUDE.md` invariant 6).

### 6.1 What the guarantees compose into: a unique constraint

The lost-update guarantee is stronger than it first looks, and one composition of it is worth
stating outright because a layer above depends on it. **Check-then-insert on a single key is
safe.** A transaction that

1. reads key `k` at its own `start_ts` and finds nothing, then
2. writes `k` and commits,

cannot both succeed alongside another transaction doing the same thing to the same `k`. Exactly one
commits; the other is refused, and refused *before* it writes anything.

This is not a new rule — it is §5.2's two checks applied to a key that did not exist — but the two
orderings it can take are worth spelling out, because only one of them is the obvious one:

- **The loser arrives while the winner still holds its lock.** Prewrite's check 3 answers
  `Locked`, the loser resolves the lock (§5.5), finds the winner committed, rolls it forward, and
  retries — and *now* check 1 sees a commit above its snapshot and answers a conflict. Two
  round trips, one refusal.
- **The loser arrives after the winner committed.** Check 1 answers a conflict immediately.

Either way the refusal is a write-write conflict on `k` and the loser must start again at a fresh
`start_ts` — at which point its read of `k` finds the winner's row, and the constraint holds.

**Which key lost is part of the answer.** A `Prewrite` reports per key
([ADR 0016](adr/0016-txnkv-on-the-wire.md) decision 1), so the refusal names the key rather than only
the transaction — and it has to, because a lost race on an ordinary row and a lost race on a unique
index entry are the same event to this layer and different events to the one above it.
`esker_client::Error::TxnConflict` carries `key: Option<Bytes>`; `None` means the method that refused
does not answer per key (`Commit`, `Rollback`) and no key was named, which is not the same as no key
having lost.

`esker-sql` builds unique-index enforcement on exactly this: the index entry is the key, a snapshot
read proves it absent, and an ordinary `Put` claims it. No `SELECT … FOR UPDATE` and no `Lock`-kind
record is needed, because the conflict is on a key the transaction **writes**, which is the half of
the read/write set prewrite checks. (The half it does not check is what §6's write skew is about,
and a uniqueness constraint does not fall in it: it is one key, and the transaction writes it.)

`crates/esker-txn/tests/protocol.rs` tests both orderings — `two_inserts_of_one_new_key_leave_one
_winner` and its lock-first sibling — so the property is pinned rather than inferred.

## 7. Garbage collection, and the one rule that is easy to get wrong

PD publishes a **safepoint**: the ts below which old MVCC versions may go. A compaction filter walks
each key's versions newest-first and:

- keeps every version with `commit_ts > safepoint` — some open transaction may still read it;
- keeps the **newest** version with `commit_ts ≤ safepoint`, because that is what a read at the
  safepoint returns, and dropping it would make an existing key vanish;
- drops the versions older than that one, and their `default` entries;
- drops a `Delete` record once nothing older survives, since a key with no versions reads the same
  as a key whose newest version is a delete.

**Rollback markers are the exception.** A `Rollback` at `commit_ts == start_ts` may only be dropped
once the safepoint has passed `start_ts` — and that is the whole point of it: below the safepoint
there can still be an in-flight `Prewrite` from that transaction, and §5.2's second check is the only
thing that stops it. Dropping the marker early re-opens the window the marker exists to close, and
the failure is a transaction that everyone agreed was aborted quietly committing one key of its
write set, days later.

The filter itself is `esker-store`'s (deliverable 2, deferred); this section is the rule it has to
implement, written where the encoding is defined so that the two cannot drift.

## 8. What is not implemented

Pessimistic locks, async commit, one-phase commit, `min_commit_ts`, and the paper's notification
columns. `docs/DESIGN.md` §15 keeps the first three open; the last is out of scope for a KV store.
`Lock`-kind records exist in the format (§4) but no API writes them yet — the tag is reserved so
that adding `SELECT … FOR UPDATE` is not a format change.
