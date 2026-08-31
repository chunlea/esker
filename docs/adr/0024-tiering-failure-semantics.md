# 0024 — An upload is not on the write path

Status: accepted (phase 6b). Implements the tiering hook in `docs/DESIGN.md` §13 and
`prompts/06-sql-serverless.md` §6b item 1. See `docs/plans/phase-6b.md`,
`crates/esker-engine/src/fs/tier.rs`, [ADR 0025](0025-s3-transport-and-tls.md) for the
transport half of the same feature.

## Context

Phase 6b puts SSTs in object storage. The mechanical part is easy — an SST is immutable the
moment `TableBuilder::finish` returns, so copying one to a bucket is a `PutObject` and reading
it back is a ranged `GetObject`. The part that needs deciding is what happens when any of that
does not work, because the answers determine whether `CLAUDE.md` invariant 1 still holds.

The invariant: *log before state, fsync before ack*. A write is acknowledged when its WAL bytes
are durable. Object storage is nowhere in that sentence, and this ADR exists to keep it that
way under every failure it can be asked about.

There is a second pressure in the other direction. The point of tiering is that local disk
stops being the limit on how much data a store can hold. If the only way to free local bytes is
a successful upload, then a tier that is down eventually becomes a tier that stops writes. That
is not a bug to be engineered away; it is the honest consequence, and the decision is to make it
*backpressure* rather than *data loss*.

## Decision 1: the upload happens after the manifest edit, never before it

The order is fixed:

```
build the SST locally  ->  sync_data  ->  manifest edit names it, location = Local  ->  ack
                                                                                       \
                                                                                        upload
```

An upload is never awaited by a flush, a compaction, or a foreground write.

Two reasons, and only the first is about correctness:

- **Invariant 1 is a statement about local durability.** If a flush waited on `PutObject`, the
  acknowledgement of a write would depend on a network, and the phase-1 crash tests — which
  kill the process and assert every acked write survives — would be asserting something about
  a remote service. Worse, an upload that *succeeded* while the local fsync had not would put
  the state ahead of the log, which is the exact inversion the invariant forbids.
- **Tail latency.** `DESIGN.md` §1 asks for predictable tail latency and no unbounded write
  stalls. A compaction blocked on S3 holds L0 above its trigger, and L0 above its stop trigger
  halts writers. One slow bucket would then be indistinguishable from a stuck disk.

The cost of this ordering is a window: between the edit and a successful upload, the only copy
of those bytes is local. That window is exactly as dangerous as a non-tiered database, which is
to say it is the situation Raft replication already covers. Tiering is not a substitute for
replication and this ADR does not pretend it is.

## Decision 2: an upload failure fails nothing

`PutObject` returning an error, timing out, or finding no MinIO at all has one effect: the file
stays local and its number goes back on the retry queue. Specifically it does **not**:

- fail the flush or compaction that produced the file,
- fail any write,
- mark the database in an error state,
- change what any read returns.

The file is `Local` in the manifest, complete on disk, and readable. Every read of it is a local
read. The only thing that has not happened is the part that would have let us delete it.

**The backoff is one attempt per maintenance pass.** A file that fails goes to the back of the
upload queue, and a pass takes at most `batch` *distinct* files, so a bucket that is refusing
everything is retried at the uploader's own cadence — a new SST, or its five-second idle tick —
rather than spun on. This is deliberately not an exponential backoff with a jitter: that would
need a clock inside the engine for the simulator to fake, and the pass cadence already bounds
the retry rate by the flush rate, which is the thing that actually matters. If a measurement
ever shows an idle database retrying a dead endpoint too eagerly, lengthening the idle tick is
the knob, and it is one knob rather than three.

A file whose upload has failed *n* times is not otherwise special-cased: it is retried forever,
because the alternative is a file that is permanently unevictable and silently so. The failure
count is kept for the log line and for nothing else.

## Decision 3: local disk full is backpressure, and the tier is not a relief valve

The governor may delete a local SST only when the object store holds it. When uploads are
failing, nothing is evictable, and the local SST directory grows exactly as it would without a
tier configured. At that point the engine does what it already does when a disk fills: the write
fails with the underlying `ENOSPC`, reported as an I/O error, having acknowledged nothing.

We considered and rejected two alternatives:

- **Evict anyway and re-fetch later.** This deletes the only copy of a live file. No.
- **Stall writes early, before the disk is actually full, to keep headroom for uploads.** A
  tempting knob and a real one for a later phase, but it converts an operational problem into a
  latency cliff whose trigger nobody can predict from the outside. Deferred, with a
  `TODO(phase-7)`; when it lands it should be a stated watermark, not a heuristic.

What the governor *does* do is emit a `tracing::warn!` with the backlog depth and the age of the
oldest un-uploaded file, so that "the bucket is unreachable" is visible long before it is fatal.

## Decision 4: `location` records what has happened; it never decides where to look

`FileMeta::location` is `Local` or `Tiered`. The read path does not branch on it:

```
open(NNNNNN.sst) = local file if present, else fetch the object, else NotFound
```

This is deliberate, and it is what makes the field safe. A crash between a successful upload and
the promote edit leaves a file recorded `Local` that is in fact in the bucket — and everything
still works, because the local copy is what gets opened, and the next retry re-uploads
idempotently (`PutObject` of identical bytes to the same key). A file recorded `Tiered` whose
object has been deleted out from under us is an error, and it is the *same* error a missing local
file gives, because it is the same situation: the bytes are gone.

The field earns its place three times over regardless: the governor consults it to know what is
evictable, the sweep consults it to know what to delete remotely, and `manifest-dump` shows an
operator where the bytes are. What it must never become is a lookup table the read path trusts
over the filesystem.

## Decision 5: an object is deleted only when no live version names it

The phase-1 rule — *never delete a file a `Version` still references, and never delete a pending
output* — extends to objects unchanged, with one adjustment to how the set is computed.

The local sweep lists the directory. It cannot be the source of truth for objects, because an
evicted file is absent from the listing and its object would then never be reclaimed. Object
deletion therefore comes from the version set's live-file set directly:

```
delete the object for number N  when  N ∉ live_files(all pinned versions)  and  N ∉ pending_outputs
```

The two sets are read in that order — live first, pending second — for the same reason the local
sweep reads them in that order (`db/compact.rs`): a file that becomes pending after the live set
was read is not yet in any version, and a file that stops being pending has already been named by
one. Reading them the other way round admits a file that is in neither.

A `DeleteObject` that fails is retried and, failing that, leaked. A leaked object costs storage;
a wrongly deleted one costs data. `esker-cli` gets an offline reconciler in a later phase — the
list-and-compare is cheap and the correct place for it is a tool, not the write path.

## Decision 6: a ranged GET must prove it read what it asked for

`GetObject` with a `Range` header returns `206 Partial Content` with a `Content-Range`. Two
things can go wrong quietly: a proxy or a misconfigured gateway can answer `200` with the whole
object, and a bucket whose key was overwritten can answer a range from *different bytes*.

Every ranged read checks:

- the status is `206`, and `Content-Range`'s first-byte position is the offset asked for;
- `Content-Length` equals the length asked for, unless the range ran past the end of the object,
  in which case it equals what remained;
- the `ETag` matches the one recorded when the object was uploaded, when we have it.

A gateway that answers `200` with the whole object has already *given* us the full-GET fallback,
so the client slices the window out of what arrived rather than asking a second time. Every
other mismatch — a `206` from the wrong offset, a short body that is not at the end of the
object, an `ETag` that is not the one recorded at upload — is an error, retried by the layer
above like any other failed read, and never quietly accepted (`CLAUDE.md` invariants 2 and 9).
The SST's own block CRCs are still checked underneath all of this; this check exists so that
"the object was replaced" reads as itself instead of as a checksum failure three layers up.

## Consequences

- The write path's latency and its crash-safety story are unchanged by tiering. The phase-1
  crash battery applies verbatim to a tiered database, and is run that way.
- A store whose bucket is unreachable degrades to a non-tiered store, then to a full disk. Both
  are visible in logs well before they are fatal.
- A database that never tiers writes byte-identical manifests to a phase-5 one, because the
  location tag is omitted when the location is `Local`. Every existing golden still passes,
  which is how we know the format change is additive.
- An old binary refuses to open a tiered database. Correct: it cannot fetch the files.
- We keep a retry queue and a governor in the engine, which is new machinery in a crate that had
  none. Both are `std`-only and driven by an explicit tick from the caller, so the simulator can
  still replay a run.
