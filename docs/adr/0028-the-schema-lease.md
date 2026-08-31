# 0028 — The schema lease is a method, not a field

Status: **accepted, and built** (`docs/plans/phase-6e.md` unit 3). See
[ADR 0020](0020-online-schema-change.md) for what the lease is for and
[ADR 0021](0021-time-machine.md) for the retention window it shares a number with.

## Context

ADR 0020 needs a **schema lease**: a deadline past which a SQL node may not serve a write from a
cached schema, so that PD's step clock can advance on a timer rather than on a poll of every node it
might not be able to reach. It says the lease should ride "in whatever PD already sends nodes
periodically", and that is the problem — **PD sends a SQL node nothing.** Every message in
`PdReq`/`PdResp` is a store's or a region's; a SQL node is the first thing above the store that is
neither, and today it does not speak to PD at all.

So the lease needs a way to travel, and there were two shapes.

## The shape that was rejected, and why the rejection is the interesting part

**A field on `PdResp::Tso`.** Elegant: a writer needs a timestamp to begin a transaction and another
to commit one, so every writer talks to PD *by construction*. Fail-closed would then be free — a
node that cannot reach PD cannot get a timestamp, cannot begin a transaction, and therefore cannot
write, with no separate liveness path to get wrong. `docs/plans/phase-6e.md` §3 proposed exactly
this.

It was rejected on the constitution. `CLAUDE.md` says to stop and ask before changing "an on-disk or
wire format that already has a golden test", and `PdResp::Tso` has one. The phase's brief widened
this lane to `esker-proto` and pre-approved a *catalog* format change by name; it did not pre-approve
a wire one, and reading a widened lane as a blanket permission is how a rule stops meaning anything.

**What the rule bought is worth stating, because it is not nothing.** Looking for a shape that did
not need the change produced a better one:

## Decision: a new method, `Pd::SchemaLease` (0x0307)

Additive. No existing message's bytes move, no existing encoder or decoder changes, and every
existing golden is byte-identical — the golden file *gains* two lines rather than having any
rewritten. An old peer that never sends the method is unaffected by its existence.

```text
0x0307  Pd::SchemaLease   →  lease_ms, step_interval_ms, removal_extra_ms
```

The request carries nothing: the answer is a cluster-wide number with one writer, exactly like the
GC safepoint.

Fail-closed is no longer free, and it costs one line rather than a design: the node's lease source
returns `None` when it cannot renew, and `Backend::schema_lease_remaining` returning `None` refuses
every write. The extra round trip is **once per lease period**, not once per timestamp, which is
cheaper than the rejected shape rather than dearer.

### What is in the answer, and why three numbers rather than one

* `lease_ms` — how long a node may serve writes from a cached schema.
* `step_interval_ms` — `lease_ms + lock_ttl_ms`. Sent rather than derived by the caller so that the
  arithmetic has **one home**: a node holding its own opinion about how long it is safe to be behind
  is a node PD cannot reason about.
* `removal_extra_ms` — the MVCC retention window, which a **removing** step waits on top of the
  interval and an **adding** step does not (ADR 0020, as amended). Separate rather than folded in
  because retention defaults to an hour: folding it in would price every `CREATE INDEX` at the
  retention window.

## The lease is about writers, and reads are never gated

Worth stating here because the tempting mistake is symmetry. A reader's snapshot already agrees with
the rows it can see: a transaction's catalog read is at its own `start_ts`, so it never sees a
schema its rows disagree with. Gating reads would add stalls, close no hole, and take a node that
has lost PD from *degraded* to *useless* — which is the wrong trade for a bound only writers can
violate.

## PD is told its inputs rather than keeping copies

ADR 0020 says PD "already owns both inputs" to the step interval. It does not. The lock TTL is
`esker_client::LOCK_TTL_MS` and the retention window is `esker_sql::catalog::DEFAULT_RETENTION_MS` —
both in crates *above* PD in `CLAUDE.md`'s layer table, which PD does not link.

They are therefore `PdOptions` fields whose defaults name their sources, rather than constants copied
into `esker-pd`. A copy is a number that can drift, and a step interval short by exactly the drift is
not merely wrong, it is unsafe: it is how a node ends up two states behind. The **interval** stays
computed — one somebody could tune independently of the bounds it exists to keep would be one
somebody could tune below them.

The cost, stated: a cluster that runs a different lock TTL and does not say so here gets a step
interval that is too short. That is a configuration error with a real consequence, and it is the
price of not linking upwards.

## Consequences

* One new method code, `0x0307`, and two new golden lines. Nothing existing moved.
* A SQL node speaks to PD for the first time. Its lease source is a trait
  (`esker_sql::backend::SchemaLease`) rather than a `PdClient` field, for one reason that matters:
  **the test that proves fail-closed has to be able to stop answering.** Untested fail-closed is
  fail-open with good intentions.
* A node with **no** lease source writes freely, which is right for a cluster with no placement
  driver and is a different thing from a node whose lease has run out. "Nobody is coordinating" and
  "I have lost the thing that coordinates" are not the same fact, and only the second is a reason
  to stop.
