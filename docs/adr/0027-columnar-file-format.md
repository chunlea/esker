# 0027 — The columnar file format

Date: 2026-08-31 · Status: accepted · Phase: 7 · Crate: `esker-columnar`

## Context

[ADR 0022](0022-columnar-learner-replica.md) decides *that* Esker gets a columnar copy of a table
and where the copy comes from. It deliberately does not decide what the bytes look like — it says
only that milestone 1 is "a single-node columnar file format. Writer, reader, per-type encodings,
statistics, goldens, proptests, a crash test and a decoder fuzz."

This ADR is that layout. It exists because `CLAUDE.md` requires one for every format change, and
because every choice below is one a future reader could reasonably reverse: each is a trade with a
losing side, and the losing side is what an ADR is for.

The constraints were fixed before the first byte: no new dependency (the budget is 36 of 40 and
this crate adds nothing), LZ4 as the only codec, CRC32C as the only checksum, and the six types
`esker_sql::value::ColumnType` carries as the whole type set.

## Decision 1: stripes, then chunks, then a footer, then a fixed trailer

```text
file   := stripe* ++ footer ++ trailer
stripe := chunk*                        one chunk per column, in schema order
chunk  := payload ++ codec:u8 ++ crc32c:u32
```

This is the Parquet/ORC shape and there is no interesting alternative. What is worth recording is
the **trailer**, which those formats do differently and which we spend 32 fixed bytes on:

```text
byte  0.. 8   footer_offset:  u64 LE
byte  8..12   footer_len:     u32 LE
byte 12..16   footer_crc32c:  u32 LE   over the footer payload
byte 16..20   trailer_crc32c: u32 LE   over bytes 0..16
byte 20..24   format_version: u32 LE
byte 24..32   magic:          the 8 ASCII bytes "ESKERCOL"
```

Fixed size, because finding it must be a seek and not a search — the same reasoning that fixes the
SST footer at 48 bytes (`esker_engine::sst::footer`). Magic last, because that is what a reader
meets first when it seeks backwards.

**The trailer checksums itself, and that is not redundant.** The footer's CRC lives in the
trailer, so a corrupt trailer means a `footer_offset` and a `footer_len` that no later check has
yet questioned. Believing them for one read is believing an attacker-chosen length; the extra four
bytes make the trailer self-verifying before any of its fields is used. It is the cheapest guard
in the format and it is the one that runs first.

## Decision 2: a file without a valid trailer is *unsealed*, not *corrupt*

The writer's order is: stripes, footer, trailer, `sync_data`, rename, `fsync` the directory. The
trailer is therefore the commit point (invariant 3), and any prefix of that sequence is a file a
crash left behind.

`esker_columnar::Error` has a variant for exactly that — `Unsealed` — separate from `Corruption`,
and the difference is operational rather than cosmetic. An unsealed file is *expected*: the right
response is to delete it and move on, and an alert on it would fire after every unclean shutdown
until somebody learned to ignore alerts. A sealed file whose bytes have since rotted is the
opposite: rare, never expected, and the thing an operator must hear about. One error type for both
teaches whoever is on call to ignore the wrong one.

The line between them is the trailing magic and nothing else. Magic absent — the file was never
finished. Magic present but a checksum failing — something wrote a complete trailer and the bytes
have changed since. That rule is decidable from eight bytes at a known offset, which is what makes
it usable during recovery rather than after a full scan.

## Decision 3: the null mask is above the value encoding, and values are dense

```text
payload := encoding:u8 ++ rows:varint ++ null_count:varint
             ++ [null mask, bitpacked or run-length]
             ++ values           only the rows that are not NULL
```

The alternative is a per-encoding null representation — a sentinel in the dictionary, a reserved
bit pattern in a packed integer. Every such scheme steals a value from the domain, and the domain
here includes `i64::MIN` and the empty string, both of which are real data.

Keeping the mask above means the value encodings never see a NULL, so each one is a total function
over its own type, which is what makes a per-encoding round-trip proptest meaningful. It also
makes a NULL cost one bit rather than a slot: a mostly-NULL column is a run-length mask and almost
no values, which is the common shape after `ALTER TABLE ADD COLUMN`.

The mask is itself a boolean encoding — bitpacked or run-length, whichever is smaller — so a
column with no NULLs pays two bytes for the count and the mask is absent entirely.

**The chunk repeats `rows` and `null_count`, which the footer already states.** That is deliberate
duplication: a decoder must not resolve a disagreement between two regions in favour of whichever
it read first. They disagreeing is corruption and is reported as such.

## Decision 4: the encoding is chosen by encoding it both ways and keeping the smaller

| Type | Candidates |
|---|---|
| `Int8`, `TimestampTz` | frame of reference; delta (zigzag, itself frame-of-reference packed) |
| `Text`, `Bytea` | plain (bit-packed lengths ++ bytes); dictionary (bit-packed codes) |
| `Bool` | bitpacked; run-length |
| `Double` | plain, and only plain |

The obvious alternative is a heuristic: dictionary below some cardinality ratio, delta when the
column looks sorted. Every such rule is a magic number tuned once against one workload by someone
who then leaves, and `CLAUDE.md`'s "no tuning before a profile" cuts both ways — a threshold
guessed in advance *is* tuning before a profile. Encoding twice costs a pass over data already in
cache and the writer is not the bottleneck this feature exists to fix.

The dictionary is skipped when distinct values exceed half the rows, which is not a tuning
parameter but a proof: above that ratio the codes alone cost more than the values they replace.

**Doubles get neither delta nor dictionary**, and this is the decision most likely to be revisited.
Delta on a float is lossy or pointless. A dictionary needs an equality over `f64` that answers for
`NaN` and `-0.0`, which is a second equality relation in a format whose statistics already need
one, and two of them will eventually disagree. LZ4 handles the repetitive case; if a profile says
that is not enough, adding an encoding is a version bump and a new golden, which this format is
built to absorb.

## Decision 5: statistics live in the footer, and say when they are approximate

`min`, `max` and `null_count` per column chunk, written into the footer's chunk entry rather than
into the chunk. Statistics inside the chunk would mean reading the chunk to find out whether to
read the chunk, which is not a pruner.

Two rules are not the obvious ones, and both are what Parquet learned the hard way:

* **`NaN` is not in the range.** It is excluded from both bounds, and a chunk of nothing but `NaN`
  has no bounds. A `NaN` in a comparison produces a bound that fails every test, and a pruner
  would then skip a stripe that contains matching rows — a missing row, which is the worst failure
  mode this feature has (ADR 0022 says so about the two engines disagreeing, and it is just as
  true within one).
* **Zero is signed and comparison is not.** A minimum of `0.0` is stored as `-0.0` and a maximum of
  `0.0` as `+0.0`, so the bound holds whether the reader compares numerically or bitwise.

Text and byte bounds are truncated to 64 bytes, and **a truncated bound says so** in its flags bit.
Without that bit, the first pruner to conclude `col = 'x'` from a bound that is merely *at or
below* the true minimum returns the wrong rows. The bit costs nothing today; adding it later would
cost a format version, and it would be added after the bug rather than before it.

## Consequences

* Two version numbers now exist for one table's bytes: `ROW_FORMAT_VERSION` and
  `COLUMNAR_FORMAT_VERSION`. They are independent on purpose — a columnar file is a derived
  artefact and rebuilding one is always available as a migration, which is not true of rows.
* The type tags are shared with `esker_sql::catalog::record` by *value*, not by linkage. A test in
  `esker_columnar::value` pins them; moving either side breaks it. Sharing a constant would mean
  the columnar crate depending on the SQL crate, which ADR 0022 explicitly refuses.
* Every encoding needs a round-trip proptest, every fixed layout a golden, and every decode entry
  point a place in the fuzz. That is ADR 0002's recurring cost, paid again here, and it is the
  reason this milestone is "worth its own phase".
* Nothing in this ADR settles compaction, the scan path or how a file is delivered. A columnar
  file is a file; ADR 0022's milestones 2 and 3 decide what writes it and what reads it.
