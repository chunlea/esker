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
  src/lib.rs          crate docs, the frozen constants and limits, re-exports
      error.rs        Error / Result: Io, Corruption, Unsealed, InvalidArgument
      cursor.rs       the bounds-checked reader every decode path goes through
      value.rs        ColumnType, Value, ValueRef, ColumnDef, Schema
      footer.rs       Trailer, Footer, StripeMeta, ChunkMeta — encode and decode
      frame.rs        chunk framing: LZ4 + codec byte + CRC32C
      column.rs       Column, ColumnData, NullMask, ColumnBuilder, the row-order iterator
      stats.rs        ColumnStats, the accumulator, bound truncation
      encode/mod.rs   Encoding tag, encode_column / decode_column
      encode/bitpack.rs   the bit-width packer, and frame-of-reference over u64
      encode/boolean.rs   bitpacked / RLE, and the null mask
      encode/integer.rs   frame-of-reference / delta
      encode/bytes.rs     plain / dictionary
      encode/double.rs    plain
      writer.rs       Writer: append_row, seal_stripe, finish
      reader.rs       Reader: open, schema, stripes, read_column, read_stripe
  tests/corpus.rs     the deterministic ledger batch the fixtures are built from
  tests/golden/*.col  frozen files, byte for byte
  tests/golden.rs     rebuild-and-compare, read-the-committed-file, flip-every-byte
  tests/crash.rs      every truncation point, of a finished file and of a temporary
  tests/fuzz_decode.rs arbitrary bytes, and mutations of a valid file
  tests/roundtrip.rs  writer → reader over generated schemas and batches
  tests/statistics.rs the footer's claims, checked against the chunks
  tests/compression.rs the measurement, with floors as a regression guard
```

Files stay under ~800 lines; `encode/` is split by type for that reason and because a per-type
proptest belongs next to the type it exercises. Two departures from the sketch above as first
drafted: the layout module is `footer.rs` rather than `format.rs` (the frozen constants live in
`lib.rs::format`, matching `esker_engine::format`), and `cursor.rs` was added — see
[what changed](#what-changed-while-building).

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

**A stripe is capped at 4Mi rows, and the fuzz did not find that.** A review pass after unit 7
found the one allocation shape the cursor's rule does not catch. Every count in this format is
refused when its items cannot fit in the bytes behind it — but a one-bit encoding satisfies that
rule honestly: a 256 MB chunk really does hold two billion one-bit values, and decoding them into
`u64`s asks for sixteen gigabytes. The same shape is reachable through a dictionary's entry count.
Two caps close it, the row count and the unpacked output, and the row cap outranks the writer's
options because a limit a writer can be configured past is not a limit. Worth recording *how* it
was found: the fuzz corpus is five kilobytes, so no mutation of it could ever produce a chunk
large enough to matter. A fuzz proves the decoders survive the inputs it can build, and the size
of its corpus is part of what it does not prove.

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

---

# Milestone 2 — the scan path and the fragment, locally

[ADR 0022](../adr/0022-columnar-learner-replica.md) "Milestones" item **2**, and only item 2:
*"a scan path and the fragment protocol, evaluated locally against that format, with the
differential test against the row engine standing up from the first query."*

M1 is accepted and the format is frozen. This milestone makes it answer questions.

## What this phase is

A **fragment** is a piece of a plan — a table, a key range, a projection, a filter over the
projected columns, and a set of aggregates with an optional grouping. ADR 0022 decision 3 puts it
on the seam `TxnKvReq::Scan` already establishes: a request that carries work rather than a range,
and returns what the work produced rather than what it read. This phase defines the fragment's
bytes and evaluates one **locally**, against a columnar file, with no service and no wire.

The load-bearing unit is not the evaluator. It is the **differential harness**: a second,
deliberately naive interpreter that answers the same fragment by materialising every row and
looping over it, and a proptest that generates fragments until the two disagree. ADR 0022 names
the two engines disagreeing as the worst failure this feature can have, "because it is silent",
and says the defence "has to be built *before* the routing rule and not after". This is that
defence, built one milestone before there is a routing rule to defend.

## Scope

In:

1. The fragment as a **format**: version byte, hand-written little-endian body, CRC32C, golden.
   Unknown version, node tag, operator or aggregate refuses the **whole** fragment.
2. A local evaluator: stripe pruning from M1's statistics, decoding only projected columns,
   three-valued filtering, and partial aggregates.
3. The differential harness, and the generated-fragment proptest behind it.
4. A fragment-decoder fuzz, and the M1 file fuzz extended to run fragments against mutated files.
5. `docs/bench/columnar-m2.md`: scan throughput, the projection effect, the pruning effect, and
   M1's compression table, which owed itself a bench home.
6. One new `docs/DESIGN.md` section.

Out — named so nobody looks for them:

- **No wire service.** The fragment's bytes are defined here and `esker-proto` is another lane's;
  what it must add is a report item, not a commit. Nothing here opens a socket.
- **No planner routing, no cost rule, no `EXPLAIN`, no session GUC.** ADR 0022 milestone 4.
- **No learner feed and no tiering rewrite.** Milestone 3 picks one; this phase feeds neither.
- **No MPP, no exchange.** Milestone 5, and only if measured.
- **No cross-file or cross-region combination.** A fragment is evaluated against one file. The
  two-level finish belongs to whatever calls many of them, and the float-association question it
  raises is recorded below rather than answered here.
- **No `ORDER BY`, `LIMIT` on aggregates, `HAVING`, `DISTINCT`, joins or arithmetic.** The
  fragment is scan, filter, project, aggregate, exactly as ADR 0022 scopes it. `LIMIT` exists on
  the row output only, where it is a bound on work rather than an operator.

## The correction M2 forces on M1

**M1's floating-point statistics are unsound for pruning in this system, and the differential
harness is what would have found it.** M1 took Parquet's rule — exclude `NaN` from `min`/`max` —
and Parquet is right, for IEEE semantics, where every comparison with `NaN` is false and a `NaN`
row can never match a range predicate.

This system does not have IEEE semantics. `esker_sql::value::Datum::pg_cmp` implements
PostgreSQL's ordering, where **`NaN` is greater than every other float, `Infinity` included, and
equal to itself** — measured against a real server, and the reason `WHERE x > 5` genuinely
matches a `NaN` row. So a chunk holding `[1.0, NaN]` gets `max = 1.0` under M1's rule, a pruner
skips it for `x > 5`, and the `NaN` row is lost. A missing row, silently, which is the exact
failure mode the statistics were tested so hard against.

The fix is one line of principle: **bounds are computed in the ordering the query engine uses.**
`NaN` is therefore the maximum, and `min` is `NaN` only when every value present is one. The
signed-zero rule survives unchanged. No golden byte moves — the golden corpus contains no `NaN` —
and ADR 0027 is amended, because the rule it records is the wrong one. It lands before the pruner,
so the pruner is never briefly built on it.

## The fragment

A message, not a file: no magic and no trailer, because it arrives inside a frame that already
said how long it is.

```text
fragment := version:u8 ++ body ++ crc32c:u32          the CRC covers version ++ body
body     := table ++ range ++ projection ++ filter ++ output

table      := tenant:varint ++ table_id:varint
range      := start_len:varint ++ start ++ end_len:varint ++ end
projection := count:varint ++ column:varint *          indexes into the file's schema
filter     := present:u8 ++ [expr]
output     := kind:u8 ++ (rows | aggregates)
  rows       := limit:varint                           0 means unbounded
  aggregates := group_count:varint ++ slot:varint *
             ++ agg_count:varint ++ (kind:u8 ++ [slot:varint]) *

expr := node:u8 ++ ...
  1 Column  ++ slot:varint
  2 Literal ++ type_tag:u8 ++ value                    type_tag 0 is NULL, 1..6 are M1's tags
  3 Compare ++ op:u8 ++ expr ++ expr                   1 =, 2 <>, 3 <, 4 <=, 5 >, 6 >=
  4 And     ++ expr ++ expr
  5 Or      ++ expr ++ expr
  6 Not     ++ expr
  7 IsNull  ++ negated:u8 ++ expr
```

**Everything after the projection refers to projection *slots*, never to table columns.** A filter
cannot name a column the projection did not ask for, which makes "decode only what was projected"
a property of the format rather than a discipline the evaluator has to keep. It is also what makes
the invocation-counting test below meaningful rather than decorative.

**The expression tree has a depth limit** (32), enforced while decoding. A recursive decoder
without one is a stack overflow reachable from a wire message, which is a panic on untrusted input
by another name (invariant 9).

### Refuse, never partially honour

ADR 0022 decision 3 and `crate::plan`'s own rule. A fragment this build cannot evaluate comes back
as `Error::Refused` — a new variant, distinct from `Corruption` (the bytes are damaged) and from
`InvalidArgument` (the caller of this crate made a mistake) — and the caller falls back to a row
scan. Refused, specifically:

* an unknown version byte, node tag, comparison operator, aggregate kind or output kind;
* a projection slot, group slot or aggregate slot outside the projection;
* a column index outside the file's schema;
* `sum` of anything but `int8` or `double`;
* **a key range that is not unbounded.** A columnar file records no key range, so this build
  cannot honour one. Carrying the field and ignoring it is precisely the defect the rule exists
  for; M3 gives the field meaning, and until then a bounded range is refused.

## Aggregate semantics, which the row side does not have

`crates/esker-sql/src/parse/lower.rs` refuses `GROUP BY`, `HAVING` and `SELECT DISTINCT` with
`0A000`, and `crate::plan::Expr` has no function node at all: **there are no aggregates on the row
side to match.** So they are defined here, against PostgreSQL rather than against a sibling, and
the differential harness tests both sides against the definition.

| Rule | Why |
|---|---|
| `count(*)` counts rows, including all-NULL ones | PostgreSQL |
| `count(col)` skips NULLs | PostgreSQL |
| `sum`, `min`, `max` over no rows or only NULLs are **NULL**, not zero | PostgreSQL; a zero here is a wrong answer that looks like data |
| `min`/`max` order by `pg_cmp` — `NaN` largest, `-0.0 == 0.0`, text by bytes | the ordering everything else in this system uses |
| grouping identity is `pg_cmp` equality; NULL forms one group of its own | PostgreSQL's `GROUP BY`; `pg_cmp` makes NULL equal only to NULL |
| groups come back in `pg_cmp` order of their keys, and a group's key is the first one seen | determinism, which a byte-comparing harness requires |
| `sum(int8)` overflowing is a typed error on both sides | **a declared divergence.** PostgreSQL's `sum(bigint)` returns `numeric` and cannot overflow; phase 6a has no `numeric` (ADR: `crate::plan::expr` refuses decimal-to-`int8` for the same reason). Returning a wrapped number would be silently wrong, so it is `Error::Overflow` |
| `sum(double)` accumulates left to right in **row order across the whole file** | so that a fragment's answer does not depend on where stripe boundaries fell |

That last row is the one to read twice. Floating-point addition is not associative, so summing per
stripe and adding the partials gives a different answer from a flat fold. This phase therefore
keeps **one accumulator per group across every stripe** rather than combining per-stripe partials
— the result is a flat left fold in row order, which the reference reproduces exactly. Combining
partials *between files* changes the answer, and that is milestone 4's problem to state; it is
recorded here so it is a decision there rather than a discovery.

## Pruning, and why it is sound

For each stripe, each **top-level conjunct** of the filter is tested against the chunk statistics
of the column it names. Only conjunctions count — a comparison under an `OR` constrains nothing —
which is the same rule `esker_sql::exec::query` applies to required constants, arrived at
independently and for the same reason.

```text
col < lit  or  col <= lit     skip when lit is below min
col > lit  or  col >= lit     skip when lit is above max
col =  lit                    skip when lit is outside [min, max]
any comparison                skip when the chunk is entirely NULL
IS NULL                       skip when null_count is 0
IS NOT NULL                   skip when null_count is the row count
```

Soundness rests on one fact: **truncation only ever widens a bound.** A truncated minimum sorts at
or below the true minimum and a truncated maximum at or above the true maximum, so a range that
excludes the whole widened interval excludes the true one. No rule above concludes *equality* from
a bound, which is the one thing a widened bound cannot support and the reason M1 wrote the
truncation flags.

Two tests hold it, because an argument is not a proof:

* **Pruning off must equal pruning on**, over every generated fragment in the differential
  proptest. A pruner that lies fails here immediately.
* **Pruning must actually happen**: a corpus and a fragment where the stripes read are provably
  fewer than the stripes present, so a pruner that silently stopped pruning is caught too.

## Files

```text
crates/esker-columnar/src/
    fragment/mod.rs     Fragment, Output, Aggregate — the type and its validation
    fragment/expr.rs    Expr, CompareOp, Literal, and pg_cmp over this crate's values
    fragment/codec.rs   encode / decode, the depth limit, the CRC
    scan.rs             evaluate(): prune, decode, filter, aggregate
    scan/group.rs       GroupKey, Partial, and how partials combine
    reader.rs           + ScanStats: stripes considered, stripes read, chunks decoded
    stats.rs            (amended) bounds in pg_cmp order
crates/esker-columnar/tests/
    fragment_golden.rs  frozen fragment bytes, and every way one is refused
    differential.rs     the reference interpreter, and the generated-fragment proptest
    scan.rs             pruning on == pruning off, pruning happens, projection is respected
    fuzz_decode.rs      (extended) fragments into mutated files
```

## Tests

| Kind | What it holds |
|---|---|
| Fragment golden | frozen bytes for a realistic fragment; a change is a format change |
| Refusal | every unknown tag, every out-of-range slot, a bounded key range, `sum(text)` — each refusing the whole fragment |
| Differential, hand-written | the nasty corpus: NULLs everywhere, `NaN`, `-0.0`, both infinities, `i64::MIN`/`MAX`, empty strings, groups that come out empty |
| Differential, generated | random projections, filters and groupings over a seeded mixed corpus, columnar against the reference, **including the error case** — both sides must fail the same way |
| Pruning soundness | pruning on equals pruning off, over the same generated fragments |
| Pruning efficacy | a stripe that is provably skipped |
| Projection | untouched columns are never decoded, by counting decoder invocations |
| Fuzz | arbitrary bytes into the fragment decoder; fragments against mutated files |

## Risks

* **The two engines disagreeing is the failure this phase exists to prevent**, and a harness that
  shares code with what it checks does not prevent it. The reference shares exactly one thing with
  the evaluator: `pg_cmp`, which is the *specification* and not an implementation. Everything
  else — scanning, pruning, decoding, grouping, accumulating — is written twice.
* **Floating point in the harness.** Comparing sums with `==` would pass on `NaN` by accident and
  fail on `-0.0` by accident. The harness compares by bits, as M1's `Column::identical` does.
* **A generated fragment that is always refused proves nothing.** The generator draws from the
  file's own schema so that most fragments are evaluable, and the proptest asserts a floor on how
  many actually ran.
* **The depth limit is a stack overflow if it is wrong.** It is enforced in the decoder, where the
  recursion starts, and the fuzz feeds deliberately nested bytes.

## Not doing, restated

No wire service, no `esker-proto`, no planner routing, no learner feed, no tiering rewrite, no
MPP, no cross-file combination, no new dependency, and no edit outside
`crates/esker-columnar/**`, this file, `docs/bench/columnar-m2.md`, one new `docs/DESIGN.md`
section, and an amendment to ADR 0027.

## M2 — what changed while building

*(Filled in as the units land.)*
