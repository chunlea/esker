# 0004 — A separate `esker-base` crate for shared primitives

Date: 2026-08-30 · Status: accepted · Phase: 0

## Context

`prompts/00-scaffold.md` asks for four in-house primitives — `crc32c`, varints, a `hash64` for
cache keys, and a seeded PCG32 — "in `esker-keys` (or a tiny `esker-base` if cleaner)".

Who actually uses them decides the answer:

| Primitive | Used by |
|---|---|
| `crc32c` | `esker-engine` (WAL records, SST blocks), `esker-proto` (frames), `esker-store` (snapshot chunks) |
| varints | `esker-engine` (WAL and SST entries), `esker-proto` (message fields) |
| `hash64` | `esker-engine` (block cache, bloom filters) |
| `Pcg32` | `esker-sim`, `esker-raft` (randomised election timeouts), test code everywhere |

Not one of them is used only by `esker-keys`.

## Decision

An eleventh crate, `esker-base`, holding `crc32c`, `varint`, `hash` and `rng`. It depends on
nothing but `thiserror` and knows nothing about keys.

Putting them in `esker-keys` would force `esker-engine` to depend on `esker-keys`, and
`CLAUDE.md` invariant 7 says the opposite: "Engine and Raft are byte-opaque. Key semantics
(tenant, table, MVCC suffix) live only in `esker-keys` and above." A dependency edge from the
engine up to the key layer would make that invariant a convention enforced by nothing —
someone would eventually reach for `esker_keys::codec` from inside a compaction filter, and the
compiler would allow it.

The alternative of duplicating the primitives per crate was rejected outright: two CRC
implementations that drift is a corrupt database.

`docs/DESIGN.md` §4.5 names the checksum `esker-engine::crc32c`. Rather than let the code and
the design document drift — which `CLAUDE.md` forbids — `esker-engine` re-exports it, so that
path resolves and there is a test asserting it does.

## Consequences

* Eleven crates instead of ten. The prompt allows this explicitly, and the layering table in
  `CLAUDE.md` gains one row below `esker-keys`.
* `esker-base` is the bottom of the dependency graph. Nothing may depend upwards from it, which
  is what keeps it free of key, engine and protocol semantics.
* Its byte layouts (varint encoding, CRC polynomial) are part of on-disk formats, so changing
  one is a format change under ADR 0002 even though the crate looks like a utility library.
* `esker-base` carries `unsafe_code = "deny"` like every crate except `esker-engine`. The two
  CRC32C intrinsic call sites relax it with a targeted `#[allow]` plus a `// SAFETY:` comment
  naming the `cfg` that proves the precondition, and a test compares the hardware path against
  the portable one — which is what `CLAUDE.md` invariant 8 asks for.
