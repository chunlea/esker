# 0080 — the time-zone table is a dependency, the reader is ours

**Status**: accepted · **Date**: 2026-09-05

## Context

`SET TIME ZONE 'America/New_York'` is `0A000 the time zone "America/New_York"` on this node, and
so is every named zone that is not a spelling of UTC. `crates/esker-sql/src/parameter.rs` says why
in one sentence: **a `timestamptz` is printed in UTC and nowhere else**, so a zone honoured in
`SHOW` and ignored in every row would be a setting that lies. Refusing was the right answer while
there was no zone data; it is what has to change now.

What wants it: `connection_test.rb` and `timestamp_test.rb` one test each, and six statements
across the corpora that this node answers only because they name UTC. Beyond the suite it is the
whole of `AT TIME ZONE`, which is not lowered at all today.

A zone name is not a rule anybody can derive. `America/New_York` is a *history* — every transition
the United States has legislated since 1883, plus a forward rule for instants past the last
recorded one — and it changes several times a year as governments move their clocks. It is the one
kind of thing this project cannot write itself, and the human ruling on 2026-09-05 said so:

> the named time-zone table may come from a **dependency**, because it is only a data source.

**That ruling is about the table, and this ADR is about how far it reaches.** A TZif file is a
framed binary format — a four-byte magic, a version byte, six counts, then arrays of transition
times, type indices, `ttinfo` records, abbreviation bytes, leap seconds and standard/wall
indicators, followed in v2+ by the whole thing again in 64-bit and a POSIX `TZ` footer string.
CLAUDE.md's dependency policy names that shape exactly: *"varints and all on-disk and wire framing"*
is on the list of things written in-house with tests. So the data is bought and the reader is
written, which is the same division this project already makes for LZ4 (`lz4_flex` decodes a byte
format nobody here wants to own) and refuses for its own SST blocks.

## What was measured

`tests/captures/pg19_time_zone.txt`, in one rolled-back session on 19beta1. The rows that decide
how much of a TZif file has to be read:

```
SET TIME ZONE 'America/New_York'
  '2020-01-01 00:00:00+00'  ->  2019-12-31 19:00:00-05      standard
  '2020-07-01 00:00:00+00'  ->  2020-06-30 20:00:00-04      daylight
  '1880-01-01 00:00:00+00'  ->  1879-12-31 19:03:58-04:56:02  Local Mean Time, to the second
  '2099-07-01 00:00:00+00'  ->  2099-06-30 20:00:00-04      past the table, from the POSIX rule
SET TIME ZONE 'Asia/Kathmandu'
  '2020-01-01 00:00:00+00'  ->  2020-01-01 05:45:00+05:45   a quarter-hour offset
```

Three things follow, and none of them is a lookup table. **An offset is a signed number of
seconds**, not minutes — every instant before 1883 in New York is `-04:56:02`. **The transitions
run out**: TZif tables stop around 2037 and everything after comes from the footer's `TZ` string,
`EST5EDT,M3.2.0,M11.1.0`, whose month/week/weekday arithmetic is the half of the format that is a
parser. And **a local time can be missing or doubled** — `2020-03-08 02:30` does not exist and is
pushed forward, `2020-11-01 01:30` happens twice and takes the second — which neither raises nor
can be decided without the transition list.

Two more, measured because they would have been guessed wrong: `SET TIME ZONE 'EST'` is
**refused** where `'EST5EDT'` is accepted (abbreviations are a different table from zone names,
195 rows against 487), and `SET TIME ZONE -5` is read back as `<-05>+05`, a POSIX string with the
sign inverted.

## Options

Each measured against `deny.toml`: no `*-sys`, no `cc`, no build script that compiles anything, no
`serde`, `multiple-versions = "deny"`, and a licence on the allow list.

| option | runtime crates added | build script | licence | verdict |
|---|---|---|---|---|
| `chrono-tz` | **five at least** — `chrono-tz`, `chrono`, `num-traits`, `phf`, `phf_shared`; more if `chrono`'s default `clock` feature pulls `iana-time-zone`, which was not measured because the build script settles it | **yes**, and `num-traits` has one too | MIT/Apache-2.0 | rejected |
| `tzdb` 0.7.3 | **3** — `tzdb`, `tz-rs`, `tzdb_data` | no | Apache-2.0 | rejected on size |
| `tz-rs` 0.7.3 + `jiff-tzdb` 0.1.8 | **2**, both with zero dependencies of their own | no | MIT/Apache-2.0, Unlicense/MIT | the fallback |
| **`jiff-tzdb` 0.1.8 alone, with our own TZif reader** | **1**, zero dependencies | no | Unlicense **OR** MIT | **chosen** |
| vendor the compiled tzdata into the repository | 0 | no | — | rejected: see *updates* below |

`chrono-tz` is out twice over. It brings `chrono` — a date-time library this node does not need,
having written its own civil-date arithmetic — and it *generates* its tables in a build script,
which is the exact shape `deny.toml`'s preamble says has to be looked at by a human rather than
discovered in a profile six months later.

`tzdb` is the smallest ready-made *answer*: `tzdb::tz_by_name(b"America/New_York")` hands back a
`tz::TimeZone` and nothing else is written. It costs three crates where one buys the same data.

Vendoring the tables in-tree costs no dependency at all and was rejected on the update path rather
than on the size: a checked-in copy of tzdata is a file nobody has a reason to look at again, and
the first time a government moves a clock it is silently a year stale. A version in `Cargo.toml` is
a thing `cargo update` and a human both know how to read.

## Decision

Take **`jiff-tzdb`** as a data-only dependency and write the TZif reader in `esker-sql`.

`jiff-tzdb` embeds the IANA database as TZif bytes and exposes three items and no more:
`get(name) -> Option<(&'static str, &'static [u8])>`, `available()`, and a `VERSION` static naming
the IANA release. It has **no dependencies of its own**, no build script, and its licence is
`Unlicense OR MIT` — cargo-deny reads the `OR` and MIT is on the allow list.

The reader is ours: RFC 8536, v1 through v3, the 64-bit block preferred when present, and the
POSIX `TZ` footer for instants past the last transition — which is the half that is a *parser* and
not a table lookup, and the half a bought crate would have hidden.

### The budget arithmetic

`deny.toml` records `esker-dep-budget = 37`, and `crates/esker-cli/tests/dep_budget.rs` counts the
transitive runtime graph, dev-dependencies excluded. Run at this commit:

```
runtime transitive crates: 33 of 37 allowed
```

**Thirty-three, so the headroom is four** — and `deny.toml`'s own preamble, which says the test
"reports 36", is stale by three; the explanation beside that number still holds, since
`cargo tree -e normal --workspace` puts the same graph at 30 and the walk counts optional edges it
does not compile. The stale figure is corrected in the same commit as this ADR.

**So the budget is not what decides this.** Four crates of headroom would pay for `tz-rs` beside
`jiff-tzdb` (35 of 37) or even for `tzdb` (36 of 37), and neither would need the budget raised.
What decides it is the policy: CLAUDE.md puts "all on-disk and wire framing" on the list of things
this project writes with tests, TZif is on-disk framing, and headroom is not a reason to spend
headroom. If the reader turns out to be materially harder than RFC 8536 reads — the POSIX footer is
the candidate — `tz-rs` is one line of `Cargo.toml` away and this ADR is amended rather than
reversed.

The `tls` feature's graph is not measured by that test and is not affected here — `jiff-tzdb` is
not optional and is in the default set.

## Consequences

* **A zone name resolves without asking the operating system.** No `/usr/share/zoneinfo`, no
  `iana-time-zone`, nothing that behaves differently in a scratch container than on a developer's
  Mac. This matters more than it sounds: a serverless database that reads the host's zone files is
  a database whose answers depend on its base image.
* **Binary size, and it is not yet measured.** The published crate is **62 066 bytes** compressed;
  what the linker actually adds is the uncompressed TZif for every zone, which is larger by an
  unknown factor. The before-and-after goes in `docs/bench/` with the unit, because a number
  guessed here would be the third floating-point-shaped claim this lane has had to retract.
* **Updates arrive as a version bump.** `jiff-tzdb` is published per IANA release and `VERSION`
  names it, so `SHOW` can report which tzdata this node carries and a stale one is visible rather
  than silent.
* **The reader is code we own and must test.** TZif is where a wrong answer is a *time*, so it gets
  the same treatment as every other format in this repository: a golden file, a round-trip over
  every zone in the database, and the transitions of a handful of zones checked against
  PostgreSQL 19 itself rather than against the TZif bytes — the oracle is the point (ADR 0075).
* **`AT TIME ZONE` becomes reachable**, and is a separate unit: this ADR buys the table, not the
  operator.
* **If `jiff-tzdb` is ever withdrawn**, the same bytes can be vendored under the option this ADR
  rejected, and only the update path changes. That is the whole exposure of depending on a crate
  whose README calls it an internal component of another library.

## What this does not decide

Whether `timestamptz` starts *storing* anything other than a UTC instant. It does not: the
representation is unchanged and always UTC, and a zone is applied when a value is rendered or when
`AT TIME ZONE` converts one. Nothing on disk moves.
