# A declared divergence must cite its measurement

**Status**: queued. One unit, one commit, after run 88's `relation "…" already exists` read.

## The hole

A parity corpus row carries PostgreSQL's answer, and the harness compares this node against it on
every run — so a wrong expected value fails immediately and loudly. **Except for a declared
divergence.** There the harness checks only that the two still *differ*: rule 1 fails an unlisted
divergence, rule 2 fails a listed one that started agreeing, and neither ever asks whether the
recorded PostgreSQL answer was ever true.

So a divergence row's expected value is the one number in this repository that nothing verifies.

It is not hypothetical. `pg19_concat.txt` carried

    SELECT 'r', '{"a":1}'::json || '{"b":2}'::json    r|{"a":1}{"b":2}

declared as "json is a Datum::Text here, so || concatenates the documents". PostgreSQL answers
`42883 operator does not exist: json || json` — there is no `||` for `json` at all. I wrote that
expected value from memory in the same edit that declared it a divergence, so from that moment
nothing could have caught it. `b4979f0a` measured it and corrected it.

## The rule

A declared divergence carries a **provenance**: either

* `captures/<file>:<line>` — the capture and line the PostgreSQL answer was read from, or
* the literal `UNMEASURED` — a declaration that nobody has measured this, which is a fact worth
  recording and not an omission.

The harness **rejects a divergence with neither**, and **resolves** a citation: the cited file and
line must exist and the statement on it must be the row's.

That last property is the one that matters. Filling 261 citations in once is bookkeeping; a harness
that re-reads them every run is what keeps them true, because a capture that is re-captured, or a
row that moves, breaks the citation exactly the way a stale expected value should break the build.

## The sweep, as of `2283fbbe`

| | |
|---|---|
| declared-divergence rows | **446** across 96 test files |
| appear verbatim in a `captures/` file | **261** |
| do not | **185** |

**185 is the number this plan exists to shrink, and it may not grow.** A unit that adds a
divergence adds a capture with it, or writes `UNMEASURED` where a reader can count it.

Not finding a capture is not evidence of fabrication: most of the 185 were measured by a lane in an
ad-hoc `psql` session and never filed, so the measurement happened and cannot be checked. That is
the same defect from the reader's side, which is why they are one list.

The full list is `scratchpad/divergence-provenance-sweep.txt`; the concentrations are
`unknown_literal.rs` (18), `aggregate_type.rs` (10), `enum_value.rs` (9), then
`pg_catalog_information.rs`, `scalar_functions.rs` and `numeric_arithmetic.rs` (7 each).

## The unit

**One commit**, and explicitly **not** 446 hand annotations:

1. The provenance field on a divergence row, and the harness rule — reject a row with neither form,
   resolve a `captures/...` citation against the file and line.
2. The **261** filled in by the same matcher this sweep used, not by hand.
3. The **185** written `UNMEASURED`, with the count asserted in a test so the list cannot quietly
   grow — the same shape `deny.toml`'s dependency budget uses.
4. Three rows measured in this session seed the capture format, because they are exactly the case
   the rule is about — measured against 19beta1 in a rolled-back session and never filed:
   `1 || 2`'s `42883` (the int8-literal trade), the array `||` row, and `pg_typeof` over a merge.
   They become a real capture in this unit.

## What this does not do

It does not check that a capture is *right* — a capture is a recording of a real server and is
trusted. It checks that an expected value can be traced to one. The failure it removes is a number
written from memory, which is the only one that can survive a green suite indefinitely.
