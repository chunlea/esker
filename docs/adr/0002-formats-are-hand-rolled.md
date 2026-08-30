# 0002 — On-disk and wire formats are hand-rolled, never serde

Date: 2026-08-30 · Status: accepted · Phase: 0

## Context

Esker has a lot of byte layouts: WAL records, SST blocks and footers, the manifest, Raft log
entries, region metadata, RPC frames, and every message inside them. Something has to turn
structures into bytes and back.

The obvious answer is `serde` with `bincode` or `postcard`, or `prost` for the wire. All three
are good libraries. The question is whether they are right for bytes that must be readable by a
different build of this program, years from now, after a crash.

## Options considered

1. **`serde` + a binary format** for disk and wire.
2. **`prost`/`tonic`** — protobuf schemas, generated code, gRPC.
3. **Hand-written `encode`/`decode` pairs**, one per type, with explicit framing and golden
   tests.

## Decision

Every on-disk and on-wire layout is written by hand, documented in `docs/DESIGN.md`, and pinned
by golden test vectors. `serde`, `prost` and `tonic` are banned from the dependency graph.

The reasons are specific, and none of them is "dependencies are bad".

**A derive makes the format implicit.** With `#[derive(Serialize)]`, the byte layout is a
consequence of field order and of the encoder's version. Reordering two fields is a
one-character diff that silently changes what is on disk. Adding a field changes it too. The
compiler will not object, review will not notice, and the failure appears as a corrupt database
on somebody's machine. When encoding is a function you can read, changing the format is a
change to that function — visible in the diff, and next to the golden test that will fail.

**Corruption handling is the actual requirement.** `CLAUDE.md` invariant 2 says corruption is
returned as an error, never a panic and never a silent skip; invariant 9 says nothing panics on
on-disk data. A generic decoder is built to reject malformed input, but the interesting cases
here are specific: a length prefix that would allocate a gigabyte, a CRC that does not match, a
record header that is valid but comes from another position in the file, a torn tail on the
last WAL segment that is *expected* rather than corrupt. Those decisions belong in code that
knows what it is reading.

**Forward compatibility should be explicit.** `docs/DESIGN.md` §9 makes unknown methods and
unknown fields errors, negotiated by `WIRE_VERSION` on connect. Protobuf's model — unknown
fields are skipped, everything is optional — is a good default for loosely coupled services and
the wrong one for a storage cluster, where a node that silently ignores a field it does not
understand is a node that has silently disagreed with its peers.

**The dependency policy would reject them anyway.** `serde` and `prost` bring proc-macro trees
and, for `tonic`, an HTTP/2 stack; the budget in `deny.toml` is 40 crates.

**And it is part of what this project is for.** Varints, framing and checksums are a few
hundred lines. Writing them is not the cost of this decision, it is some of its value.

## Consequences

* Every message type needs an `encode`/`decode` pair with round-trip and malformed-input tests.
  This is real, recurring work, and the largest cost of the decision.
* Every fixed format has golden vectors checked into the repository
  (`crates/esker-keys/tests/golden/keys.txt` is the first). A golden file changing is a format
  change: it needs an ADR and a format version bump, never a re-blessed file.
* Every format carries a magic number and a version, so an old build meeting new bytes says so
  instead of guessing.
* Refactoring a struct no longer risks the format, because the format is not derived from the
  struct.
* `serde` may still be used in places that are neither disk nor wire — a human-facing config
  file, say — but only by ADR, since it is currently banned outright in `deny.toml`.
