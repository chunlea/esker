# 0109 — An operator can ask a store to flush and to compact

Status: **Accepted**, 2026-09-10.

## Context — the measurement that could not be taken

`Store::flush` and `Store::compact_write_cf` have existed since phase 2 and, between them, have
exactly **one caller**: `esker bench --compact`, which opens its own database rather than speaking
to a running store. Nothing that reaches a store over a socket can ask it to flush.

That is not an abstract gap. #58 is the finding that the same work costs more the longer a node has
been running with the catalog size pinned, and the first arm to isolate it reported this:

```text
arm A, four passes, every one:   sst = 0 · MANIFEST = 468 bytes, unchanged
```

The node never crossed `write_buffer_size` — 64 MiB by default, and four passes of that workload
came to 40 MB across the whole cluster — so **no SST was ever written**. Two candidate causes were
ruled out by that one fact (there is no compaction debt without files to compact, and no manifest
churn with an unchanging manifest), and one was left *unmeasured* rather than ruled out: tombstones
live in the memtable and the WAL here, and `esker-cli sst-dump` had nothing to open.

The arm that would separate "the cost is the accumulated data" from "the cost is something a
restart rebuilds" is a flush arm, and it cannot be written. The only lever available is a restart,
which changes several things at once.

## Options

### How an operator reaches the store

* **(a) A new top-level RPC service.** A whole `Storage` service, its own code range, its own
  handler. Nothing else would go in it for now.
* **(b) Two more methods on the existing `Admin` service.** `Request::Admin(AdminReq)` already
  exists and is already documented as *"an operator asking a store to do one specific thing"* —
  which is exactly what this is. It carries no `RequestHeader`, which suits a request that is about
  a store and not about a region.
* **(c) A signal, or a file the store watches.** No answer, no receipt, and nothing to assert on.
* **(d) Widen `esker bench --compact`.** It is a benchmark that owns its database; making it drive
  somebody else's store is a different command wearing this one's name.

### What the answer carries

* **(e) Nothing — an empty ack.** The arm then has to read the store's data directory to find out
  what happened, which it can only do when it is on the same machine.
* **(f) A count.** Enough for *"sst > 0"*, and not enough to see that a flush put one file in L0
  while compaction moved three to L1 — which is the shape #58 is asking about.
* **(g) Every SST the store holds, per column family, as `(level, number)`.** `Db::files_by_level`
  already computes exactly this, for the range-tombstone invariant. Tens of files, so the answer is
  small.

## Decision — (b) and (g)

Two methods on the existing admin service:

| Method | Code | Request | Answers |
|---|---|---|---|
| `Admin::Flush` | `0x0504` | `AdminReq::Flush` | `AdminResp::Storage` |
| `Admin::Compact` | `0x0505` | `AdminReq::Compact { cf }` | `AdminResp::Storage` |

`AdminResp::Storage` is every SST the store holds after the operation: a list of column families,
each with its name and its `(level, file number)` pairs.

**Both answer only when the work is done.** `Db::flush_all` already blocks on `wait_for_flush`, and
`compact_range` on the compaction it schedules; the methods add no cadence of their own. An
operator's `esker admin flush --store <addr>` that returned early would be useless to the thing it
is being built for, which is an arm that asserts on what the flush produced.

`Compact`'s `cf` is optional — empty means every column family. `esker bench --compact` compacts
`write` alone because that is where MVCC versions are, and an arm looking at #58 will want the same
default made explicit rather than assumed.

### This is purely additive, and nothing existing moves

* Two new `Method` codes at the **end** of the admin range. No existing code changes value.
* Two new `AdminReq` variants and one new `AdminResp` variant. Encoding is per-method — `AdminReq`
  and `AdminResp` both decode by `Method` rather than by a leading tag — so no existing message's
  bytes change by a single byte.
* **No golden is touched.** A reader that does not know `0x0504` answers it the way this protocol
  already specifies for an unknown method, which is the behaviour `Method::from_code` returning
  `None` already produces.
* No format version moves. `CATALOG_FORMAT_VERSION` is a catalog concern and this is not one.

## Rationale

**(b) over (a)** because a new service for two operator verbs is a code range and a handler for
something the tree already has a place for, and the admin service's own doc sentence describes this
request without amendment.

**(g) over (f)** because the extra cost is a few dozen varints and the extra information is the
question being asked. #58's next arm is "flush, then measure": what it needs to see is whether the
pass that follows a flush is cheaper, and if it is not, whether the files went anywhere. A count
cannot distinguish one L0 file from one L1 file, and that distinction is the difference between
"the versions are still all here" and "they were merged away".

**Why not leave it to a restart.** A restart replays the WAL and rebuilds the same memtable, so it
separates "cost is the data" from "cost is an in-memory structure a replay does not rebuild" — a
real and useful cut, and r1 is taking it. It does not separate either from "cost is the number of
versions a read walks", because a replay rebuilds those too. Only a flush plus compaction removes
versions, and until now nothing outside the process could ask for either.

## Acceptance

`crates/esker-cli/tests/admin_storage.rs`, two tests against a **real store process**:

* Rows written, **no SST on disk** — asserted before the flush, because a store that flushed on its
  own would make the verb look like it worked — then `esker admin flush --store <addr>`, then SSTs
  on disk, and a receipt that names them.
* `esker admin compact --store <addr> --cf write` answers with what is left.

The on-disk count is read from the data directory rather than from the verb's own answer, so the
receipt is checked against something it did not produce.

## Consequences

* **An arm can flush.** #58's open question — does the climb survive a flush — becomes a
  measurement rather than an inference, and r1's arm C no longer has to reach it sideways by
  setting `write_buffer_size` small at startup.
* **An operator can flush a production store**, which is a thing an operator can now do at a bad
  moment. It is a write stall, not a correctness risk: a flush is what the store does on its own
  every time a memtable fills.
* **`compact_write_cf` gains a general form.** It compacted `write` and nothing else; it now takes
  a column family, with `write` still what the benchmark asks for.
* **Two more methods to keep answering.** Both are thin — they call a `Store` method that already
  existed and read a version list — so the surface added is the wire, not the behaviour.
