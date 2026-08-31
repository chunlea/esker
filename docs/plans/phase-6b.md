# Phase 6b — SST tiering to object storage, and the scale-to-zero hooks

Milestone: `prompts/06-sql-serverless.md` §6b. Design hooks: `docs/DESIGN.md` §13.
Lane `wy-p6b-tier`; frozen for this lane: `esker-raft`, `esker-store`, `esker-txn`,
`esker-sql`, `esker-client` — anything needed there is reported, not edited.

## What this phase is

An SST is immutable the moment it is finished. That is the whole reason tiering is possible
at all: a file that never changes can be copied to object storage once and read from there
forever, and the only hard question is *when a local copy may be thrown away*. Everything
below is an answer to that question.

The WAL and the Raft log stay local, always. They are the log, and `CLAUDE.md` invariant 1
says a write is acknowledged when its log bytes are durable — putting a network between the
writer and that fsync would either break the invariant or make every write cost a round trip.
**S3 is a tier, not the log.**

## Scope

In:

1. SHA-256 and HMAC-SHA-256 in `esker-base`, golden-tested against published vectors.
2. AWS SigV4 request signing, golden-tested against the AWS test suite.
3. A minimal S3 client — `PutObject`, ranged `GetObject`, `ListObjectsV2`, `DeleteObject` —
   over an in-house HTTP/1.1 codec, in a new crate `esker-s3` with no new dependencies.
4. `FileLocation` on `FileMeta`, carried by a new manifest tag; golden-tested.
5. `TieredFileSystem` in `esker-engine`: SSTs written locally, uploaded after they are
   durable, read back through a local disk cache under a byte budget.
6. `--sst-store s3://bucket/prefix` parsed and plumbed as far as the frozen boundary.
7. MinIO integration tests behind `--ignored`, with the recipe recorded.
8. `readrandom` with a cold cache benched into `docs/bench/phase-6b.md`.

Out — named so nobody looks for them:

- **Region hibernation** (§6b item 2) is ADR-and-design only, and only if the rest lands.
- **TLS.** ADR 0025 settles the choice and the first milestone is plain HTTP to MinIO. No
  `rustls`, no exception to the pure-Rust rule, in this phase.
- **Tiering the WAL or the Raft log.** Never, per the above.
- **Multipart upload.** SSTs default to 8 MiB (`target_file_size`); a single `PutObject`
  covers that with room to spare. A 5 GiB file would need multipart, and the client returns
  a clear error rather than silently truncating.
- **Server-side encryption, versioning, lifecycle rules.** Bucket configuration, not ours.
- **A second local cache directory.** The database directory *is* the cache; see below.

## The shape

```
esker-base           sha256, hmac        (golden vectors)
esker-s3             ObjectStore trait   put / get_range / list / delete
                     sigv4               (golden vectors)
                     http                request writer + response parser (fuzzed)
                     transport           Transport trait + blocking std::net impl
esker-engine  fs/    mod.rs              FileSystem, LocalFileSystem   (moved from fs.rs)
                     tier.rs             TieredFileSystem, the upload queue, the governor
```

`esker-s3` is a leaf crate: `esker-base` and `thiserror`, nothing else. `esker-engine`
depends on it for the `ObjectStore` trait. That direction is deliberate — the trait's
contract (a ranged GET whose length and ETag must agree with what was asked for) is an
object-store concept, not an engine one, and inverting it would make the wire crate depend
on the engine.

**No tokio.** `CLAUDE.md` puts async only at the network edge and keeps the engine
synchronous and `std`-only; an uploader driven from the flush path must therefore block.
`esker-s3::Transport` is a trait with a `std::net::TcpStream` implementation, which is what
`docs/DESIGN.md` §13 asks for ("design so the transport is a trait and the choice is local to
one module") and costs zero dependencies. A tokio transport can be added later by a crate
that already has tokio; the engine will not be it. *(This departs from the lane brief's
"tokio TCP" — recorded here rather than done quietly, because the constitution outranks the
brief and the trait keeps the door open.)*

## The read path, and why the database directory is the cache

`TieredFileSystem` routes by file kind. `filename::classify_path` already tells it which
paths are SSTs; every other kind — WAL, MANIFEST, CURRENT, tmp — goes straight to the local
filesystem, untouched.

For an SST:

```
open(NNNNNN.sst)
  local file present?  -> open it                                  (the hot path)
  else                 -> fetch the object into that same path,    (a cold read)
                          then open it
  else                 -> NotFound, exactly as a local engine would report
```

There is no second cache directory and no rename dance. **Evicting a tiered SST is deleting
the local file; refilling it is fetching it back to the same path.** The engine is not told
which of its files are resident, because it does not have to care: every path it asks for
either opens or reports an honest error, which is the contract `FileSystem` already had.

That choice has one consequence worth stating: the obsolete-file sweep lists the directory,
so an evicted file is invisible to it and its *object* would never be reclaimed. Object
deletion is therefore driven from the version set's live-file set instead of from the
listing — `tier.retain(&live, &pending)` — which is the phase-1 pending-register discipline
extended by one set. An object is deleted only when its number is named by no live version
and is not a pending output.

Blocks are cached by `(file_number, offset)` in front of all of this, unchanged. A tiered
SST that is hot in the block cache costs no I/O at all, local or remote.

## Writes: local first, always

```
build SST locally -> sync_data -> manifest edit names it as Local -> ack
                                                                  \
                                                                   -> upload queue
                                                                        \
                                                                         -> on success,
                                                                            promote to Tiered
```

The upload happens **after** the file is durable locally and after the edit that names it.
Two reasons, and they are the whole of ADR 0024:

- Invariant 1 is about local durability. A flush that waited on S3 would put a network on the
  write path's tail latency, and a compaction that waited on S3 would stall L0.
- An upload that fails must not fail anything else. The file stays local, stays readable,
  stays correct, and is retried. The only cost of a permanently failing tier is disk.

`location` is a **record of what has already happened, never a promise the read path
depends on**. A file recorded `Local` that is in fact uploaded still reads (the local copy
is there). A file recorded `Tiered` that is missing from the bucket is an error — the same
error a missing local file gives. Nothing branches on the field to decide *whether* to look
somewhere; it exists so the governor knows what is safe to evict, so the sweep knows what to
delete remotely, and so an operator can see where the bytes are.

## The manifest format change

`FileMeta` gains `location: FileLocation` (`Local` | `Tiered`). On the wire it is a new
optional edit tag:

```
tag 9  FILE_LOCATION = cf:varint ++ level:varint ++ number:varint ++ location:varint
```

Emitted **only when the location is not `Local`**, so a database that never tiers produces
byte-identical manifests and every existing golden still passes. A new reader reading an old
manifest sees no tag and defaults to `Local`, which is true. An old reader reading a tiered
manifest rejects it — `VersionEdit::decode` treats an unknown tag as corruption on purpose
(`version/edit.rs`), and that is the correct outcome: a binary that cannot fetch from S3 must
not open a database whose files are there.

A separate tag rather than a field appended to `ADD_FILE` because promotion happens *after*
the add, and a promote edit should cost four varints, not a repeat of the file's whole
metadata.

## Failure semantics — ADR 0024

| what fails | what happens |
|---|---|
| upload | SST stays local, stays `Local` in the manifest, retried with backoff |
| upload, permanently | disk fills; the governor cannot evict; **backpressure**, not data loss |
| ranged GET, length or ETag mismatch | fall back to a full GET; a second mismatch is corruption |
| GET, object missing, file `Tiered` | error, same class as a missing local file |
| local disk full | write stall, as today — the tier is not a relief valve for the WAL |
| MinIO gone mid-upload | the SST is local and complete; the retry queue drains when it returns |

## Unit list, in commit order

1. **This plan, ADR 0024 (failure semantics), ADR 0025 (transport and TLS).** No code.
2. **`esker-base::sha256`, `esker-base::hmac`.** FIPS 180-4 / RFC 4231 vectors, a
   length-extension-shaped property test, and the empty-input case that catches a padding
   bug.
3. **`esker-s3` — SigV4.** Canonical request, string to sign, signing key, `Authorization`
   header, against the AWS `aws-sig-v4-test-suite` vectors. Provenance of every vector
   recorded in the test module; any hand-derived vector says so and says from what.
4. **`esker-s3` — HTTP/1.1 and the four calls.** Response parser fuzzed against truncation
   and adversarial headers: chunked encoding, absurd `Content-Length`, header without a
   colon, status line that is not a status line. Never panics; every malformed byte is an
   error value.
5. **`esker-engine` — `FileLocation` and the manifest tag,** with a golden test for both the
   untiered (unchanged bytes) and tiered cases.
6. **`esker-engine` — `fs/tier.rs`.** `TieredFileSystem`, the upload queue and its retry, the
   byte-budget governor, `retain` for object deletion. Unit-tested against an in-memory
   `ObjectStore` so the tests need no container.
7. **MinIO integration** behind `#[ignore]`: upload-on-flush, ranged reads, cache-hit
   accounting, kill-MinIO-mid-upload, eviction and refill. Recipe in the plan and in the
   test module.
8. **`--sst-store`** parsed in `esker-cli`, plumbed to the frozen boundary; the one-line
   change `esker-store` needs is reported, not made.
9. **Acceptance and bench**: delete the local SSTs, reopen, read everything back;
   `readrandom` cold into `docs/bench/phase-6b.md` with p99 and the hit rate.

## Tests

- Golden: SHA-256, HMAC, SigV4, the manifest tag (tiered and untiered).
- Property: HTTP response parser never panics on arbitrary bytes; the governor never evicts a
  file that is not uploaded; `retain` never deletes an object a live version names.
- Fault: upload fails at every step (the `FaultFs` discipline, applied to a failing
  `ObjectStore`); MinIO killed mid-upload.
- Crash: kill during the window between the manifest edit and a successful upload — the file
  must be local and readable on reopen.
- Acceptance: the local SST files are deleted with the database closed; it reopens and reads
  every key.

## Risks

- **Two databases sharing one prefix** silently overwrite each other's `000007.sst`. Guarded
  by a marker object holding a database id, checked on open; a mismatch refuses to start.
  Sequenced last so it can be dropped without stranding anything.
- **`fs.rs` → `fs/mod.rs`** is a move other lanes may be editing around. Done as its own
  commit, content-identical, so a conflict is trivial to resolve.
- **The dep budget** (`deny.toml`, 40): `esker-s3` adds no external crate. Verified by
  `dep_budget.rs`, not by assertion.
- **Machine contention**: MinIO is a container and the bench is heavy; both are scheduled
  with the coordinator rather than run whenever they are ready.

## What this lane will not do

Touch `esker-raft`, `esker-store`, `esker-txn`, `esker-sql` or `esker-client`. Add any
dependency. Implement TLS. Tier the WAL. Change the SST format itself — the bytes of an SST
are the same whether it lives on disk or in a bucket, which is the property that makes all of
this cheap.

---

## What was built, and where it differs from the plan above

Written after the fact, per `CLAUDE.md`'s definition of done. The plan above is left as it was
written; this section is the diff.

### Delivered

| Unit | State |
|---|---|
| 1. Plan, ADR 0024 (failure semantics), ADR 0025 (transport and TLS) | done — `d68f3d9` |
| 2. SHA-256 + HMAC in `esker-base`, against FIPS 180-4 and RFC 4231 | done — `527bec1` |
| 3. SigV4 + HTTP/1.1 + the four S3 calls, in `esker-s3` | done — `a12166d` |
| 4. `FileLocation`, the manifest record, and `fs::tier::TieredFileSystem` | done — `720abf0` |
| 5. `MinIO` integration: `esker-s3`'s four calls, and the engine's tier end to end | done — `a12166d`, `720abf0` |
| 6. `bench --sst-store`, and `docs/bench/phase-6b.md` | done — `00b737f` |
| 6. `esker server --sst-store` and the store-level acceptance | done — `843c8bf`, after `StoreOptions::fs` landed in `9dd71b1` |
| Region hibernation (§6b item 2) | **not done.** It was "ADR and design only, unless time allows"; it did not. Nothing depends on it |

### Departures from the plan

- **Blocking `std::net`, not tokio.** The lane brief said tokio TCP. `CLAUDE.md` puts async at
  the network edge and keeps the engine synchronous and `std`-only, and the uploader runs on
  the engine's side of that line. `esker_s3::Transport` is a trait with a blocking
  implementation, which is what `DESIGN.md` §13 asked for and costs zero dependencies. ADR 0025
  decision 2 records the reasoning; flagged in the lane report rather than done quietly.
- **The backoff is one attempt per maintenance pass**, not exponential with jitter. ADR 0024's
  first draft promised the latter; the implementation makes a pass take *distinct* files, once
  each, which bounds the retry rate by the uploader's cadence without a clock the simulator
  would have to fake. The ADR was corrected to describe the code (`720abf0`).
- **A `200` answer to a ranged `GET` is sliced locally** rather than re-fetched. It *is* the
  full-GET fallback, already paid for. ADR 0024 decision 6 was corrected to match.
- **No prefix-collision marker.** The plan listed a `TIER-ID` marker object guarding against two
  databases sharing one prefix, sequenced last so it could be dropped. It was dropped. Two
  databases sharing an `--sst-store` prefix would overwrite each other's `000007.sst`
  **silently** — the sharpest edge this lane left behind.
  **Paid** in the 6c debt wave: `esker_engine::fs::claim`, [ADR
  0029](../adr/0029-the-sst-store-claim.md), `docs/plans/debt-c1.md`. Identity turned out not to
  be `(cluster_id, store_id)` — two benchmarks have neither — but a random id kept in the
  database's own directory.

### Debt, in the order it should be paid

1. **Connection reuse in `esker_s3::transport`.** One TCP connection per request costs ~692 µs
   per cold read on loopback and dominates the cold-cache p99 (`docs/bench/phase-6b.md` §3).
   Local to one implementor of one trait.
2. ~~**The prefix-collision marker**, above.~~ Done; see the note above.
3. **TLS** — ADR 0025 §"what has to be true before TLS lands" is the checklist.
4. **An offline object reconciler** in `esker-cli`: a `DeleteObject` that fails leaks the
   object, deliberately, because a leaked object costs storage and a wrongly deleted one costs
   data. List-and-compare is cheap and belongs in a tool, not on the write path.
5. **A tiered-read block size.** 4 KiB is tuned for a local SSD; a tiered SST pays a round trip
   per block. Measure before adding the knob.
6. **The rest of the engine's knobs on `esker server`.** `--write-buffer-size` landed here
   because the acceptance needed it; `docs/bench/phase-4.md` wanted `region_split_size` and the
   heartbeat intervals for the same reason and had to wrap the binary instead.

### The acceptance, and the bug it found

`crates/esker-cli/tests/tier_acceptance.rs` runs the scenario the phase asks for against three
store processes and a real `MinIO`: node 3 uploads its SSTs, is killed, misses a phase of
writes, loses all six SSTs, and comes back holding all 2,400 keys. All six deleted files
reappear under the same numbers, which is what distinguishes recovery from object storage from
recovery by Raft snapshot. `docs/bench/phase-6b.md` §5 has the numbers.

**The first version of it proved nothing, and that is the most useful thing this lane found.**
Deleting the SSTs lost zero keys with or without a tier, because an idle column family was
pinning the write-ahead log: `oldest_log` answered a stale `active_log` for a family with an
empty memtable, so `esker-store`'s idle `lock` and `write` families held every segment the
database had ever written — 41 surviving 40 flushes. Every write was still in the log, recovery
replayed the whole history, and the SSTs were decoration. Fixed in `b790f50`; with the fix the
untiered control loses 1,617 of 2,000 reads, which is what makes the tiered run mean something.

A test that cannot tell a working tier from a broken one is a test that will pass on the day the
tier breaks. This one now fails without either half.

### Two harness lessons, for whoever writes the next multi-process test

- **Stop the supervisor with `SIGINT`, not `SIGKILL`.** `cluster start` stops its children on
  ctrl-C; killing it outright orphans three stores that then sit on their ports until the *next*
  test fails to start a cluster, for reasons that have nothing to do with it.
- **Keep the supervisor's output.** "The cluster did not start" is not a diagnosis, and the log
  is the only thing that says why.
