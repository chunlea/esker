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
