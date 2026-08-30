# Phase 2 — A single-node server, client, and the RawKV API

Turn the engine into a network service so the rest of the system has a stable surface to grow behind.
Nothing distributed yet: one process, one store, one region covering everything, but every request
already carries the `{ region_id, epoch, peer }` header from `docs/DESIGN.md` §9 and every error is the
typed enum — the multi-node phases must not need to change the API.

## Deliverables

1. `esker-proto`: the hand-rolled framing from DESIGN.md §9 (`frame.rs`: length + crc32c + kind +
   request id; `codec.rs`: one `encode`/`decode` pair per message with varints and length-prefixed
   bytes; `WIRE_VERSION` negotiated on connect); a `tokio` TCP transport (`transport.rs`) with a
   per-connection writer task, a `request_id` demultiplexer, ping/pong keepalive, and streaming frames;
   typed error enum with redirect hints; `region_id`/epoch on every KV request. Tests: golden frames,
   proptest round trips, a fuzz-style test feeding random bytes to the decoder (must never panic),
   and a loopback test with 1,000 concurrent in-flight requests.
2. `esker-store` (single-region mode): opens an engine with the four built-in CFs, serves `RawKv`
   (Get, BatchGet, Put, BatchPut, Delete, DeleteRange — v1 limitation from DESIGN.md §4.7 applies —
   Scan with limit and reverse, CompareAndSwap). Keys are namespaced under `'r'` by the store, never by
   the client. A `RegionMeta` struct with epoch checks even though there is only one region.
3. `esker-client`: `RawClient` with a region cache that today has one entry; all retry/backoff logic is
   written now (bounded retries on `NotLeader`/`EpochNotMatch`/`ServerIsBusy`) and unit-tested with a
   fake transport that returns those errors.
4. `esker-cli`: `server --data-dir --listen`, `raw get/put/delete/scan`, and `bench` gains a
   `--remote` mode that drives the network API (same workloads as phase 1).
5. Optional TTL for RawKV keys implemented as a `CompactionFilter` plus a read-time check, with an ADR
   on the expiry semantics (expired keys are invisible immediately, physically removed at compaction).

## Tests

- Integration tests spawning the server in-process on a random port over the real TCP transport.
- The phase-1 model test re-run **through the client** (a `Store` trait implemented by both the engine
  and the client so the same proptest drives both).
- Crash loop re-run through the client with `sync = true` writes.
- Load test: 64 concurrent clients for 60 s; assert no error other than `ServerIsBusy`, and that
  `ServerIsBusy` corresponds to a real engine stall (metric) rather than a bug.

## Acceptance

`esker-cli server` + `esker-cli raw put/get` work end-to-end; bench `--remote` numbers recorded in
`docs/bench/phase-2.md` next to the phase-1 in-process numbers (the gap is the network + framing cost;
explain it in one paragraph); DESIGN.md §9–10 match the code.
