# 0005 — Two bounds the SST format needs: a maximum table size, and a block order

Date: 2026-08-30 · Status: accepted · Phase: 1

## Context

`docs/DESIGN.md` §4.5 fixes the SST footer at **48 bytes** and says what it holds: an index
handle, a filter handle, a properties handle, a format version, and the magic `ESKERSST1`. That
size is not decorative. A reader has no way to find the footer except by seeking to
`file_size - 48`, so the number has to be a constant of the format rather than something
derived from the file.

Spending those 48 bytes leaves less room than it looks:

| Field | Bytes |
|---|---|
| magic `ESKERSST1` | 9 |
| `format_version: u32` LE | 4 |
| three block handles | **35** |

A block handle is `{offset, size}`, and both are byte positions in a file, so both are `u64`.
Three handles is six integers. Encoded as LEB128 varints a `u64` can take **10 bytes**, so the
worst case is 60 bytes in a 35-byte hole — the footer as specified cannot always be written.

Nothing in the design document had noticed this, because it is only visible once you try to
encode a handle near the top of the `u64` range.

A second, smaller question came out of the same work. §4.5 lists the blocks a table contains
but not the order they are written in, and the writer needs an order it can commit to: it
appends to a `WritableFile` in one pass and never seeks back, so any field of a block that
describes a *later* block is unwritable.

## Options

**Fixed-width fields instead of varints.** `u64` offset plus `u32` size is 12 bytes a handle,
36 for three — one byte over. Two `u64`s is 48, well over. Both fail, and the second-best
option here would have cost 27 bytes of padding in every table for a field that is almost
always small.

**Grow the footer.** Contradicts §4.5 and `format::SST_FOOTER_SIZE`, and buys nothing that a
bound does not: a 64-byte footer holds three worst-case varint handles with room to spare, but
"48" is already the number every reader will be written against.

**A metaindex block, as LevelDB has.** LevelDB's footer carries only two handles — metaindex
and index — precisely because `2 × 20 + 8 = 48`. The filter and properties handles then live
inside the metaindex, keyed by name. That is a real solution, and it is more extensible: a new
meta block costs no footer bytes. It also costs a block read and a lookup at every table open,
for a table format that will only ever have these three meta blocks, and it makes `sst-dump`
on a damaged file a two-step recovery instead of one.

**Bound the table size so the varints are bounded.** A varint of a value below 2^35 is at most
five bytes, so six of them is at most 30, inside 35 with five bytes to spare.

## Decision

**1. `sst::footer::MAX_TABLE_SIZE = 2^35` — 32 GiB.** `TableBuilder` checks the running offset
after every block it appends and fails with `Error::InvalidArgument` rather than writing a file
whose footer it could not encode. `Footer::encode` checks the encoded handles against the
region independently, so a future change that reintroduces the problem fails a test rather than
silently overflowing.

The padding sits *after* the handles, and a zero byte is itself a valid varint encoding of
zero, so a decoder reads exactly six varints from the front and ignores the rest. That also
makes `{offset: 0, size: 0}` available as "this block is absent", which is how a table with no
bloom filter records its filter handle: offset 0 is where the first data block starts, and the
first data block is never the filter, the index or the properties.

**2. The block order is data → filter → index → properties → footer.**

The properties block records `esker.index.size`, which is only known once the index block has
been written. A single-pass, append-only writer therefore cannot put properties before the
index. Everything else follows: the filter is complete as soon as the last data block is
flushed, and the footer is last because that is how it is found.

## Consequences

* 32 GiB is not a limit anyone will meet. A region splits at 96 MiB (§14), L1 is 64 MiB with a
  ×10 multiplier over 7 levels, and an L0 file comes from a 64 MiB memtable. The largest table
  the design can produce is roughly three orders of magnitude below the cap.
* If some future compaction genuinely wants a larger file, raising this constant is **not** the
  fix: it would produce files whose footers cannot be encoded. The fix is a format version bump
  with a wider footer or a metaindex, which is the option this ADR declined and which stays
  available.
* The cap is a property of the *format*, not of a build. It belongs with the other frozen
  numbers, so changing it is a format change under ADR 0002.
* The block order is **not** load-bearing for readers. Every block is named by an explicit
  handle in the footer, so a reader never assumes where anything is, and `TableReader` does
  not. What depends on the order is the writer's single-pass property and the accuracy of
  `esker.index.size`. Reordering is therefore a change to this ADR and to the writer, not a
  format version bump — but it is pinned here so nobody rediscovers the index-size problem.
* `esker-cli sst-dump` prints blocks in this order and a test asserts the layout is contiguous,
  which is what turns the order from a convention into something checked.
