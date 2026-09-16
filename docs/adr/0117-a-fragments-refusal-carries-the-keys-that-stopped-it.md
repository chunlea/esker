# 0117 — A fragment's refusal carries the keys that stopped it

Status: **Proposed**, 2026-09-16 — debt [#88](../plans/debts-v1.1.md), the number issued by the
coordinator. **This page stops at the design.** Every shape below changes a wire format that has a
golden, which `CLAUDE.md` reserves for the human; what follows is the byte-level cost of each shape
and the one measurement that should come before the ruling, not a decision.

## Context

[#86](../plans/debts-v1.1.md)'s fix is right and blunt, and the code that carries it already named
this page (`crates/esker-store/src/server.rs`, the fragment service):

> `TooFarBehind` and not a reason of its own: what it promises is exactly true here — *the same
> node may succeed later* — and a new `RefusalReason` is a wire change.

A columnar copy is built from the `write` column family, so a key whose transaction committed its
primary and left this secondary locked has no version in the copy and would simply be **absent**
from the answer. A learner is not a voter and cannot resolve the lock, so it refuses and the planner
falls back to the row path, which resolves it. That is correct and it is the whole of what a
fragment can say today.

The row path answers the same problem **per key**: `esker_txn::read` refuses *that* key with a
a `LockInfo` (`crates/esker-proto/src/txn.rs`) attached, and the client resolves it and asks
again. The fragment has no way to name a key, so the refusal is about the whole request.

**The blast radius is larger than debt #88's row says.** The row says one lock "sends the whole scan
back to rows"; `crates/esker-sql/src/exec/fragment.rs` says it sends back more than the scan:

```rust
Answer::Refused { reason, .. } => {
    return refused(columnar, shards.len(), refusal_text(reason));
}
```

That `return` leaves the **whole columnar node**, not the shard. Shards are walked in order; if
shard seven refuses, the six answers already received are dropped and the statement reads rows.
The run record then reports `asked: shards.len(), answered: 0`, so `EXPLAIN` does not preserve how
much of the answer was already in hand when it was thrown away.

### What it costs, measured

Measured 2026-09-16 (debt #88's own cell, `routing_differential.rs`, `#[ignore]`d):

| | 400 rows | 100,000 rows |
|---|---|---|
| clean scan | 68.2 ms | 653 ms |
| scan that met a lock | 86.6 ms (+27%) | **7.29 s (+1016%)** |
| how often | 1 scan in 5.5 | 1 scan in 30 |

The fallback's own cost is the **stable** half — five round medians of 7.177 … 7.306 s, a 1.8%
spread — while the clean baseline moves 2.9× as the writer adds rows. So the *ratio* slides 26× → 8.9×
without the fallback ever getting cheaper, and against the 400-row table the same fallback is **84×**
more expensive. The average scan carries about `(1/30) × (7.29 − 0.653) ≈ 221 ms` of this; the tail
carries all of it, which is the ground this page stands on.

## The wire, as it stands

`crates/esker-proto/src/fragment/mod.rs`:

```rust
Self::Refused { reason, detail } => {
    out.put_u8(1);                 // FragmentResp::Refused
    out.put_u8(reason.tag());      // Unsupported 1 | TooFarBehind 2 | NotColumnar 3
    out.put_str(detail);           // varint length, then UTF-8
}
```

and the golden that pins it, `crates/esker-proto/tests/golden/messages.hex` line 180, built by
`golden_fragment_responses()` in `crates/esker-proto/tests/messages.rs`:

```
response fragment-refused 01 06 | 01 01 19 6e6f2073756368206167677265676174653a20737464646576
                          ^^env   ^^ ^^ ^^ ^^ "no such aggregate: stddev"
                                   |  |  \ detail length 0x19 = 25
                                   |  \ reason 1 = Unsupported
                                   \ kind 1 = Refused
```

The split is not guessed: `0x19` must be the 25 bytes of `"no such aggregate: stddev"`, and no other
alignment of these bytes puts a 25 in front of 25 bytes. The leading `01 06` is the envelope every
`response fragment-…` line carries and no option below touches it.

**The key is already on this wire — as prose.** #86's refusal builds its detail with
`format!("a transaction at {start_ts} still holds a lock on {} in region {}", printable(&key), …)`,
and `printable` is not reversible: it passes ASCII-graphic bytes through and escapes the rest as
`\xNN`, and `\` is itself ASCII-graphic, so a key containing the four literal bytes `\x41` renders
exactly like a key containing the byte `0x41`. A decoder cannot recover the key from the detail, and
should not try. Every option below is the same key, promoted from prose to bytes.

## The fact that governs all three options

**Appending a field to `Refused` is not backward-compatible on this wire.** `FragmentResp::decode`
reads exactly kind, reason and detail, and `crates/esker-proto/src/messages.rs` (1361, 1687) then
calls `input.finish()?`, which is:

```rust
/// Finishes the body, refusing anything left over.
pub fn finish(self) -> Result<(), DecodeError> { … "{remaining} trailing bytes after the message" }
```

pinned by `codec.rs::trailing_bytes_are_refused`. A new **reason** is no better:
`RefusalReason::from_tag` answers `DecodeError::invalid("fragment.refusal", "unknown refusal
reason")` for a tag it does not know.

### Why [ADR 0074](0074-a-fragment-expression-node-is-added-by-tag-not-by-version.md) does not carry over

0074 decided that a new fragment expression node is a new **tag** and not a version bump, and it is
right, but its safety net is on the **request** side: DESIGN §16.2 says an evaluator that meets a
node it cannot evaluate refuses *the whole fragment*, and "a refusal is answered by the row plan the
routed plan already carries, at the same snapshot, with the answer the client would have had
anyway." **A response has no such rule.** Trace an old node meeting a new store:

1. `FragmentResp::decode` succeeds, `input.finish()` fails → `DecodeError`;
2. `FragmentSource::evaluate` answers `Err` — a *transport* failure, not `Answer::Refused`
   (`crates/esker-sql/src/fragment.rs`: "A **refusal is a value**, not an `Err`");
3. `exec/fragment.rs` asks `re_routed`, which re-resolves the shard and returns `None` because the
   region tiles itself unchanged — "**No news is not a repair.** A store that is simply down answers
   the same shard back";
4. → `refused(columnar, shards.len(), "a region could not be reached")` → rows.

So a half-upgraded cluster **answers correctly**, and that is the good news. The bad news is that it
loses **all** push-down silently, pays a wasted round trip plus a `shards()` re-resolution per shard
per query, and writes an `EXPLAIN` line that is false: the region was reached and answered.

**The upgrade order is therefore the mirror of [ADR 0114](0114-a-unique-key-being-written-waits-at-read-committed.md) §2's.** There the node produced the new
bytes and stores were upgraded first; here the **store** produces them and the **node** consumes
them, so **every reader is upgraded before any store starts writing the new field.** Two gating
shapes exist and one of them needs a read this page has not done:

* **by deployment order**, as 0114 did — cheap, and it makes a mis-ordered rollout a silent
  de-optimisation rather than a wrong answer;
* **by a new `Method`** (`FragmentEvaluate` beside a second method), so an old store answers
  "unknown method" and the node retries the old one. This is the only shape that is safe in *any*
  order. **Whether an unknown method is a clean refusal on this wire is not verified here** — it is
  one read, and it belongs to whoever builds this, not to this page.

## Option (a) — the refusal carries a key list

```rust
out.put_u8(1);
out.put_u8(reason.tag());
out.put_str(detail);
out.put_varint(keys.len() as u64);          // the house idiom: a count, then the elements
for key in keys { out.put_bytes(key); }     // (region.rs:289, raft.rs:292, result.rs:600)
```

decoded with `input.get_count("fragment.refusal.keys")?` — which refuses a count larger than the
bytes remaining, so a forged length cannot ask for a four-gigabyte `Vec`.

* **Golden:** every existing `fragment-refused` line gains a trailing `00` (the empty list), and a
  new line is needed for a non-empty one. `golden_fragment_responses()` is where both are built.
* **Bound:** the list needs a `MAX_…` on the ADR 0074 model — "a bound on what arrives from the
  wire, not a preference".
* **What it does not carry:** to decide a lock's fate the resolver needs the **primary**, and
  `esker-client`'s `classify` needs `start_ts` and `ttl_ms` besides. With a bare key the node must
  go read the lock again — an extra round trip, to the store that just told it about the lock.

## Option (b) — the element is the row path's `LockInfo`

Same list, with each element the shape the row path already refuses with:

```rust
out.put_bytes(&lock.key);
out.put_bytes(&lock.primary);
out.put_varint(lock.start_ts);
out.put_varint(lock.ttl_ms);
```

* **Cost of new code: nearly none.** `LockInfo::encode_to`/`decode_from` are `pub(crate)` in
  `esker-proto` — the *same crate* as `FragmentResp` — so the fragment module calls them without
  making anything newly public, and the SQL side gets the shape `esker-client` already resolves.
* **Golden:** as (a), plus the four fields per element.
* **Bytes:** a `LockInfo` repeats the primary for every key of one transaction.

### (b′) — one entry per blocking **transaction**, not per key

Worth separating, because the resolution unit is the transaction, not the key: `resolve()` decides a
transaction's fate by reading its **primary**, and one such round trip settles the fate of every
secondary that transaction holds. Rolling each key forward is still per key, but the expensive,
round-tripping half is per transaction. On a fill — the workload that produced the measurement above
— the blocking locks in a region are overwhelmingly *one* writer's, so (b′) is the same answer in a
fraction of the bytes.

## Option (c) — the store returns all it found, not the first

`crates/esker-store/src/columnar/region.rs`:

```rust
pub(crate) fn unresolved_lock(…) -> Result<Option<(Vec<u8>, u64)>> {
    …
    while iter.valid() && iter.key() < high.as_slice() {
        …
        if lock.start_ts <= ts && lock.kind != Kind::Lock {
            return Ok(Some((user_key, lock.start_ts)));   // ← the first one, and then it stops
```

**This is not a third way — it is the store half of (a) and (b).** With this `return` in place the
richest wire format in the world carries exactly one key: the node resolves it, re-asks, meets the
next lock, and pays a round trip per lock. Conversely (c) alone changes nothing observable, because
without (a) or (b) there is nowhere to put what it found except the detail string.

Its own costs are small and real: the walk stops being O(1) and becomes O(locks in the region's
share of the table) — the `lock` column family, bounded by locks and not by rows — and it needs a
cap, which means a truncated list, which means the node must be able to ask again.

## What the SQL side needs, in every option

Two things, and neither is on the wire:

1. **A seam for resolution.** Debt #88's row says "the SQL node resolves those and re-asks, which is
   what the row client already does with `LockInfo`" — but it is the **client** that does it, and
   `esker-sql` is deliberately written against the `FragmentSource` trait rather than
   `esker-client`'s concrete types, because "a node with **no** source is a real configuration"
   (`crates/esker-sql/src/fragment.rs`). So resolution belongs **behind `FragmentSource`**, as a
   method beside `evaluate`, not in `esker-sql`.
2. **A bounded retry.** A hot table's writer produces fresh locks continuously, so "resolve and
   re-ask" is not guaranteed to converge. The shape already exists ten lines away in the same file:
   `MAX_ROUTE_REPAIRS` and `re_routed`'s "No news is not a repair". The fallback to rows must stay,
   as the end of a small budget.

## Decoders to gate before it lands

Every file that names `FragmentResp` or `RefusalReason` outside the fragment module, with the number
of references — the list debt #88 asks for:

| File | refs | |
|---|---|---|
| `crates/esker-store/src/server.rs` | 24 | **product — the producer** |
| `crates/esker-client/src/fragment.rs` | 10 | **product — the first decoder** |
| `crates/esker-sql/src/exec/fragment.rs` | 5 | **product — the consumer that would resolve and re-ask** |
| `crates/esker-proto/src/messages.rs` | 2 | **product — the `finish()` that refuses trailing bytes** |
| `crates/esker-store/src/columnar/decode.rs` | 1 | product |
| `crates/esker-sql/src/fragment.rs` | 1 | product — the re-export (`FragmentAnswer as Answer`) |
| `crates/esker-proto/src/schema.rs` | 1 | product |
| `crates/esker-sql/tests/joint_gate.rs` | 11 | test — holds #86's correctness pin |
| `crates/esker-client/tests/fragment.rs` | 7 | test |
| `crates/esker-store/tests/snapshot.rs` | 5 | test |
| `crates/esker-sql/tests/routing.rs` | 5 | test |
| `crates/esker-sql/tests/routing_differential.rs` | 5 | test — holds the measurement above |
| `crates/esker-proto/tests/messages.rs` | 4 | test — **the goldens** |
| `crates/esker-client/src/testing.rs` | 3 | test double |
| `crates/esker-store/tests/schema_fetch.rs` | 2 | test |

## Compatibility with #86's blunt fix

All four shapes **keep** it. #86's check is the thing that finds the lock; what changes is that its
answer stops being thrown away. Concretely:

* The reason stays `TooFarBehind`. Its promise — *the same node may succeed later* — is still exactly
  true, and it is now true for a reason the node can act on. No new `RefusalReason`, so the one
  incompatibility #86's author called out is not spent.
* An empty key list is exactly today's behaviour and must keep meaning "refused, and I am not telling
  you why in keys" — **not** "no locks", because `NotColumnar` and `Unsupported` refusals carry no
  keys either.
* The detail string stays. It is what an operator reads, and it is the only thing that survives when
  the list is truncated.

## The benefit, and the measurement that should come first

If the blocking lock **can** be resolved, the fix takes the 1-in-30 scan from 7.29 s to roughly a
clean scan plus one resolution — 653 ms plus a round trip — so the tail falls by about an order of
magnitude and the mean loses its ~221 ms.

**That "if" is not yet measured, and it decides the size of the prize.** #86's case is a transaction
that *committed its primary* and left a secondary locked — resolvable, and the fix collects all of
it. A transaction that is still **live** is not resolvable by anyone: the node would resolve nothing
and fall back exactly as it does today, and the fix would buy a round trip and no answer. The 100k
measurement was taken beside a writer that held its transaction open for essentially the whole
window, so **most of its eleven encounters were plausibly live locks**, and nothing in that run
separates the two.

The instrument for the separation already exists and was built for [#108](../plans/debts-v1.1.md):
`routing_differential.rs`'s `locks_over_table` reads `kind`, `start_ts`, `ttl_ms` and `primary` from
the `lock` column family, and two samples one lease apart tell a heartbeated lock from a stranded
one. **The number this page wants before the ruling is one number**: of the locks a fragment meets
under a real write workload, what share belong to transactions that have already finished.

### A fourth shape, which the code suggests and the brief did not ask for

If the answer to that number is "few", none of (a)–(c) helps much, and the code already holds the
shape that would: **the fragment answers what it can and names the keys it could not**, and the SQL
node computes those few rows on the row path and folds them in. `exec/fragment.rs` already merges
one region's partials into a running set across shards — folding in one more small partial is the
same operation, not a new one. Its limits are real and should be written down before anyone likes
it: it works where partials already merge, `Body::Rows` needs an ordering argument it does not have
today, and ADR 0022's refuse-rather-than-half-answer instinct is pointed directly at it. It is
listed here because it is the only one of the four that helps when the blocking transaction is
**still live**, which is the case the measurement cannot currently rule out.

## What this page does not decide

1. **Which shape** — (b′) with (c) is the recommendation: the row path's own `LockInfo`, one entry
   per blocking transaction, over a store that returns every lock it found rather than the first.
2. **How it is gated** — deployment order, or a second `Method` that is safe in any order.
3. **Whether to measure the live/stranded split first.** Recommended: yes, and it is cheap — the
   probe exists, and it is the difference between a fix that removes an order of magnitude from the
   tail and one that buys a round trip.
