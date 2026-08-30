# 0010 — Choosing the key a region splits at

Status: accepted (phase 4b). Supersedes nothing. See `crates/esker-store/src/split.rs`,
`docs/plans/phase-4.md` §12, `prompts/04-multiraft-pd.md` 4b.

## Context

A region is split when it grows past 96 MiB (`docs/DESIGN.md` §14). Splitting needs a **boundary
key**, and where that key lands decides whether the split was worth doing: a boundary at the 1st
percentile produces one region holding 99% of the data, which will be split again immediately, and
again, until the region that should have become two has become twenty.

`prompts/04-multiraft-pd.md` specifies "split key chosen at the midpoint of the region's data",
with the size coming from "approximate size from SST properties + memtable". That is how RocksDB
and TiKV do it: an SST records the keys at regular offsets in its index, so the midpoint of a range
is a lookup rather than a scan.

**`esker-engine` exposes neither.** `Version::overlapping` and `FileMeta` hold exactly what is
wanted, `Db::checkpoint` reads them, and `Db`'s `versions` field is `pub(crate)`; `Db::property`
offers `esker.mem-table-size.<cf>` for a whole column family and file *counts* per level, but
nothing per key range and no key sample at all. Adding the accessor is out of this lane
(`docs/plans/phase-4.md` §12.3 states the one method and requests it for 4c).

So the choice is between three ways of picking a boundary from what the store *can* see.

## Options

**A. The arithmetic midpoint of `start_key` and `end_key`.** No I/O at all: treat the two bounds as
big integers and halve. It is what a naive range-partitioner does, and it is wrong for exactly the
key shapes this system is built for. `esker-keys` encodes a table's rows as `t<table><row>`, so a
region holding one table is `["t\x01", "t\x02")` and the arithmetic middle is `t\x01\x80…` — a key
that sorts in the middle of the *key space* between the bounds and nowhere near the middle of the
*data*, which is packed at the low end. Prefix-shaped keys are the normal case here, not the corner
one.

**B. A full scan that counts, then a second that seeks.** Exact: count the keys, then take the one
at position *n*/2. Two passes over up to 96 MiB, and the count is stale by the time the second pass
runs. Exactness buys nothing — the halves diverge again with the next write — and the second pass
is pure cost.

**C. One pass keeping a bounded, uniform sample.** Scan the region's range once, keeping every
*stride*-th key; when the sample fills, keep every second one and double the stride. Take the
sample nearest the middle. One pass, memory bounded by the cap, and the sample stays spread over
everything the scan has seen rather than over its first few thousand keys.

## Decision

**C**, with a cap of 16,384 samples.

The boundary must additionally be:

* **a key that exists**, not a synthesised value — for option A's reason. Any key strictly inside
  the range would be *legal*, but only a real key is *even*;
* **strictly inside `(start_key, end_key)`**, where an empty `end_key` is the end of the key space;
* **not the region's first sampled key**. This is the rule the tests found rather than the design:
  a boundary at the minimum leaves the left half holding nothing, so a region needs at least two
  keys before it has a middle, and the search starts at sample index 1 rather than 0.

A region with no such key — empty, one key, or holding only its own `start_key` — is **not split**,
and that is not an error. Such a region is large because of one large value, and 4b divides key
ranges rather than values.

The search walks **forward** from the middle when the middle sample is not a legal boundary.
Forward can only make the left half larger, and the left half is the one at risk of being empty;
walking back towards `start_key` would search in the direction of the failure.

## Consequences

* **A split costs one full scan of the region.** Roughly 96 MiB of reads, on a blocking thread,
  once per split. It is the price of the missing accessor and it is paid rarely — a region splits
  once per 96 MiB written, and the scan is perhaps a second of work against the minutes that took.
* **The boundary is by key count, not by bytes.** A region whose keys are 900 small and 10 huge
  splits in the middle of the 900, so the halves are uneven in bytes. The next size check catches
  whichever half is still too large, so the error corrects itself; a byte-weighted sample would need
  the value sizes, which is another scan's worth of reading.
* **A cap below 8 is floored to 8.** The sample halves whenever it fills, so a cap of 2 collapses to
  the region's own first key and stays there — the one place the boundary must not be. Nothing real
  configures a cap that low; the floor turns a bad number into a poor sample rather than a broken
  split.
* **Replacing this is a local change.** Nothing outside `split.rs` depends on how the key was
  chosen: the `Split` entry carries the key, and every peer applies the key it is given. When
  `esker-engine` grows the range accessor, option C's scan becomes an index lookup and the three
  rules above are unchanged. That is the reason this is an ADR about *selection* and not about the
  split protocol.
