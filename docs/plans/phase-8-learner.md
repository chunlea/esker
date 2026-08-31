# Phase 8 — the columnar learner: the wire, the placement, the flag

[ADR 0022](../adr/0022-columnar-learner-replica.md) milestone **3**. Two lanes: `wy-c2` owns
everything that lets a columnar learner *exist and be spoken to* — the wire messages, the catalog
flag and its DDL, and PD placement. `cl-c1` owns the learner's insides. This file is written from
both ends: **§wire** and below is `wy-c2`'s, **§store** is `cl-c1`'s.

---

## §wire — the fragment service

### 0. What is already decided, and is not re-litigated here

The fragment *request payload* is a format that already exists, is versioned, and has goldens:
`esker_columnar::fragment::codec`, `FRAGMENT_FORMAT_VERSION = 1`. Milestone 2 built and fuzzed it
(402 million cases through the decoder). **This lane does not re-encode it and does not parse it.**

The *response* is the real gap. Milestone 2's report says so in as many words: the fragment
response is Rust types only — `FragmentResult { output, stats }` over `FragmentOutput::Rows` /
`Groups`, `Group`, `Partial` — and has no wire format at all. Defining one is unit 1's substance.

### 1. A service of its own: `Fragment`, `0x06`

Services `0x00`–`0x05` are taken (system, RawKv, TxnKv, Pd, Raft, Admin). The fragment gets
`SERVICE_FRAGMENT = 0x06` and one method, `FragmentEvaluate = 0x0601`.

Its own service rather than a seventh `TxnKv` method, for the reason `SERVICE_ADMIN` gives for
itself: *"they are not key-value work and must not be counted as it by anything watching request
rates."* A fragment is a plan evaluated on a node that may hold no voter at all. Anything counting
`TxnKv` rates, or routing on the service byte, must be able to tell the two apart without decoding
a body.

### 2. The request carries the fragment **opaquely**

```text
FragmentReq {
    fragment:         bytes,   // esker_columnar's format. Length-prefixed, never interpreted here.
    ts:               u64,     // ADR 0022 Decision 4, half one
    min_apply_index:  u64,     // ADR 0022 Decision 4, half two
}
```

**One definition of fragment bytes, and it is `esker-columnar`'s.** This crate carries them as a
length-prefixed byte string and has no opinion about their contents — the same treatment the lock
payload gets (`docs/adr/0016`, and `7ac5bb7` "the lock payload is opaque by decision, not for want
of a type"). A second decoder here would be a second definition of what a filter means, and two
nodes disagreeing about that is a wrong answer rather than a protocol error.

**`ts` and `min_apply_index` are two fields and neither is derived from the other.** Decision 4 has
two halves and they are different mechanisms: `ts` is MVCC visibility applied *during* evaluation,
`min_apply_index` is a catch-up bound satisfied by a `ReadIndex` round *before* it. A build that
inferred one from the other would answer from a state it had not reached.

**The epoch is not in the body.** `RequestHeader` already carries `{ region_id, epoch, peer }` on
every request, fragments included, so invariant 5 is satisfied by the field that is already there.
Adding a second epoch to the body would be a second source of truth about the same fact.

### 3. The response, which is the part that did not exist

```text
FragmentResp::Result  { result: bytes, stats: ScanStats }
FragmentResp::Refused { reason: RefusalReason, detail: string }
```

#### Refusal is a normal answer, never an error frame

ADR 0022 Decision 3's "refuse, never partially honour" rule crosses the wire as a **response
variant**. A node that does not implement an operator, or that cannot reach `min_apply_index`
inside its deadline, is answering correctly: the answer is *"fall back to a row scan"*, which is a
path the planner already has. An error frame would make every rolling upgrade look like a fault,
and would put a normal fallback on a caller's error path where retry logic and metrics live.

`RefusalReason` is an enum on the wire, not a string, because the planner branches on it:
`Unsupported` (this build cannot evaluate it — do not retry here), `TooFarBehind` (could not reach
the apply index — another replica may be closer), `NotColumnar` (this region has no columnar copy).
`detail` is for a human and is never matched on.

#### The result body is a format of its own

Version byte, hand-written little-endian body, CRC32C, golden — like everything else on this wire
(ADR 0002), and like the request it answers.

```text
result := version:u8=1 ++ kind:u8 ++ body ++ crc32c:u32     // crc covers version..body
kind    := 0 rows | 1 groups

rows   := ncols:varint ++ type:u8 * ncols
        ++ nrows:varint ++ (value * ncols) * nrows

groups := nkeys:varint ++ type:u8 * nkeys
        ++ naggs:varint ++ (agg_kind:u8 ++ agg_type:u8) * naggs
        ++ ngroups:varint ++ (value * nkeys ++ partial * naggs) * ngroups

value   := 0x00                      // NULL
         | type_tag:u8 ++ payload    // as esker_columnar::fragment::codec writes a literal
partial := count: varint | (sum|min|max): value
```

**Types are declared once in a header, not per value** — except for the null-or-type
tag each value already needs. The declaration is what makes a schema mismatch a typed error instead of a silent
misparse, and that distinction is one this project has already paid for: M2's format version 2
exists because *a checksum proves a block is intact, not that it is the block that was asked for.*
A result decoded against the wrong column list has exactly that shape — intact bytes, wrong
meaning — and a declared header turns it into a refusal to decode.

#### The type tags are copied, deliberately, and pinned

`1 Int8, 2 Text, 3 Bool, 4 Bytea, 5 TimestampTz, 6 Double`. These are already a frozen vocabulary
shared by two crates that do not link: `esker_sql::catalog::record`'s `TAG_*` constants are the
original, and `esker_columnar::value` copies them with a test named `tags_match_the_row_side`
saying so. This crate becomes the third, the same way, with the same kind of test.

Copied rather than linked because the alternative is worse in both directions: `esker-proto` cannot
depend on `esker-sql` (which sits above it) and must not depend on `esker-columnar` (a leaf whose
format versions would then be able to break the wire). The house answer to that is already on the
page — copy the constants and pin them with a test that names its source — and a third copy with a
third pinning test is consistent rather than novel.

#### `sum(double)` does not associate, and the response docs say so

Combining partial aggregates adds numbers in a different order than a single-level fold would, so a
`sum(double)` finished from many fragments may differ in its last bits from the same query run over
one. `esker_columnar::scan::group::Partial::combine` documents it at the point of combination; this
is the other place a reader meets it, because the wire is where "many fragments" becomes true. It
is inherent to two-level aggregation, not a defect, and it is the reason a fragment folds its own
answer in row order across every stripe rather than per stripe and combined.

#### `stats` is a typed field, not part of the format

`ScanStats` — five counters — rides beside the result rather than inside it, because it is not part
of the *answer* and a build that ignored it would still be correct. Carried now rather than at
milestone 4 because `EXPLAIN` is the named consumer and a field added later costs a version bump.

### 4. What this lane will **not** carry

**A schema.** Placement carries where a replica lives, not what a table looks like. A placement
operator naming a table's columns would make PD a carrier of SQL semantics, which is the exact line
`docs/plans/phase-6e.md` §10 drew when it took the schema-step drive away from PD on invariant 7 —
PD cannot read a table definition, let alone write one. Placement carries a table id and a desired
replica count: a number and an id.

The learner's decoder is therefore `cl-c1`'s seam and not a field of mine. The types it needs can
travel as data; the *codec* that walks a row's bytes cannot, and something below `esker-sql` has to
own it. That is an ADR-level question crossing both halves — recorded here as **OPEN**, and put to
the coordinator rather than settled between two lanes mid-flight.

---

## §store — the learner's insides

*Owned by `cl-c1`.*
