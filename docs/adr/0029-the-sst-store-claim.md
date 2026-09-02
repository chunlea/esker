# 0029 — a prefix proves whose it is

Status: accepted. Phase 6c, closing the sharpest edge phase 6b left
(`docs/plans/phase-6b.md`, "No prefix-collision marker").

## Context

`--sst-store s3://bucket/prefix` tiers a database's SSTs into object storage. The object key is
derived from the file number — `prefix/000007.sst` — and **file numbers restart at one in every
database**. Two databases pointed at one prefix therefore overwrite each other's objects.

Nothing about that is loud. Every `PutObject` succeeds, every checksum matches, every manifest is
consistent with itself. The loser discovers it when a read returns another database's block, at
which point both databases have been wrong for as long as they have been running.

Phase 6b shipped `sst_store::for_node`, which derives `prefix/node-N` for `esker cluster start`,
and recorded the marker as debt. Derivation is a convenience: it helps the operator who uses the
cluster command and does nothing at all for the one who runs two `esker server`s by hand, or two
benchmarks, or who restores a backup into a directory beside the original.

## Decision

**1. A prefix is claimed.** The first database to open one writes `ESKER-CLAIM` under it; every
later open reads that object and refuses a prefix that belongs to another database. The check
happens in `TieredFileSystem::new`, before the bucket is listed, so a database that is going to
be refused never learns another database's file numbers.

**2. Identity is a random id in the database's own directory**, not `(cluster_id, store_id)`.

The obvious identity cannot do the job. Two `esker bench` runs have neither id; two stores
misconfigured with the same store id are exactly the collision worth catching. So the authority
is eight random bytes drawn once, when a database first claims any prefix, and kept in
`ESKER-CLAIM-ID` in its own directory. Same directory, same claim; different database, different
claim, whatever the flags say.

The cluster and store ids go into the marker anyway, because they are informational in the way
that matters: an operator who has to fix a collision needs to be told *which two databases*
collided, and two random numbers tell them nothing.

`ESKER-CLAIM-ID` is written and fsynced with its directory before any claim is made, so a crash
can leave an id with no marker — harmless, the next open claims — but never a marker with no id,
which would be a database that cannot recognise its own prefix.

**3. The format is versioned, fixed-width and checksummed.** Thirty-seven bytes:

```text
0   8  magic "ESKERCLM"
8   1  format version (1)
9   8  claim id
17  8  cluster id      informational
25  8  store id        informational
33  4  CRC32C of bytes 0..33
```

Fixed-width so there is no length field to disagree with a buffer: a truncated or padded object
is caught by its length before anything is parsed out of it. The CRC is checked *before* the
version, so a corrupt version byte reads as corruption rather than as an unreadable future
format. A version this build does not know is refused as a version, so the message sends the
operator to an upgrade rather than to a checksum hunt. Every one of these is an error value;
none is a panic (`CLAUDE.md` invariants 2 and 9). The bytes have a golden test.

**4. A prefix with objects and no marker is refused, and the hatch is explicit.** Such a prefix
is either pre-6c or has had its marker deleted, and from outside there is no way to tell either
from a prefix a live database is still using. `esker server --adopt-sst-store` says "I have
checked, it is mine", logs loudly, and writes the marker. Nothing adopts silently, ever.

**5. A marker that is unreadable refuses the open.** It is not overruled and not overwritten,
including under `--adopt-sst-store`: the hatch is for *no* marker, not for one that will not
parse, because one that will not parse might still be somebody's.

## Consequences

**The race we do not win — closed in wave C4, by the change this said to wait for.** As shipped,
two databases claiming an empty prefix in the same instant could not be separated by a
`PutObject`: the claim was **read back** after it was written, so the loser of a simultaneous claim
read the winner's marker and refused, which narrowed the window from the lifetime of a database to
two overlapping round trips. This paragraph said closing it wanted `PutObject` with
`If-None-Match: *` and was worth doing *when we next touch `esker-s3`*.

That happened: [ADR 0039](0039-a-kept-alive-s3-connection.md). So the marker is now written with
`ObjectStore::put_if_absent`, and the window is gone — exactly one conditional put creates the
object and the other is told the key exists. Three notes on what did **not** change:

* it is a **header on an existing call**, not the fifth S3 call this paragraph feared. `S3Client`
  maps `412` and `409` to `PutOutcome::AlreadyThere` and nothing else moves;
* **the read-back stays**, and now does two jobs. It is what turns "already there" into an answer —
  the marker names its owner, so a *retried* claim recognises its own marker instead of refusing
  it, which the outcome alone cannot say — and it is the whole safety story on an endpoint that
  ignores the precondition, where the conditional put degrades to the unconditional one and the
  window is the narrowed one described above. Never wider, never silent;
* the compatibility question is answered by a test rather than by assumption:
  `a_conditional_put_is_refused_by_the_real_endpoint` asserts that the endpoint under test really
  enforces `If-None-Match`, so an endpoint that stopped doing so is a red test rather than a quiet
  return to the narrow window.

**Losing `ESKER-CLAIM-ID` costs two deliberate steps.** A database whose id file is gone draws a
new one and is refused by its own prefix. That is the safe way to be wrong — the alternative is a
database that adopts a prefix on the strength of a file anybody could delete. The way back is to
delete the marker object and reopen with `--adopt-sst-store`, which is exactly the hatch for a
prefix that holds objects and no marker. Two deliberate steps, and no silent anything.

**One extra `GetObject` per open**, plus a `PutObject` and a second `GetObject` the first time.
Against the cost of opening a database this does not register.

**`for_node` stays.** Derivation and the claim answer different questions: derivation stops an
operator making the mistake, the claim stops the mistake being silent. Deleting either would be
worse than keeping both.

**Existing prefixes.** Every 6b prefix has objects and no marker, so every one of them refuses on
first 6c open with a message naming `--adopt-sst-store`. That is the intended upgrade path: it
is one flag, once, and it makes an operator confirm that the prefix is what they think it is.
