# Phase 7 — the columnar file format

Milestone: [ADR 0022](../adr/0022-columnar-learner-replica.md) "Milestones" item **1**, and only
item 1. Lane `p7-col`. New crate `esker-columnar`; everything else in the workspace is read-only
for this lane, and `esker-sql` is being edited by another lane *right now*.

## What this phase is

ADR 0022 ranks the cost of a columnar replica and puts the file format first: *"a file format with
a footer, a version and golden tests; a set of per-type encodings each with a round-trip proptest;
a stripe/row-group layout with statistics for predicate pruning; ... a crash test ...; and a fuzz
test proving that no arbitrary byte sequence panics a decoder. That list is `esker-engine`'s own
deliverable list with the words changed."*

This plan builds exactly that list and nothing else. It is deliberately the boring half of the
feature, and it is the half both delivery mechanisms — the Raft learner and the 6b tiering rewrite
— share. Choosing between them is milestone 3's problem; neither can start before these bytes
exist.

## Scope

In:

1. A new crate, `esker-columnar`, with **zero new external dependencies**. `esker-base` for
   CRC32C, varints and the seeded RNG; `esker-engine` for the `FileSystem` seam *only* (the one
   dependency ADR 0022 sanctions); `lz4_flex`, `thiserror`, `tracing`.
2. The `ESKERCOL` file layout: stripes → per-column chunks → footer → a fixed 32-byte trailer.
   Every region checksummed; the trailer is the commit point.
3. Per-type encodings for the six types a row carries, each with a round-trip proptest:
   frame-of-reference and delta for integers and timestamps, dictionary or length-prefixed plain
   for text and bytes, bitpacked or RLE for booleans and the null mask, plain for doubles, and
   LZ4 over the top of any of them when it pays.
4. A streaming writer (append rows, seal stripes, finalize) and a reader that opens by the
   trailer and decodes only the columns it was asked for.
5. Per-column-chunk statistics — min, max, null count — in the footer, so that pruning a stripe
   costs no I/O beyond the footer. Nothing consumes them yet; that is milestone 2.
6. Byte-exact golden files, a crash test over every truncation point, and a decoder fuzz.

Out — named so nobody looks for them:

- **No Raft, no learner, no apply stream.** ADR 0022 milestone 3.
- **No fragment protocol, no scan path, no filter/aggregate evaluation.** Milestone 2. The
  statistics this phase writes are for that phase's pruner, and this phase only proves they are
  true.
- **No SQL, no planner, no routing, no `EXPLAIN`, no session GUC, no catalog flag.** Milestone 4.
  Not one line of `esker-sql` is touched.
- **No MPP, no exchange, no shuffle.** Milestone 5, and only if measured.
- **No compaction.** Appending a stream column-wise produces small runs and they will need
  merging, exactly as ADR 0022 says. A compaction strategy needs a write path to observe first.
- **No mutation and no deletes.** A columnar file is immutable, like an SST (invariant 3).
  MVCC visibility is a *column* here — a `commit_ts` the reader can filter on — not a mechanism
  this crate implements.
- **No zero-copy read path.** The reader decodes a chunk into owned buffers. Milestone 2 profiles
  the scan and decides whether it needs more; optimising first is what `CLAUDE.md` forbids.
- **No new column types.** The set is the six `esker_sql::value::ColumnType` carries and no more —
  a richer model than a row can hold is dead code that still has to be fuzzed.

## The layout

```text
file   := stripe* ++ footer ++ trailer
stripe := chunk*                       one chunk per column, in schema order
chunk  := payload ++ codec:u8 ++ crc32c:u32          (the SST block rule, ADR 0002)
```

### The trailer — 32 bytes, exactly, forever

A reader seeks to `file_size - 32`. It is fixed size so that finding it needs no search, and the
last thing written so that its presence *is* the commit:

```text
byte  0.. 8   footer_offset:  u64 LE
byte  8..12   footer_len:     u32 LE
byte 12..16   footer_crc32c:  u32 LE   over the footer payload
byte 16..20   trailer_crc32c: u32 LE   over bytes 0..16
byte 20..24   format_version: u32 LE   = 1
byte 24..32   magic:          the 8 ASCII bytes "ESKERCOL"
```

**A file without a valid trailer is not a file.** Not a damaged one — one that was never
finished. The writer writes stripes, then the footer, then the trailer, then `sync_data`, then
renames the temporary into place and syncs the directory (invariant 3, the same shape as an SST).
Every prefix of that sequence reads back as `Error::Unsealed`, which is a *different* error from
`Error::Corruption`: a torn write during a crash is expected and the file is discarded, whereas a
sealed file whose bytes have rotted is an alarm. Conflating them would either make every crash
look like corruption or make corruption look survivable.

### The footer

Hand-written little-endian and LEB128 varints, never serde (ADR 0002). It carries the schema and
one entry per column chunk, statistics included, so that opening a file and pruning a stripe are
one read:

```text
footer := schema ++ stripe_index

schema       := column_count:varint ++ column*
column       := name_len:varint ++ name ++ type_tag:u8
stripe_index := stripe_count:varint ++ stripe*
stripe       := rows:varint ++ offset:varint ++ len:varint ++ chunk*
chunk        := offset:varint ++ len:varint ++ encoding:u8 ++ stats
stats        := null_count:varint ++ flags:u8 ++ [min_len:varint ++ min] ++ [max_len:varint ++ max]
```

`type_tag` is the row side's own tag byte (`esker_sql::catalog::record`): `1` int8, `2` text,
`3` bool, `4` bytea, `5` timestamptz, `6` double. Sharing the numbering is free and means one
table's type set has one spelling on disk; the crates stay unlinked, and a test pins the values.

`flags` says which bounds are present and whether they are *truncated* — bit 0 min present, bit 1
max present, bit 2 min is a lower bound rather than the value, bit 3 max is an upper bound. A
pruner that cannot tell an exact bound from a truncated one will eventually conclude equality from
one, which is a wrong answer; the bit costs nothing and closes it now rather than in milestone 2.

### A chunk

```text
payload := encoding:u8 ++ rows:varint ++ null_count:varint
             ++ [null_mask if null_count > 0]
             ++ values                        only the rows that are not NULL
```

The chunk repeats `rows` and `null_count` from the footer on purpose: **a decoder trusts nothing
it did not read from the region it is decoding.** A footer that disagrees with a chunk is
corruption and is reported as such, rather than being resolved in favour of whichever came to
hand.

The null mask is a boolean encoding of its own — bitpacked or run-length, whichever is smaller —
so a column with no NULLs costs nothing and one that is nearly all NULL costs a handful of bytes.
Values are stored **densely, present rows only**, so a NULL costs one bit rather than a slot.

### The encodings

| Type | Encodings | Chosen by |
|---|---|---|
| `Int8`, `TimestampTz` | frame-of-reference (`min` + bit-packed offsets), delta (zigzag deltas, themselves frame-of-reference packed) | whichever encodes smaller |
| `Text`, `Bytea` | plain (bit-packed lengths + concatenated bytes), dictionary (bit-packed codes) | whichever encodes smaller; the dictionary is skipped when distinct values exceed half the rows |
| `Bool` | bitpacked, run-length | whichever encodes smaller |
| `Double` | plain, 8 bytes LE | only one |

Then LZ4 over the encoded payload, and only when it saves more than an eighth — the engine's own
rule, for the engine's own reason: a barely smaller block does not repay the CPU to decompress it.

Two arithmetic notes that the proptests exist to hold:

* **Delta uses wrapping arithmetic.** `i64::MAX - i64::MIN` does not fit an `i64`; the delta is
  `wrapping_sub`, zigzagged into a `u64`, and the decoder `wrapping_add`s it back. Exact for
  every pair of `i64`s, including the ones that overflow.
* **Frame-of-reference subtracts in `u64`.** `value - min` where both are `i64` can exceed `i64`
  but never `u64`, so the offset is `(v as u64).wrapping_sub(min as u64)` and the bit width comes
  from the largest of them.

Doubles get no delta and no dictionary. Delta on a float is either lossy or pointless, and a
dictionary needs a notion of "the same double" that has to answer for `NaN` and `-0.0` — a second
equality relation in a format whose statistics already need one. LZ4 handles the repetitive case,
and if a profile later says that is not enough, adding an encoding is a version bump and a golden.

### Statistics, and the two floating-point rules

`min`, `max` and `null_count` per column chunk. Two cases have a right answer that is not the
obvious one, both taken from what Parquet learned:

* **`NaN` is not in the range.** A `NaN` is excluded from min and max; a chunk of nothing but
  `NaN` has no bounds at all. Comparing with `NaN` in the mix would produce a bound that fails
  every comparison, and a pruner would skip a stripe that contains matching rows.
* **Zero is signed and comparison does not care.** `-0.0 == 0.0`, so a min of `0.0` is written as
  `-0.0` and a max of `0.0` as `+0.0`. The bound then holds under both bitwise and numeric
  reading.

Text and byte bounds are truncated to 64 bytes so that one wide value cannot inflate a footer:
a truncated min is the first 64 bytes (which sorts at or below the true min), and a truncated max
is the first 64 bytes with the last byte below `0xFF` incremented and the trailing `0xFF`s dropped
(which sorts at or above the true max). A max whose first 64 bytes are all `0xFF` has no upper
bound and is written absent. Both cases set their truncation flag.

## Files

```text
crates/esker-columnar/
  Cargo.toml
  src/lib.rs          crate docs, the frozen constants, re-exports
      error.rs        Error / Result: Corruption, Unsealed, Io, InvalidArgument, Unsupported
      value.rs        ColumnType, Value, ValueRef, ColumnDef, Schema
      format.rs       Trailer, Footer, StripeMeta, ChunkMeta — encode and decode
      frame.rs        chunk framing: LZ4 + codec byte + CRC32C
      column.rs       Column, ColumnData, NullMask, the row-order iterator
      stats.rs        ColumnStats, the accumulators, bound truncation
      encode/mod.rs   Encoding tag, encode_column / decode_column dispatch
      encode/bitpack.rs   the bit-width packer
      encode/boolean.rs   bitpacked / RLE, and the null mask
      encode/integer.rs   frame-of-reference / delta
      encode/bytes.rs     plain / dictionary
      encode/double.rs    plain
      writer.rs       StripeWriter: append_row, seal_stripe, finish
      reader.rs       Reader: open, schema, stripes, read_column
  tests/golden/*.col  frozen files, byte for byte
  tests/golden.rs     rebuild-and-compare, plus read-the-committed-file
  tests/crash.rs      every truncation point, and every single-byte flip
  tests/fuzz_decode.rs arbitrary bytes into every decode entry point
  tests/roundtrip.rs  writer → reader over generated batches
```

Files stay under ~800 lines; `encode/` is split by type for that reason and because a per-type
proptest belongs next to the type it exercises.

## Tests

| Kind | What it holds |
|---|---|
| Round-trip proptest, per encoding | every encoding is exact for every input, including `i64::MIN`, empty strings, all-NULL, single-row and zero-row chunks |
| Round-trip proptest, whole file | a generated schema and batch survives write → read for every projection |
| Encoding choice | the writer picks the smaller encoding, and the decoder reads whichever it picked |
| Statistics | min/max/null-count are true for generated batches; truncated bounds *bound*; `NaN` and `-0.0` follow the rules above |
| Golden | frozen files rebuilt byte-for-byte, and the committed bytes still read |
| Crash | truncation at every one of a file's lengths gives an error, never a panic and never a short read presented as complete |
| Corruption | every single byte of a golden file flipped in turn: an error, or exactly the right data |
| Fuzz | arbitrary bytes into every decode entry point, for a real minute, never a panic |

## Risks

* **A decoder that allocates what a corrupt length asks for.** The SST reader already met this
  (`max_plausible_raw_len`); every count in this format — dictionary size, value count, string
  length — is checked against the bytes actually available before a `Vec` is sized. The fuzz test
  is what proves it, and it is the reason unit 7 is a real minute rather than a smoke test.
* **The bit packer is the one place a subtle bug hides silently.** Wrong shifts read plausible
  numbers rather than failing. It is the first thing built, it is proptested against a naive
  reference implementation, and every other encoding is layered on it.
* **Statistics that are subtly false are worse than absent**, because milestone 2 will prune with
  them and the failure is a missing row, not an error. They are proptested against a recomputed
  truth on the decoded values, not against the writer's own accumulator.
* **Scope creep towards milestone 2.** A reader that can prune is one step from a reader that can
  filter. The line here is: this crate computes and exposes statistics, and evaluates no
  predicate.

## Not doing, restated

No Raft. No learner feed. No fragment protocol. No SQL, planner or `EXPLAIN`. No MPP. No
compaction. No mutation. No new dependency. No edits outside `crates/esker-columnar/**`, the one
workspace-member line in the root `Cargo.toml`, this file, and the ADR that records the format.

## Units, in order, each committed

0. This plan.
1. Primitives and the layout skeleton: error type, types, trailer, footer, chunk framing. ADR.
2. The encodings, with their proptests: bit packer and booleans; integers; bytes and doubles.
3. Writer and reader.
4. Statistics.
5. Goldens.
6. The crash test.
7. The decoder fuzz.

## What changed while building

All seven units landed as planned. Five things are worth recording, four of them decisions the
plan did not anticipate and one a number that came in below the ADR's estimate.

**The trailer grew a checksum over itself.** The plan had it carrying the footer's CRC and its own
magic. That is not enough: a torn write that happens to leave the magic intact hands a reader a
`footer_offset` and a `footer_len` that nothing has questioned yet, and believing them for one
read is believing an attacker-chosen length. Four more bytes make the trailer self-verifying
before any of its fields is used, and it is now the first check that runs. Recorded in
[ADR 0027](../adr/0027-columnar-file-format.md), decision 1.

**`encode_column` and `decode_column` are public.** They were going to be internal to the writer
and reader. Making them the crate's core public operation costs nothing, is what the fuzz feeds
directly, and is what milestone 2 will want when it has a chunk's bytes and no file — a fragment
evaluator reading from object storage does not necessarily hold a `Reader`.

**`Column` is the shape in both directions.** The plan had a builder for writing and a decoded
column for reading. Collapsing them into one type means a round-trip test is `decode(encode(c))
== c` over a single type with no adapter in between that could quietly correct a mistake. It also
forced `Column::identical`, which compares floating point by bits — `NaN != NaN` would have made
every round-trip assertion over doubles pass without proving anything, which is the sort of test
that is worse than no test.

**Two golden files rather than one.** An LZ4 file alone would let an encoding change hide behind
the compressor. The uncompressed file pins the encodings; the LZ4 file pins what is actually on
disk, `lz4_flex`'s output included, so a dependency bump that moves those bytes fails a test
rather than silently rewriting every file the next compaction touches.

**`docs/bench/` was out of the lane**, so the compression measurement lives in
`crates/esker-columnar/tests/compression.rs` and its numbers are below. It asserts floors rather
than tracking a curve, so it is a regression guard; a proper bench entry belongs with milestone 2,
which is the first thing that will have a *read* worth timing.

### The numbers

50,000 rows of a mixed ledger corpus — ascending ids and timestamps, a five-value label, a boolean
that runs, amounts in a narrow band, and a payload of random bytes. Two handicaps keep it honest:
the row baseline counts **values only**, though every row in an SST also carries a key, and the
random payload column is incompressible and puts a hard floor under every ratio.

```text
raw rows            2 329 831 bytes
lz4 rows (4 KiB)    1 643 314          1.42x
columnar, plain     1 020 068          2.28x vs raw   (the encodings alone)
columnar, lz4         896 176          2.60x vs raw, 1.83x vs lz4 rows
```

ADR 0022 assumed **3×** against what Esker already stores. The measured answer on this corpus is
**1.83×**, and the gap is entirely the incompressible column: it is roughly a third of the bytes
and no layout can do anything with it. Against the uncompressed baseline the published figures are
quoted against, the answer is 2.60×. The ADR's disk arithmetic should be read with 1.8–2.6× rather
than 3×, which moves "one columnar learner costs +11%" to about +40–55% of one replica — still
single-digit to low-double-digit percent of a three-replica total, so the conclusion stands and
the number does not.

### What the tests actually ran

* **Goldens:** two frozen files; rebuilt byte-for-byte, re-read from disk, and every single byte
  flipped at two bit positions — 42,000 mutations, every one detected. No region of this format is
  uncovered by a checksum, and that test is what says so.
* **Crash:** every one of ~9,000 truncation points of a finished file and of a half-written
  temporary. Not one read back as complete; almost all were reported unfinished and the handful
  landing inside the trailer were reported corrupt.
* **Fuzz:** 90 seconds in release over two campaigns — 4,579,520 mutated whole files and
  2,403,072 with the footer left intact so the damage lands in the stripe data. No panic, no
  inconsistent answer. The committed budget is two seconds; `ESKER_FUZZ_SECONDS=60` runs a real
  one.

### Owed to milestone 2

* Nothing prunes. The statistics are written, proven true, and read by nothing.
* The reader decodes into owned buffers. Whether that costs anything is a question for the first
  scan that exists.
* The differential test ADR 0022 asks for — every query answered both ways and compared — cannot
  be written until there is a query. It is milestone 2's first deliverable, not its last.
