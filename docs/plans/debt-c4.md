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

## 5. In-flight operators were invisible to `esker pd inspect`

Inventory #14. `docs/plans/phase-4-pd.md` §12.3 "Not built, deliberately", bullet 3.

`pd inspect` opens a **stopped** PD's database, and the in-flight set is deliberately not in it:
[ADR 0013](../adr/0013-repair-operators-are-requests-not-commands.md) makes an operator a request
rather than a command, and a restart forgets every one and re-derives what is needed from the next
round of heartbeats. So "what is PD moving right now" was answerable only from PD's own log lines,
on a process an operator may not be able to attach to.

`Pd::Status` (method `0x0309`) answers it: PD's clock, and every operator in flight with its
progress, when it was issued, when progress was last observed, and how many times it has been sent.
`esker pd status --pd HOST:PORT` prints it. `inspect` and `status` are complements — one says what
PD believes about the cluster, the other what it is doing about it.

Four decisions in it are worth the words:

* **the clock travels with the set, taken under the same lock.** An age computed from two reads can
  be negative, and `CLAUDE.md` invariant 6 is the same instinct: one clock, PD's;
* **`since_ms` is reported beside `issued_ms`**, because the timeout runs from the last observed
  progress and not from the issue. A minute old and moving is not a minute old and stuck, and a
  report that showed only the age would make the two look identical;
* **the region id is not a field.** It is `Operator::region_id()`, and duplicating it would make a
  disagreement between the two expressible;
* **`Status` is exempt from the cluster-id gate**, alone with `Bootstrap`. It reads no
  cluster-scoped state, and an un-bootstrapped PD is exactly when an operator most wants to ask —
  answering "the cluster is not bootstrapped" would be a reply to a question nobody asked. A test
  asserts the gate is still on everything else, so the exemption is a decision rather than a hole.

Golden bytes for both the request and the response, **derived independently** in the file's own
terms (tag LE, LEB128 varints) rather than dumped from the encoder, which is the only way a golden
pins anything. The response golden carries two operators of different kinds and different progress:
one of anything pins neither the repeat nor the tag it repeats.

`crates/esker-pd/tests/loopback.rs` over a real socket: a repair in flight is visible from outside
the process with the right progress and send count, asking twice does not change it, a quiet PD says
so, and a PD with no cluster still answers. `crates/esker-cli/src/pd.rs`'s tests pin the report's
shape, including that an age from a timestamp in the future saturates instead of wrapping to
nineteen billion seconds and reading as a hung operator.

The reverse-dependent gate earned its keep immediately: a new `PdReq` variant broke an exhaustive
match in `crates/esker-store/tests/pd_client.rs`, a crate this unit does not otherwise touch.

## 6. `region ls` walked the key space one `GetRegion` at a time

Inventory #15. `docs/plans/phase-4.md` §14.6, bullet 3.

`GetRegion` is the routing question a *client* asks: one key, one region. `esker region ls` had
nothing else to ask, so it walked — ask for `""`, take the region's end key, ask again — which is
correct and is `O(regions)` round trips. Measured, with the walk restored under the new test:
**61 `GetRegion` calls for 60 regions**.

`Pd::ScanRegions` (method `0x030a`) answers a page of the routing table in **key** order.

* **Paged, not whole.** A cluster's region count grows with its data, and a single frame carrying
  all of them is a message whose size nobody chose. `limit` of zero means the server's default
  (128) and the server caps it at 1024 either way: a caller's limit is a request, never an
  instruction. Fewer regions than the limit means the end of the table, so no separate flag says
  when to stop.
* **The store table is sent once per page**, deduplicated, rather than per region. `GetRegion`
  answers about one region and has nothing to deduplicate against, which is why the two response
  shapes differ rather than one being reused.
* **The scan starts at the region *containing* `start_key`**, not the one after it, so continuing
  from the previous page's end key lands on the region that starts there. The key-ordered range
  index — the one `lookup` already seeks into — is what makes that a seek and a walk rather than a
  sort of the id-ordered records.
* **`ScanRegions` is not exempt from the cluster-id gate**, unlike `Status`: it reads the routing
  table, which is cluster-scoped, and a scan addressed to another cluster is the same mistake
  `GetRegion` is checked for.

**The walk's contiguity check survives the change, and that is the part worth keeping.** The old
shape got it for free — a gap answered "no region" — while a page of records has to check it
explicitly. `region ls` now checks that each region starts exactly where the previous one ended,
**across page boundaries as well as within one**, and reports a gap or an overlap rather than
rendering it tidily.

Tests: `crates/esker-pd/tests/loopback.rs::fifty_regions_come_back_in_key_order_a_page_at_a_time`
over a real socket — fifty regions in one page, the same fifty seven at a time with the pages
matching one call exactly, a limit above the cap clamped rather than refused, and a scan from the
middle of a key starting at the region that contains it. `crates/esker-cli/src/region.rs`'s
`ls_pages_the_whole_routing_table_in_key_order` lists sixty regions and asserts the **round-trip
count**, which is the only place the change is visible: 2 calls including `PdConn`'s documented
cluster-id discovery, and 1 for the second listing on the same connection — against 61 before.
`TestPd` grew a counting service for it, because a claim about round trips that no test can see
stops being true the moment somebody reintroduces a loop.

Golden bytes for the request and the response, derived independently again. The response golden
carries two regions that meet and one store shared by both, so it pins the contiguity the caller
checks and the deduplication the shape exists for.

## 7. `esker server` lacked `region_split_size` and the heartbeat intervals

Inventory #16. `docs/plans/phase-6b.md` debt list, item 6; the complaint itself is in
`docs/bench/phase-4.md`'s method table, which records that the phase-4 lane had to run a scratchpad
wrapper — *"`esker-cli`'s own `server.rs`/`pd.rs` with those `StoreOptions`/`PdOptions` fields
exposed as flags, otherwise identical"* — to get a meaningful region count without minutes of
writing at the 96 MiB default.

Four plain flags: `--region-split-size`, `--store-heartbeat-ms`, `--region-heartbeat-ms`,
`--heartbeat-tick-ms`. Each refuses zero, because zero is a typo rather than "the default": a split
size of zero asks a leader to split every region for ever, and a heartbeat interval of zero beats
on every tick. Absent means the store's own default, so a flag's absence is not a different
configuration from not having the flag.

`--heartbeat-tick-ms` is in there because the other two are counted **in ticks**
(`esker_store::Heartbeats`), so an interval below one tick is rounded up to one and shortening an
interval without also shortening the tick does nothing. The help text says so, because a knob that
silently does nothing is worse than a missing one.

`store_options` is a function now rather than an expression inside `run`, and that is what the test
holds on to: each of these parses fine and does nothing at all if it is dropped on the way through
to `StoreOptions`, which is a failure no parse test can see. Two tests, one for the knobs reaching
the store and one for the defaults standing without them, plus the parse test in both spellings
(`--flag value` and `--flag=value` are two code paths) and a zero and a word refused for each.

`docs/bench/phase-4.md`'s two wrapper notes are updated to say which half is closed. **The store
half is; the PD half is not** — `pd serve` still exposes no `PdOptions` field, and that run set
`max_store_down_time_ms` among others, so `pdwrap` is still needed and the note says so rather than
claiming a fix that does not exist.

## 8. `esker bench` had no `--adopt-sst-store`

Inventory #17. `docs/plans/debt-c1.md` §"What this lane did not do", bullet 2.

`bench` passed `Claim::default()`, so `adopt` was false and a prefix holding objects with no marker
was **refused** — with a message ending *"re-run with `--adopt-sst-store` to claim it"*, on a
command that had no such flag. A dead end with instructions on it.

It matters more for `bench` than for `server`, and for a reason `server` does not have: a
benchmark's database is a **temporary directory**, so its claim id
([ADR 0029](../adr/0029-the-sst-store-claim.md) decision 2 puts the id in the database's own
directory) is new on every run. Every re-run against a hand-named prefix therefore meets objects it
did not write, and every one of them hit that message.

The flag is the same switch `server` has, off by default and for the same reason: a benchmark
pointed at a stale prefix should get a fresh one. What changed is that the run which *did* mean to
reuse what it can see now has the way to say so.

Tested at both layers. The parse test asserts the default is off and both spellings set it.
`crates/esker-cli/tests/tier_acceptance.rs::a_bench_is_refused_by_an_unclaimed_prefix_and_adopts_it_when_asked`
drives the real binary against the container through all three states — a first run that claims the
prefix, the marker removed and an object left behind, a second run refused with the flag named in
its message, and the same run succeeding with the flag. Red with the flag parsed but not passed
through to `Claim`:

```
the hatch did not open: ... re-run with --adopt-sst-store to claim it
```

which is the failure this unit is named for, reproduced by the one-line regression that would cause
it.

---

## 9. Balance preempted an unfinished repair, and the two then waited for each other

Not an inventory item — a red gate. `esker-store`'s
`promotion.rs::a_learner_on_a_fresh_store_becomes_a_voter_under_load` failed in this wave's closing
`just check` (2,383 of 2,384) and was relayed by the type lane as failing reproducibly in isolation,
with *"server is busy: leadership transfer to 40 is in progress"* and then *"region epoch does not
match"*.

### What the trace shows

Run with `RUST_LOG=esker_store=debug,esker_pd=debug` the test fails **8 of 8** — the logging widens
the window enough to make the race deterministic, which is what turned a flaky test into a
reproduction. The sequence, for region 7:

```
02:56:50.876  an operator applied  region_id=7 node=24 kind=AddLearner
02:56:51.035  a learner has caught up; promoting it to voter region_id=7 learner=24 matched=61
02:56:54.187  operator issued  region_id=7 operator="TransferLeader"     <- balance, learner still a learner
02:56:54.268  leadership was asked to move region_id=7 to_peer_id=8
02:57:00.046  operator issued  region_id=7 operator="TransferLeader"     <- and again
```

and for region 5, `operator timed out with nothing observed; it will be re-derived ... sends=3`.

**The promotion is proposed by the leader**, on the learner's `matched`, because a region heartbeat
comes only from a leader and PD can therefore never see a learner's progress
(`docs/plans/phase-4.md` §14.1). A leader with a leadership transfer in progress **refuses
proposals**. So a `TransferLeader` against a region whose learner has not been promoted blocks the
very `AddVoter` that would finish the repair; the transfer times out waiting for a target the
promotion would have caught up; PD re-derives it; and the region stays that way past the 30-second
acceptance deadline.

### The fix

`balance::is_mid_repair`. A region is mid-repair while it holds a peer on a **down store** *or* a
plain **`Learner`** — and both `region_balance` and `leader_balance` consult it. `leader_balance`
previously had no repair guard at all beyond "the leader's store is down".

`mid_repair` had counted a peer on a down store and nothing else, so a replica added on a *live*
store and not yet promoted looked like a settled region. It is the same family this module already
names twice — "**Voters, not peers** ... the third instance", "the fourth instance, found by reading
the second half of this function while fixing the first" — reaching the state repair passes
*through* rather than the one it starts from.

A **`ColumnarLearner` is excluded**: ADR 0022 Decision 1 says it is never promoted, so counting it
would freeze every region holding one out of balance for ever. A test pins that too.

`docs/DESIGN.md` §7's "Balance never touches a region that is mid-repair" bullet stated the narrow
definition and is updated in the same change.

### Attribution, since the relay asked

**Not introduced by this wave.** A 6-versus-6 A/B in a detached worktree puts the pre-wave HEAD
`8432814` at **4 passed, 2 failed** — the same intermittent failure, before any commit here. My
`esker-pd` diff up to that point was purely additive (two read-only methods, one routing helper, two
service arms) and touches no scheduling path. The relay's "passed at 18:58, began failing after" was
a true observation of a bug whose rate is roughly one in three and which any slowdown makes certain.
Recorded because the wave found it and fixed it, not to argue about who owned it.

---

## What this wave did not do

* **`pd serve` still exposes no `PdOptions` field.** Unit 7 closed the `esker server` half of
  `docs/bench/phase-4.md`'s wrapper note; `pdwrap` is still needed for `max_store_down_time_ms` and
  the balance knobs, and the note says so rather than claiming otherwise. Not in this brief.
* **`bench --remote` still resolves one static region**, which is the other half of that same note.
* Three documents outside this lane's file list now say something stale, and are listed in this
  lane's report as exact diffs for the coordinator rather than edited here:
  `docs/plans/phase-4-pd.md` §12.3 bullet 3 (unit 5), `docs/plans/phase-4.md` §14.6 bullet 3
  (unit 6), and `docs/plans/debt-c1.md` §"What this lane did not do" bullets 1 and 2 (units 3
  and 8).

