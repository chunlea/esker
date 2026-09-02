# Debt wave, lane c4: the rest of the recorded non-SQL debts

Eight items from the inventory taken at `4334403` — #10, #9, #13, #12, #14, #15, #16, #17 — in the
order the brief gave them. Items #1–#8 were wave [c3](debt-c3.md). Each unit here is a repro or a
measurement first, then the fix, then the same repro green; nothing is documented instead of fixed.

## 1. The columnar copy was rebuilt by a full walk at every open

Inventory #10. `crates/esker-store/src/columnar/region.rs`, whose module header named the fix: *"the
manifest must carry the applied index its runs are complete to."*

### What was there

Opening a table's columnar target deleted its run manifest, which made every run an orphan for
`RunSet::open` to sweep, and refilled the target by walking the region's whole `write` column family
for that table's key range. Correct by construction — a crash loses an unsealed memtable and a short
run looks exactly like a complete one — and linear in the region at every open, where "every open"
means a restart, the first fragment to reach a table, and any commit after a failed one closed the
copy.

### The fix

[ADR 0038](../adr/0038-a-run-manifest-names-the-index-its-runs-are-complete-to.md). The manifest
takes one number, `applied`, at format version 2: the region apply index whose committed versions
are all in the runs that same manifest names. An open reads it and **replays the Raft log** from
there to the region's applied index, decoding each entry's `Command` and reading back what it
committed out of the `write` column family — the same input the apply-time tee gets, from the same
source of truth.

The four rules that make it safe are in the ADR. Two of them were found by the tests rather than
reasoned out in advance:

* **the index names an entry, never half of one.** The memtable budget can seal a run in the middle
  of an entry, and that run then holds part of an entry the manifest does not name. Sealing the
  rest of the entry at its end is what lets the manifest name it — without that, a resume replays an
  entry a run already partly holds and the copy carries those rows twice;
* **the decision is made at one engine snapshot.** The applied index comes from the region's state
  record read *at the same snapshot as the data*, because the apply path writes both in one batch
  and `ColumnarSlot::table` runs on the request thread while the driver applies. Read at two
  instants, the index can name an entry the walk did not cover.

A **version 1 manifest is read rather than refused**, and answers `applied = 0`, which is the value
that asks for the full walk. An upgrade pays one rebuild per table and never fails to open a copy.

### The test, and the two reds

`crates/esker-store/tests/columnar_resume.rs`, three tests, 0.2 s. The region's log is written by
`RaftLogStorage` itself — `stage_ready` for the entry, `stage_applied` in the same batch as the
data — because a resume that only worked against entries a test wrote by hand would prove nothing.

The observable is new: `ColumnarSlot::last_build` says what the last open read (`from_index`,
`to_index`, `versions`, `resumed`). It has to be published, because a resumed copy and a rebuilt one
are *the same files*; nothing on disk distinguishes them.

Red twice, each red isolating one half of the rule:

1. **without the resume** — `a_reopen_replays_only_what_arrived_after_the_manifest_says` fails with
   *"the reopen re-walked the region instead of resuming from its manifest"*, and the crash-ordering
   test reports 4 versions replayed where 3 were left in the buffer;
2. **with the manifest claiming the buffer** (`entry_applied` republishing unconditionally) — the
   crash-ordering test answers **1 row where 4 exist**:

   ```
   assertion `left == right` failed: the reopen answered without the versions the crash took
   from the buffer
     left: 1
    right: 4
   ```

   which is the shape this whole unit is written to make impossible: not a slow answer, a silently
   short one.

The third test compacts the log past what a copy names and asserts the open falls back to the full
walk and loses nothing.

## 2. One TCP connection per S3 request

Inventory #9. `crates/esker-s3/src/transport.rs`, whose own doc said *"plain HTTP over a fresh TCP
connection per request"*, and `docs/bench/phase-6b.md` §3, which measured what that costs and named
the fix without doing it.

### What was there, and why it was right once

The transport sent `Connection: close` and opened a socket per request. `http.rs` said why: *"one
connection per request means no state to get wrong between them, and an upload per flush does not
need pooling."* True for the uploader. The read path is a different shape — 6b §2 measured 200,016
ranged `GET`s for 200,000 cold reads, one round trip per read by design — so on that path the
handshake is paid per block.

### The fix

[ADR 0039](../adr/0039-a-kept-alive-s3-connection.md). Idle connections are kept, at most 16 per
endpoint, and a request says `Connection: keep-alive`. The three rules that bound it are in the ADR;
the one worth repeating here is the third, because it is where the temptation was. A pooled
connection the peer has closed is the ordinary case — every server has an idle timeout — and the
obvious answer is to retry the request on a fresh connection. **That answer is wrong**, and more
wrong now than before: ADR 0024 decision 2 puts retrying in `S3Client` because only it knows what is
idempotent, and unit 3 below adds `PutObject` with `If-None-Match: *`, a request whose silent replay
would answer `412` and hand a prefix to the wrong owner. So the transport does not retry; it
**checks before reusing**, with a non-blocking peek, and the rare loss surfaces as `Error::Io`,
which `is_retryable` already answers for.

### The test

`crates/esker-s3/tests/keep_alive.rs`, four tests, 0.06 s, against a server that counts what it
accepts — reuse is invisible in a response, so `TcpTransport::connections_opened` is the observable.
The three server shapes are the three that exist: one that keeps the connection, one that says
`Connection: close`, and one that keeps it and drops it anyway, which is an idle timeout and is the
case a pool gets wrong if it trusts what it holds.

Red with pooling disabled:

```
assertion `left == right` failed: ten requests opened 10 connections
  left: 10
 right: 1
```

Gated against the real container as well: `esker-engine`'s four `tier_minio` tests, green.

### The numbers

`docs/bench/debt-c4.md` §1, interleaved before and after so a loaded box cannot favour one side.
Cold `readrandom`, one thread — the configuration where the p50 *is* one round trip:

| | ops/s | p50 | p99 |
|---|---:|---:|---:|
| before (median of 3) | 1,071 | 887.3 µs | 2,247.6 µs |
| after (median of 3) | 2,311 | 411.4 µs | 763.8 µs |

**The cold-read p99 falls from 2,247.6 µs to 763.8 µs**, and the two distributions do not overlap:
the after runs' worst p99 is below the before runs' best. At four threads, 1,910 → 4,276 ops/s with
p50 1,580 → 716 µs; the p99 moves less there because what is left is `MinIO` saturating, which is
the same reading 6b §2 gave of the four-thread plateau.

## 2b. Two `tier_minio` tests passed exactly once per bucket

Found on the way to that measurement, at HEAD and before any change of this lane's:
`a_database_that_lost_its_ssts_rebuilds_from_the_bucket` and
`an_outage_leaves_the_sst_local_and_the_retry_lands` both failed on the first upload, `left: 0,
right: 1`.

The bucket still held `engine/acceptance/000004.sst` and `engine/outage/000004.sst` from a run six
weeks earlier. A fresh database numbers its first SST `000004` whatever has ever existed, and
`TieredFileSystem::new` **adopts what the bucket already holds** — so the tier found `000004`
uploaded already, skipped it, and drained nothing. `unique_prefix` was already in the file, used by
the claim tests only; the reason it exists is wider than the claim, and now says so.

## 3. The SST-store claim race was narrowed, not closed

Inventory #13. [ADR 0029](../adr/0029-the-sst-store-claim.md) §Consequences, which said the window
would close *"when we next touch `esker-s3`"* — and unit 2 is that touch.

### What was there

Two databases claiming an empty prefix in the same instant both wrote the marker and both read it
back, so whichever wrote *second* was the one both then agreed with: the first could read a
confirmation of its own claim while the marker named the other. The read-back narrowed the window
from the lifetime of a database to two overlapping round trips. It did not remove it.

### The fix

`ObjectStore::put_if_absent` — `PutObject` with `If-None-Match: *` — and the marker is written with
it. Exactly one conditional put creates the object; the other is told `AlreadyThere`. `S3Client`
maps `412` and `409` (S3's answer to two overlapping conditional writes) to that outcome.

Three things deliberately did **not** change:

* **the read-back stays**, and now does two jobs. It is what turns "already there" into an answer,
  because the outcome alone cannot distinguish *somebody else has it* from *this is my own claim,
  retried* — and a claim whose response was lost must recognise its own marker rather than refuse
  it. It is also the whole safety story on an endpoint that ignores the precondition, where the
  conditional put degrades to the unconditional one and the window is the narrowed one above. Never
  wider, never silent;
* the transport still does not retry, which is what makes a non-idempotent conditional put safe to
  send at all (unit 2);
* `MemoryStore`'s conditional put checks and inserts **under one lock**, so the fake is as atomic as
  the real thing. A fake that checked and then inserted would pass a test the real store would fail.

### The tests

* `crates/esker-engine/tests/tier_claim.rs::two_databases_claiming_at_once_leave_exactly_one_winner`
  — two threads, one empty prefix, 32 attempts; exactly one wins and the loser's message names both
  identities. And `a_retried_claim_recognises_its_own_marker`, which is the case the outcome alone
  cannot answer;
* `crates/esker-engine/tests/tier_minio.rs::two_databases_claiming_at_once_over_http_leave_one_winner`
  — the same race over real HTTP;
* `a_conditional_put_is_refused_by_the_real_endpoint` — the endpoint really enforces
  `If-None-Match`. This is the assumption the closed race rests on, and without a test for it an
  endpoint that stopped enforcing the header would quietly return the claim to the narrow window
  with nothing red anywhere.

ADR 0029's "the race we do not win" consequence is rewritten to say what closed it and what did not
change.

## 4. Leaked objects after a failed `DeleteObject`

Inventory #12. `esker sst-store reconcile s3://bucket/prefix --data-dir DIR [--delete]`.

A `DeleteObject` that fails leaks its object on purpose (ADR 0024): a leaked object costs storage
and a wrongly deleted one costs data. That is right on the write path and it leaves somebody to
clean up.

**It is a tool and not a background task for a reason.** The bucket's listing and the database's
manifest are read at two instants, and a *running* database moves the manifest between them — so an
object that looks unreferenced may be one a compaction uploaded a moment ago and is about to name.
On the write path that is a race with a data-loss ending; offline it is not a race at all.

Three gates in front of a delete, and the third was not in the brief:

1. **the prefix must carry this database's own claim marker.** ADR 0029's marker answers exactly the
   question this tool must not get wrong, and a prefix with no marker is refused as well — it might
   be somebody's, which is the same reasoning `--adopt-sst-store` exists for. There is deliberately
   no such hatch here: adopting a prefix *in order to delete from it* is not a thing to make easy;
2. **dry run by default**, and what `--delete` removes is what the dry run printed;
3. **nothing at or above the manifest's next file number is ever deleted**, `--delete` or not. Such
   an object is one the manifest has not caught up with, and "the manifest is behind" is not a
   reason to delete data. A torn manifest tail is refused outright for the same reason: a truncated
   manifest names *fewer* files than the database does, and this tool would call the difference
   garbage.

Anything that is neither an SST nor the marker is reported and never touched.

`crates/esker-cli/src/reconcile.rs`'s tests, five of them, against `MemoryStore` and a real
database on a real tier: a planted orphan is reported and then deleted; an object newer than the
manifest is kept even under `--delete`; another database's prefix is refused with both identities in
the message; an unclaimed prefix is refused; a clean prefix says so rather than printing nothing.
