# Phase 2 plan — a single-node server, the wire, and the client

Status: **in progress**. Written before implementation; §8 records progress and §9 what changed.
Spec: `prompts/02-single-node-server.md`. Constitution: `CLAUDE.md`. Design: `docs/DESIGN.md`
§2, §9, §10, §11, §14.

Phase 1 (the engine) is done and accepted. This phase puts a network in front of it: hand-rolled
framing over TCP, a store that owns one region covering the whole key space, a client that already
has every retry path the multi-node phases will need, and the CLI commands that drive both.

Nothing here is distributed. The point of the phase is that **the API does not have to change when
it becomes distributed**: every request already carries `{ region_id, epoch, peer }`, every error is
already the typed enum with redirect hints, and the store already refuses a request whose epoch or
key range does not match — even though today there is exactly one region and its epoch never moves.

## 1. Scope, and the two lanes

Two lanes build it in parallel. `esker-proto` is the **shared contract** and has exactly one
writer; §3 of this document is what the other lane codes against until the real thing lands, which
it does in the proto lane's first commits.

| Lane | Owns | Steps |
|---|---|---|
| `wy-p2-proto` | `crates/esker-proto/**`, `crates/esker-store/**`, the `server` subcommand in `crates/esker-cli/src/**`, this plan, ADRs from 0006 | 0, 1, 2, 3, 4, 5, 6 |
| `cl-p2-client` | `crates/esker-client/**`, the `raw` subcommands and `bench --remote` in `crates/esker-cli/src/**` | 7, 8, 9 |

`crates/esker-cli/src/{main,args}.rs` and `crates/esker-cli/Cargo.toml` are touched by **both**
lanes. Keep the edits additive and local — one `Command` variant, one `parse_*` function, one
module, one block of `USAGE` lines each — and expect to resolve a small conflict rather than a
structural one.

Phase 1's crates (`esker-engine`, `esker-keys`, `esker-base`) are frozen to both lanes. A needed
change there is a report line to the coordinator, not an edit.

### Steps

| # | Step | Lane |
|---|---|---|
| 0 | This plan | proto |
| 1 | `esker-proto` framing: `frame.rs`, `codec.rs` — bytes, CRC, size limits, partial reads | proto |
| 2 | `esker-proto` messages: `messages.rs`, `error.rs` — Hello, RawKv, the typed error enum | proto |
| 3 | `esker-proto` transport: `transport.rs` — tokio TCP, writer task, demux, keepalive, streams | proto |
| 4 | `esker-store`: the engine, the four CFs, the region, the RawKv handlers | proto |
| 5 | `esker-cli server --data-dir --listen`, graceful shutdown | proto |
| 6 | Integration tests over real TCP on an ephemeral port | proto |
| 7 | `esker-client`: `RawClient`, region cache, bounded retry/backoff, fake-transport unit tests | client |
| 8 | `esker-cli raw get/put/delete/scan` | client |
| 9 | `bench --remote`, and `docs/bench/phase-2.md` next to the phase-1 numbers | client |

The client lane does not wait for the proto lane. Steps 1–3 land as separate commits in that order,
and the shapes in §3 are stable from the start; until they land, the client lane writes against §3
with a `FakeTransport` of its own, which it needs anyway for step 7's retry tests.

## 2. File list

```
crates/esker-proto/Cargo.toml           tokio (net/rt/io-util/sync/time/macros/signal), bytes,
                                        thiserror, tracing, esker-base
crates/esker-proto/src/lib.rs           module tree, WIRE_VERSION, frame constants   (contract)
crates/esker-proto/src/error.rs         ProtoError + its wire encoding               (contract)
crates/esker-proto/src/codec.rs         Encoder/Decoder: varints, length-prefixed bytes
crates/esker-proto/src/frame.rs         Frame, FrameKind, FrameDecoder (partial reads)
crates/esker-proto/src/messages.rs      Method, RequestHeader, Region, Hello, RawKv  (contract)
crates/esker-proto/src/transport.rs     Transport trait, TcpTransport, Server, ChunkStream
crates/esker-proto/tests/**             goldens, proptests, the fuzz decoder, loopback
crates/esker-store/Cargo.toml           esker-proto, esker-engine, esker-keys, tokio, thiserror
crates/esker-store/src/lib.rs           module tree; the phase-0 constants stay
crates/esker-store/src/error.rs         StoreError, and how each variant reaches the wire
crates/esker-store/src/region.rs        RegionMeta: the epoch and key-range checks
crates/esker-store/src/rawkv.rs         the eight RawKv handlers over the engine
crates/esker-store/src/server.rs        Store::open, the Service impl, spawn/shutdown
crates/esker-store/tests/**             real-TCP integration tests
crates/esker-cli/src/server.rs          the `server` subcommand
docs/adr/0006-*.md                      RawKV DeleteRange without range tombstones
```

## 3. The contract

**Pinned.** The client lane codes against exactly this; a change to any of it is a message to the
coordinator, never a unilateral edit. Everything marked *fixed* is a byte format with a golden test.

### 3.1 Framing (*fixed*, version 1) — `docs/DESIGN.md` §9

```text
frame = len:u32 LE ++ crc32c:u32 LE ++ kind:u8 ++ request_id:u64 LE ++ body
len   = the bytes after the len field itself: 13 + body.len()
crc   = crc32c(kind ++ request_id LE ++ body)          -- esker_base::crc32c
body  = tag:u16 LE ++ hand-encoded fields              -- varints, length-prefixed bytes
```

`tag` is the **method** in a Request or a Response and the **error code** in an Error frame; Ping,
Pong, Stream and StreamEnd carry no tag (a stream chunk's body is the chunk).

The CRC covers `kind ++ request_id ++ body` and not the length: a flipped `request_id` whose body
still checksummed would acknowledge the wrong caller, which is the one framing bug that cannot be
detected anywhere else.

| Constant | Value |
|---|---|
| `WIRE_VERSION` | `1u32` |
| `FRAME_HEADER_SIZE` | 17 = 4 + 4 + 1 + 8 |
| `MAX_FRAME_SIZE` | 16 MiB, *default*, the whole frame including `len` |
| `MAX_BODY_SIZE` | `MAX_FRAME_SIZE - FRAME_HEADER_SIZE` |

Frame kinds keep the phase-0 numbering, which is **1-based**: `Request=1, Response=2, Stream=3,
StreamEnd=4, Error=5, Ping=6, Pong=7`. Zero is reserved and never valid, exactly as in the WAL
record header (`docs/DESIGN.md` §4.3), so a run of zero bytes cannot be read as a frame before its
checksum is even reached. See §9.1.

### 3.2 Methods (*fixed*)

`tag` in a Request or Response body. High byte is the service, low byte the method, so a service's
methods stay contiguous and an unknown one is rejected rather than guessed.

| Service | Methods |
|---|---|
| `0x00` system | `0x0001` Hello |
| `0x01` RawKv | `0x0101` Get · `0x0102` BatchGet · `0x0103` Put · `0x0104` BatchPut · `0x0105` Delete · `0x0106` DeleteRange · `0x0107` Scan · `0x0108` CompareAndSwap |
| `0x02` TxnKv | reserved — phase 5 |
| `0x03` Pd | reserved — phase 4 |
| `0x04` RaftTransport | reserved — phase 3 |

An unknown method is `ProtoError::InvalidRequest`, never a skipped frame. So are trailing bytes
after a message decodes: a body that is longer than its fields is a different message.

### 3.3 Error codes (*fixed*)

```rust
pub enum ProtoError {
    NotLeader { region_id: u64, leader_hint: Option<u64> },     // 1
    EpochNotMatch { current_regions: Vec<Region> },             // 2
    KeyNotInRegion { key: Bytes, region_id: u64,
                     start_key: Bytes, end_key: Bytes },        // 3
    ServerIsBusy { reason: String },                            // 4
    Locked { lock_info: Bytes },                                // 5  reserved, phase 5
    RegionNotFound { region_id: u64 },                          // 6
    WireVersion { expected: u32, actual: u32 },                 // 7
    InvalidRequest { detail: String },                          // 8
    Unsupported { detail: String },                             // 9
    Corrupt { context: String, detail: String },                // 10
    Io { detail: String },                                      // 11
    Closed { detail: String },                                  // 12  sent, no answer
    DuplicateRequestId { request_id: u64 },                     // 13
    Internal { detail: String },                                // 14
    NotSent { detail: String },                                 // 15  provably never sent
}
```

Every variant encodes and decodes, including the ones a server never sends (`Corrupt`, `Io`,
`Closed`, `NotSent` are usually raised locally), because a round trip that is only exercised for
some variants is a golden test with holes in it.

Two predicates live on the error rather than in the client, so that the two sides cannot come to
disagree about them:

* **`is_retryable()`** — the server said "try again, or try elsewhere": `NotLeader`,
  `EpochNotMatch`, `ServerIsBusy`, `RegionNotFound`.
* **`outcome() -> RequestOutcome::{NotApplied, Unknown}`** — whether the request may have taken
  effect, which is what decides whether a *write* may be sent again. Every error the peer sent is
  `NotApplied`, because sending it is how the peer says it did not serve the request; so is
  `NotSent`, which means the request provably never reached the wire. `Unknown` is exactly
  `Closed`, `Io`, `Corrupt` and `Internal` — the failures that happened *around* the answer rather
  than in it. A transport that cannot tell must say `Unknown`: that costs a failed call, where the
  other direction costs a silently duplicated write.

### 3.4 Rust surface

```rust
// Dyn-compatible on purpose: the client keeps one per store behind an Arc, and a fake
// transport is what the retry tests drive. `async fn` in trait would forbid both.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Transport: Send + Sync + Debug {
    fn call(&self, request: Request) -> BoxFuture<'_, Result<Response, ProtoError>>;
}

pub struct Epoch { pub conf_ver: u64, pub version: u64 }
pub struct Peer { pub store_id: u64, pub peer_id: u64, pub role: PeerRole }
pub struct Region {                    // docs/DESIGN.md §6, epoch included
    pub id: u64, pub start_key: Bytes, pub end_key: Bytes,
    pub peers: Vec<Peer>, pub epoch: Epoch,
}
pub struct RequestHeader { pub region_id: u64, pub epoch: Epoch, pub peer: u64 }

pub enum Request  { Hello(Hello), RawKv { header: RequestHeader, request: RawKvReq } }
pub enum Response { Hello(HelloAck), RawKv(RawKvResp) }

pub enum RawKvReq {
    Get { key: Bytes },
    BatchGet { keys: Vec<Bytes> },
    Put { key: Bytes, value: Bytes, sync: bool },
    BatchPut { pairs: Vec<(Bytes, Bytes)>, sync: bool },
    Delete { key: Bytes, sync: bool },
    DeleteRange { start: Bytes, end: Bytes, sync: bool },
    Scan { start: Bytes, end: Bytes, limit: u32, reverse: bool },
    CompareAndSwap { key: Bytes, expected: Option<Bytes>, value: Option<Bytes>, sync: bool },
}
pub enum RawKvResp {
    Get { value: Option<Bytes> },
    BatchGet { values: Vec<Option<Bytes>> },
    Put, BatchPut, Delete,
    DeleteRange { deleted: u64 },
    Scan { pairs: Vec<(Bytes, Bytes)> },
    CompareAndSwap { swapped: bool, previous: Option<Bytes> },
}
```

Constructors — `RawKvReq::put(key, value)`, `delete(key)`, `delete_range(start, end)`,
`compare_and_swap(..)` — set **`sync: true`**. `CLAUDE.md` invariant 1 makes the un-durable
acknowledgement the thing a caller opts into, so `sync` is explicit on the wire and defaults to
durable in every constructor. `.unsynced()` is the opt-out.

`Scan` with an empty `end` runs to the end of the region. `reverse` walks from `end` down towards
`start`; `limit` is a hard cap and 0 means "the server's maximum".

### 3.5 What the client can rely on from the transport

```rust
impl TcpTransport {
    pub async fn connect(addr: SocketAddr) -> Result<Self, ProtoError>;          // does the Hello
    pub async fn connect_with(addr: SocketAddr, config: TransportConfig) -> Result<Self, ProtoError>;
    pub fn call_with_id(&self, id: u64, request: Request) -> BoxFuture<'_, Result<Response, ProtoError>>;
    pub async fn call_stream(&self, request: Request) -> Result<StreamResponse, ProtoError>;
    pub fn is_closed(&self) -> bool;
}
```

* `request_id` is **per-connection and client-assigned**. `call` allocates from a counter;
  `call_with_id` lets a caller choose. A duplicate id that is still in flight is
  `DuplicateRequestId` — silently replacing the waiter would leak the first caller for ever.
* Version negotiation happens once, inside `connect`. A mismatch fails there with `WireVersion`.
* More than `max_in_flight` outstanding requests on one connection is `ServerIsBusy`, on both
  sides. Bounded channels everywhere; no queue in this crate is unbounded.
* A connection that goes quiet for `keepalive_interval` is pinged, and one silent for
  `idle_timeout` is dropped with every waiter failed as `Closed`. Nothing hangs for ever.

### 3.6 Answers to the client lane's contract note

The client lane raised five points against the shapes above before either crate landed. What was
adopted, and what was not:

1. **"Provably never sent" vs "sent, no answer" — adopted, and it is now part of `ProtoError`.**
   `NotSent` (code 15) and `Closed` (code 12) are the pair, and `outcome()` is the predicate; see
   §3.3. This was the right thing to ask for: without it a client cannot tell a write it may
   safely repeat from one it may not, and `esker-txn` will need the same distinction for
   `Prewrite` in phase 5.
2. **A synchronous `call` — provided as a wrapper, not as the trait.** The async `Transport` of
   §3.4 stays, because it is what the store's connection tasks and phase 4's Raft transport are
   written against. Alongside it, `BlockingTransport` wraps one and exposes
   `fn call(&self, request: Request, deadline: Instant) -> Result<Response, ProtoError>`, which is
   what a CLI thread and `bench --remote` want. A per-call **deadline** is adopted on both.
   **Addressing stays an address, not a `store_id`.** One `TcpTransport` is one connection to one
   peer, which is what `docs/DESIGN.md` §6 describes and what phase 4 needs per (store, store)
   pair. Mapping `store_id → address` is routing, which `docs/DESIGN.md` §10 puts in the client
   and §7 puts in PD; a table inside `esker-proto` would put routing below the wire.
3. **Names follow the brief, not the transcription.** `Epoch` (not `RegionEpoch`),
   `RequestHeader` (not `RequestContext`), `RawKvReq`/`RawKvResp` (not `RawRequest`/`RawResponse`),
   `Method` covering every service (not `RawMethod`), and **one** `ProtoError` rather than a
   `ServerError`/`CallError` split — `outcome()` is what the split was for. `Peer`, `PeerRole` and
   `Region` are as transcribed. The empty-`end_key`-is-unbounded rule is implemented in
   `Region::contains` and `Region::contains_range` with a test that pins it.
4. **`Scan::keys_only` — dropped.** Nothing in this phase calls it, and a wire field with no caller
   is a field with no test. It is a `WIRE_VERSION` bump away if phase 6's index scans want it.
5. **Confirmed: the client sends raw user bytes.** The `'r'` namespace is the store's job on every
   path — point reads, scan bounds, `DeleteRange` bounds and `CompareAndSwap` alike.

## 4. The store

`Store::open` opens one `esker-engine` `Db` with **all four built-in column families created at
bootstrap** — `default`, `lock`, `write`, `raft` (`docs/DESIGN.md` §4.8: the engine imposes no
column family, the store does) — and one region, id 1, covering `["", "")` with epoch
`{ conf_ver: 1, version: 1 }`.

* **Keys are namespaced server-side.** A client sends `k`; the store stores `'r' ++ k`
  (`esker_keys::prefix::raw_key`). The prefix never crosses the wire, so a client cannot address
  another namespace by writing one into a key (`docs/DESIGN.md` §3).
* **Every request is checked against the region** — epoch first, then key range — even though one
  region covers everything and its epoch never moves in this phase. `EpochNotMatch` carries the
  current region so a client can refresh its cache; `KeyNotInRegion` carries the range.
* **Concurrency is the engine's.** The `Db` is behind an `Arc`, requests run on tokio workers, and
  every engine call goes through `spawn_blocking` so an fsync never stalls the reactor. There is no
  global request lock. The one exception is a `RwLock` whose *read* side every mutation takes and
  whose *write* side `CompareAndSwap` takes, which is what makes a read-modify-write atomic against
  concurrent writers on a single node; from phase 3 the Raft log is that serialisation point.
* **`DeleteRange` does not lie.** The engine's `DeleteRange` is the v1 limitation of
  `docs/DESIGN.md` §4.7 — the entry answers for the key at `begin` and no other — so the store does
  not use it. It scans the range and writes point deletes in one atomic batch, and refuses a range
  holding more than `MAX_DELETE_RANGE_KEYS` with a typed `Unsupported`. ADR 0006.

## 5. Test list

| Step | Tests |
|---|---|
| frame | golden frames (empty body, maximum body, every kind); proptest round trip; **the golden stream fed in random chunkings** — a frame split across arbitrary TCP reads must decode identically; a flipped bit anywhere in a frame is `Corrupt`; `len` over the maximum is refused before anything is allocated, on read and on write; **random bytes into the decoder never panic** (invariant 9) |
| codec/messages | golden bytes per message type and per error code; proptest round trip over generated messages; an unknown method is an error; trailing bytes are an error; a truncated body is an error, never a partial value |
| transport | loopback with **1,000 concurrent in-flight requests and randomised reply order**, every one routed to its own caller; a duplicate in-flight id is refused; the in-flight bound answers `ServerIsBusy` rather than growing; ping/pong keeps an idle connection alive and a dead peer is dropped; a stream of chunks arrives in order and ends at `StreamEnd`; a client on the wrong `WIRE_VERSION` is refused at `connect` |
| store | region checks (epoch behind, epoch ahead, key outside the range); every RawKv method against the engine; `sync = true` by default; `DeleteRange` deletes exactly the range and refuses an oversized one; `CompareAndSwap` under concurrency does not lose an update |
| server | in-process on an ephemeral port over **real TCP**: RawKv round trips; epoch mismatch → `EpochNotMatch`; oversized frame refused; wrong `WIRE_VERSION` refused; many concurrent clients; graceful shutdown finishes in-flight work and closes the database cleanly |
| client (lane 2) | retry and backoff against a fake transport returning `NotLeader`, `EpochNotMatch`, `ServerIsBusy`; the bound is respected; a non-retryable error is returned immediately; the region cache is invalidated by `EpochNotMatch` |
| cross-cutting (lane 2) | the phase-1 model test re-run through the client; the crash loop through the client with `sync = true`; the 64-client load test |

## 6. Risks

1. **Async at the edge, and only there.** `esker-engine` is synchronous, and `CLAUDE.md` keeps it
   that way. Every engine call is inside `spawn_blocking`; no engine lock is ever held across an
   `.await`. The failure mode if this slips is not a deadlock in testing, it is tail latency under
   an fsync in production.
2. **The CRC's coverage.** It covers `kind ++ request_id ++ body`. Checking the body alone would
   let a flipped request id deliver a response to the wrong caller, with both frames intact. Pinned
   by a golden file and by a test that flips exactly the id byte.
3. **Partial frames.** TCP delivers arbitrary chunks. The decoder is a pure state machine over a
   buffer with no I/O in it, so the chunking proptest can drive it directly.
4. **Backpressure, not queues.** Every channel is bounded and every bound has a test. An unbounded
   queue turns a slow disk into an out-of-memory kill.
5. **A duplicate request id.** The demux rejects it. Overwriting the waiter would leave the first
   caller waiting for a response that can never arrive.
6. **`DeleteRange` semantics.** The engine cannot do it yet; the store must not pretend it can. The
   scan-and-delete implementation is bounded, atomic, and documented as what it is (ADR 0006).
7. **Contract drift with the client lane.** §3 is pinned before either lane builds on it. A
   mismatch is a report line and a `// TODO(sibling)`, never an edit to the other lane's files.
8. **Two lanes in `esker-cli`.** Additive edits only, in different functions and different modules.

## 7. Non-goals for this phase

| Not doing | Why | Whose problem next |
|---|---|---|
| Raft, replication, more than one region | The API is already shaped for them: a header, an epoch, a redirect hint | phases 3, 4 |
| `TxnKv`, `Pd`, `RaftTransport` methods | Their service bytes are reserved so the numbering does not move | phases 3, 4, 5 |
| TTL for RawKV keys (prompt deliverable 5, optional) | It is a compaction-filter change plus a read-time check plus an ADR on expiry semantics; it is optional in the prompt, and the phase's own acceptance does not need it | phase 2b, if the coordinator wants it |
| TLS, authentication | `docs/DESIGN.md` §13 leaves the pure-Rust TLS question open until phase 6b | phase 6b |
| Real streaming *users* | Frame kinds and the send/receive path exist and are tested by an echo; the snapshot that uses them is phase 4 | phase 4 |
| Range tombstones | ADR 0006: the store works around the engine's v1 limitation rather than changing a frozen format | phase 5 |
| Reconnect inside `TcpTransport` | A dropped connection fails its waiters with `Closed`; deciding whether to reconnect is the client's policy, next to its retry budget | client lane |

## 8. Progress

- [x] step 0 — this plan
- [x] step 1 — framing (`frame.rs`, `codec.rs`, `error.rs`, `region.rs`; golden frames, the
      chunking proptest, the fuzz decoder)
- [x] step 2 — messages and errors (`messages.rs`; forty golden bodies covering every method and
      every error code)
- [x] step 3 — transport (`transport/{mod,conn,client,server}.rs`; 20 loopback tests including the
      1,000-concurrent-request demultiplexer case)
- [x] step 4 — store (`error.rs`, `region.rs`, `rawkv.rs`, `server.rs`, ADR 0006)
- [x] step 5 — `esker-cli server --data-dir --listen`, graceful shutdown on ctrl-c
- [x] step 6 — integration tests over real TCP on an ephemeral port
- [ ] steps 7–9 — the client lane's

**Gate at the end of this lane's work**: `just check` green across the workspace; 708 tests
passing; 22 of 40 runtime crates (tokio brought `mio`, `libc`, `socket2`, `signal-hook-registry`
and `tokio-macros`, which ADR 0003 anticipated); 3 golden files
(`frames.hex`, `messages.hex`, and the phase-1 set unchanged). `esker server` +
`esker raw put/get/scan/delete` verified end to end against the client lane's build.

## 9. Changes vs plan

### 9.0 What changed while building it

1. **`ProtoError` gained `NotSent` (15) and `Timeout` (16), and `outcome()`.** The client lane
   asked for "provably never sent" to be distinguishable from "sent, no answer", and it was right
   to: without it a client cannot tell a write it may safely repeat from one it may not, and
   phase 5's `Prewrite` needs the same distinction. Every failure path in the transport is written
   to preserve it, and `esker-store`'s engine-error mapping is too — an engine failure whose effect
   on the log is uncertain maps to `Unknown`, which is the safe direction to be wrong in.
2. **`BlockingTransport`**, wrapping the async `Transport` with an owned runtime and a per-call
   deadline. The CLI and `bench --remote` are threads rather than futures. It shuts its runtime
   down in the background on drop, because dropping a `Runtime` inside an async context panics and
   a type that is unsafe to drop in half the program is a trap rather than a convenience.
3. **`TransportConfig::shutdown_grace`.** The first version of `Server::serve` waited for every
   in-flight permit to come back, so one handler wedged on a stuck disk would hold the process open
   past any patience. The drain is bounded and logs what it abandoned.
4. **`WriterStop`.** With the drain bounded, an abandoned handler still held a `FrameSink` clone,
   and the writer task stops when the last one drops — so the socket stayed open and the peer waited
   out its own 30-second request timeout. Closing is now explicit rather than by last reference.
   Visible as a loopback suite that took 30 seconds and now takes 0.2.
5. **`RawKv Scan` has a byte budget as well as a key limit** (`max_scan_bytes`, 4 MiB). A response
   that will not fit in one frame is a response the transport must refuse, and the client would get
   nothing rather than a first page. Not in the original plan; found by asking what a scan of large
   values does.
6. **`RawKv DeleteRange` does not use the engine's `DeleteRange`.** ADR 0006, and it is a real
   finding rather than a design choice: `docs/DESIGN.md` §4.7 claimed v1 rejects a wide range with
   an error, and in fact the engine accepts every range and deletes the key at `begin`. §4.7 now
   says what v1 actually does. **The engine gap is phase-1 code and is not fixed here** — it is a
   report line to the coordinator.
7. **`RegionMeta` holds a `Region`, not a `(region, epoch)` pair** as the brief sketched. The epoch
   is a field of `Region` because `docs/DESIGN.md` §6 defines it that way, and splitting it would
   have put the code and the design document in disagreement.
8. **A `RwLock` write gate.** Every mutation takes the shared side, `CompareAndSwap` the exclusive
   side. It is not the "global request lock" the brief forbids — writes still run concurrently with
   each other — and it is what makes a read-modify-write atomic against a concurrent `Put` on a
   single node. It goes away in phase 3, when the Raft log becomes the serialisation point.
9. **`ChunkStream`/`ChunkSender` for streamed replies**, exercised by a loopback echo. Phase 4's
   snapshot transfer is the caller; the point of building it now is that the frame kinds are not
   written for the first time under a snapshot.

### 9.1 Frame kinds stay 1-based, against the brief's 0-based numbering

The lane brief pins `Request=0 … Pong=6`. Phase 0 had already committed
`Request=1 … Pong=7` with a test — `frame_kinds_are_distinct_and_nonzero` — and a reason: a
zero byte must not be a valid kind, so that a run of zeros is not a readable frame. It is the same
rule `docs/DESIGN.md` §4.3 states for the WAL record header, where "`0` is reserved and never
valid, so an all-zero header is not an empty record".

`docs/DESIGN.md` §9 lists the kinds but fixes no numbers, so nothing above this plan is
contradicted, and the client lane consumes `FrameKind`, never the byte. Kept 1-based, reported to
the coordinator. Reversing it is a one-line change plus one golden file.

`WIRE_VERSION` did move to `u32` as the brief pins, from phase 0's `u16`.
