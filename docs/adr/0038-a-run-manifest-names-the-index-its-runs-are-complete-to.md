# 0038 — A run manifest names the apply index its runs are complete to

Status: accepted. Supersedes nothing; extends [ADR 0027](0027-columnar-file-format.md)'s run set with
one number and bumps the run manifest to format version 2.

## Context

A columnar learner's copy of a table is a set of immutable runs under
`<data_dir>/columnar/<region_id>/<tenant>-<table>/`, named by a manifest replaced by atomic rename
(`crates/esker-store/src/columnar/runs.rs`). Opening the copy has, until now, **rebuilt** it: the
manifest was deleted, every run swept as an orphan, and the target refilled by walking the region's
whole `write` column family for that table's key range.

That was correct and deliberately so. A crash loses an unsealed memtable, and nothing about a run
says whether it is short: a partial copy and a whole one are the same files. Rebuilding made
completeness a property of construction rather than of a claim anybody had to trust.

The cost is that an open is linear in the region, at every open — and an open happens on a restart,
after a `close()` that any failed commit causes, and on the first fragment that reaches a table
whose copy is not yet built. Inventory item #10, recorded in the module header that named this fix:
*"the manifest must carry the applied index its runs are complete to."*

## Options

1. **Keep rebuilding.** Free, and wrong for a large region: a store restart re-reads every version
   of every columnar table it holds before it can answer anything.
2. **A file beside the manifest naming the index.** Two writes, two instants. The window between
   them is one in which the number names runs that do not exist yet, or runs exist that the number
   does not cover — and the first of those loses data silently.
3. **The number in the manifest.** One atomic rename publishes the run and the claim about it, or
   neither. This is what the manifest is already for: the same argument that put the live set in one
   pointer rather than in the directory listing puts this there too.
4. **Derive the index from the runs.** A run would have to carry the entry index of every row, which
   is a column of a fact no reader wants, in a format that has golden tests and a fuzz corpus.

## Decision

**Option 3.** The run manifest is format version 2: magic, version, `next`, **`applied`**, count,
the run numbers, CRC32C. `applied` is the region apply index whose committed versions are all in the
runs the same manifest names.

Four rules make it safe, and each is the understating one:

1. **It moves only in a manifest write that a seal performs.** A buffered row is not durable, so an
   index that named one would be a claim a crash could falsify without saying so.
2. **It names an entry, never half of one.** `ColumnarApply::entry_applied` is called after an
   entry's last key has been fed. When the memtable budget sealed a run *inside* an entry, the rest
   of that entry is sealed at its end, so the manifest can name it: otherwise a resume would replay
   an entry a run already partly holds and the copy would carry those rows twice.
3. **The resume decision is made at one engine snapshot.** The applied index is read from the
   region's state record *at the same snapshot* as the data, because the apply path writes both in
   one batch while a fragment on another thread may be opening the copy. Two instants is how a copy
   ends up claiming an entry it never saw.
4. **Anything that does not add up falls back to the full walk, loudly.** No manifest, a version-1
   manifest, no state record for the region, an index below the log's truncation point, an index
   above what the region has applied, a log entry that will not decode: each logs and re-walks. The
   walk is the behaviour that was always correct.

A **version 1 manifest is read, not refused**: it is this format without the number, and it decodes
as `applied = 0`, which is precisely "nothing here says how far these runs go" — the value that asks
for the rebuild. An upgrade therefore pays one full walk per table and never fails to open a copy.

The resume replays entries `applied + 1 ..= applied_index` from the region's Raft log, decodes each
`Command`, and feeds what it committed by reading the `write` column family — the same input the
apply-time tee gets, from the same source of truth. A resumed copy and a re-walked one are asserted
equal.

## Consequences

* An open costs the log since the last seal instead of the region's committed state. A store that
  restarts with a sealed copy and an idle region reads nothing.
* `ColumnarSlot::commit` takes the entry index, and `ColumnarSlot::new` takes the region id — a
  resume replays *that* region's log, and reading the id back out of the directory path would be one
  rename away from replaying somebody else's.
* `ColumnarSlot::last_build` publishes what the last open read, because "did this resume or
  re-walk" is not answerable from the runs: the two produce the same files.
* Log compaction now bounds how far back a copy may resume from. A region compacted past a copy's
  index rebuilds it, which is the old cost paid once rather than a wrong answer.
* `esker_store::raft_log` grows two standalone readers (`read_state`, `read_entry`) for a caller
  that holds a `Db` and a region id and has no business owning the region's storage.
