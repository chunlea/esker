# Phase 4a — the placement driver, lane `cl-p4-pd`

Status: **4a landed** (§10). The lane plan for `crates/esker-pd/**`, the `Pd` service section of
`crates/esker-proto/**`, and the `esker pd` subcommand. It hangs under `docs/plans/phase-4.md`,
which the store lane owns and which pins the cross-lane contract in its §3; nothing here may
contradict that file. Spec: `prompts/04-multiraft-pd.md` (4a), `docs/DESIGN.md` §7 and §9,
`CLAUDE.md` invariants 1, 2, 6 and 9.

The deliverable is **one PD process with durable state**: bootstrap, id allocation, the timestamp
oracle, the routing table, and the six wire methods that carry them. PD high availability is 4e and
scheduling is 4b–4d; both are explicitly out (§7).

## 1. What makes this component different

Two of the three pieces here are *the* single point of correctness for a layer above:

- **The oracle is the clock of the whole system** (invariant 6). A timestamp that repeats or goes
  backwards breaks Percolator's snapshot isolation in phase 5 in a way that no test in phase 5 will
  reproduce — it will look like a lost update under load, once. The defence is a high-water mark
  persisted *ahead* of what has been handed out, fsynced *before* any timestamp above the old mark
  leaves, and a restart that resumes at `max(clock, mark)` so that a clock jumping backwards is
  survivable rather than fatal.
- **An id must never be handed out twice**, for the same reason a region id must never name two
  regions. The batch's *end* is persisted before any id in it is given away, so a crash skips ids
  rather than repeating them.

The third piece, the routing table, is *advisory to clients and authoritative here* — a client's
cache being stale costs a redirect, never a wrong answer (§7 of the design). That asymmetry is why
the routing table is allowed to be a best-effort upsert while the oracle is not.

## 2. Files

```
crates/esker-pd/src/lib.rs          crate docs, re-exports; the ts layout constants (phase 0)
crates/esker-pd/src/error.rs        PdError, and the ProtoError each variant becomes
crates/esker-pd/src/clock.rs        the Clock seam: SystemClock, and a test clock that jumps
crates/esker-pd/src/keys.rs         PD's private key space, documented in one module
crates/esker-pd/src/record.rs       the four record encodings + their strict decoders
crates/esker-pd/src/alloc.rs        the id allocator: persist the batch end, then hand out
crates/esker-pd/src/tso.rs          the oracle and its mark
crates/esker-pd/src/routing.rs      region and store records: upsert, lookup, liveness
crates/esker-pd/src/pd.rs           the Pd facade — open + the six operations, synchronous
crates/esker-pd/src/service.rs      PdService: esker_proto::Service, the async edge
crates/esker-pd/tests/golden/*.hex  the record bytes, pinned
crates/esker-pd/tests/*.rs          records, restart, loopback over TCP, kill -9 loops
crates/esker-proto/src/pd.rs        service 0x03: the six messages, and the PdChannel
crates/esker-cli/src/pd.rs          `esker pd serve` and `esker pd inspect`
```

Shared-crate edits, kept as small as they can be and reported to the coordinator:
`esker-proto/src/messages.rs` (six `Method`s, the `Pd` arms of `Request`/`Response`),
`esker-proto/src/error.rs` (two codes, §5), `esker-proto/src/lib.rs` (the module and its
re-exports), `esker-cli/src/{args.rs,main.rs}` (one subcommand).

## 3. PD's own key space (*fixed*, format version 1)

PD keeps its state in its own `esker-engine` database, default column family only, under the `'m'`
metadata space of `docs/DESIGN.md` §3. Ids are **big-endian** so that a scan runs in id order, for
the same reason the store's Raft log is.

```text
'm' 'c'                        →  cluster record: cluster id, the first region's id, created ms
'm' 'a'                        →  allocator record: the end of the reserved id batch
'm' 't'                        →  the oracle's high-water mark, in physical milliseconds
'm' 's' ++ store_id:u64 BE     →  store record: address, stats, last heartbeat
'm' 'r' ++ region_id:u64 BE    →  region record: the Region, plus leader hint and last heartbeat
'm' 'k' ++ tag:u8 ++ end_key   →  range index: which region id ends here (tag 1 bounded, 2 = +∞)
```

Every value is `version:u8 ++ fields`, hand-encoded with `esker-proto`'s `Encoder` and decoded
strictly — an unknown version, a trailing byte or a truncated field is an error value, never a
guess (invariants 2 and 9). This is the store's convention in `raft_log.rs`, kept deliberately:
the engine already checksums every byte it stores, so a per-record CRC would be a second checksum
over the same bytes.

**Why two keys per region.** The primary record is keyed by region id, because a heartbeat arrives
naming a region and must find it in one seek. The range index is keyed by *end* key, because
`GetRegion(key)` is "the first region whose end is past this key" — one `seek` to
`'m' 'k' ++ 1 ++ key ++ 0x00`, which is the successor of `key` in byte order, so a region ending
exactly at `key` is skipped and the next one found. The `tag` byte exists because an empty
`end_key` means +∞ in region metadata (phase-4 plan §5) and `b""` sorts *below* everything: the
unbounded region gets tag 2 and therefore sorts last, which is where +∞ belongs. Both keys are
written in **one `WriteBatch`**, so the index can never name a region that is not there.

## 4. The three state machines

### 4.1 The allocator

`allocated_end` is the last id reserved. Ids are handed out from `next..=allocated_end`; when the
batch runs out, `allocated_end += ALLOC_BATCH` is written **with `sync = true`** before a single id
of the new batch is returned. A restart sets `next = allocated_end + 1`, so a crash *skips* the
unused tail of a batch and can never repeat one. Ids start at 1: zero is not an id, as everywhere
in this codebase.

### 4.2 The oracle

`ts = physical_ms << 18 | logical` (phase 0 pinned the layout). One allocation:

```text
now = clock.now_ms()
if now > physical            → physical = now, logical = 0     // the normal case
if logical + count > 2^18    → physical += 1,  logical = 0     // a full millisecond rolls over
reserve(physical)                                              // fsync the mark if needed
ts = compose(physical, logical); logical += count
```

`reserve(p)` is the whole invariant: **if `p >= mark`, write `mark = p + 3000` with `sync = true`
before returning.** Every timestamp ever handed out therefore has `physical < mark`, and a restart
resumes at `max(clock.now_ms(), mark)` — which is strictly greater than every physical part already
used, whatever the wall clock says. A clock that jumps *backwards* does not move `physical` at all
(the `now > physical` test), so the oracle keeps counting in logical bits within the last
millisecond it reached and never repeats.

The wall clock is read **only here**, and through the injected `Clock` seam, so the tests drive a
clock that goes backwards, stalls, and jumps a year, without sleeping.

### 4.3 The routing table

`RegionHeartbeat` is an upsert guarded by the epoch. The comparison is on `(conf_ver, version)`
lexicographically, because the two counters move on different events (`Epoch::is_stale_against`
already says so in proto, and this reuses it rather than restating it):

- incoming epoch **older** in either counter → the write is dropped, the stored record wins;
- incoming epoch **newer** → the record is replaced, and the range index is rewritten in the same
  batch if the end key moved;
- **equal** epochs → the newer leader hint and stats win. This is the case that matters after a
  leader change: two heartbeats for one epoch, and the last one to arrive is the current truth.

`StoreHeartbeat` refreshes stats and `last_heartbeat_ms`. Liveness is derived — a store is down if
`now - last_heartbeat_ms > max_store_down_time` — and *nothing acts on it in 4a*; acting is 4c.

## 5. The wire (service `0x03`)

Method numbers, contiguous as §9 of the design requires:

| Method | Number | Request | Response |
|---|---|---|---|
| `Pd::Bootstrap` | `0x0301` | `StoreInfo { store_id, address }` | `{ cluster_id, region: Option<Region> }` |
| `Pd::StoreHeartbeat` | `0x0302` | `{ cluster_id, store_id, capacity, available, region_count, leader_count, applied_bytes }` | `{}` |
| `Pd::RegionHeartbeat` | `0x0303` | `{ cluster_id, region, leader_peer_id, term, approximate_size, applied_index }` | `{}` |
| `Pd::GetRegion` | `0x0304` | `{ cluster_id, key }` | `{ region: Option<Region>, leader_peer_id: Option<u64>, stores: Vec<StoreInfo> }` |
| `Pd::AllocId` | `0x0305` | `{ cluster_id, count }` | `{ start, count }` |
| `Pd::Tso` | `0x0306` | `{ cluster_id, count }` | `{ start_ts, count }` |

The heartbeat field sets are exactly the ones phase-4 plan §3.2 pinned, so the store lane's
`PdClient` maps onto them without a gap.

**`Bootstrap` is idempotent and registers the store.** An empty cluster mints the cluster id,
records the store, creates region 1 `["", "")` with one voting peer on that store, and answers
`region: Some(..)`. A cluster that already exists refreshes the store record and answers
`region: None` — "the cluster is there, you did not create it". A store therefore calls it on every
start, which is also how PD learns a store's address again after a restart.

**`GetRegion` returns the addresses of the peers' stores.** A client addresses a store by id and
"resolving one to a socket is PD's job" (§10); returning them with the region is one round trip
instead of two, and keeps the method list at the six the design names.

**Two new error codes**, both in `esker-proto/src/error.rs`, both `NotApplied` and both *not*
retryable: `NotBootstrapped` (17) — asked before any store has bootstrapped; and
`ClusterMismatch { expected, actual }` (18) — a request carrying another cluster's id, which is a
misconfiguration and must never be retried into. `GetRegion` on an empty table is the first, never
an empty answer.

**`PdChannel`** (in `esker-proto/src/pd.rs`) is the client half: it holds an `Arc<dyn Transport>`
and the cluster id, stamps every request with it, and returns the typed answers. It is deliberately
thin — no caching, no retries, no scheduling — because the store lane's `PdClient` wraps it and
that is where the policy belongs.

## 6. Units, one commit each

| # | Unit | Files |
|---|---|---|
| 0 | This plan | `docs/plans/phase-4-pd.md` |
| 1 | The key space, the records, the errors, the clock seam | `keys.rs`, `record.rs`, `error.rs`, `clock.rs` + goldens |
| 2 | Bootstrap and the allocator, on a real engine | `pd.rs`, `alloc.rs` |
| 3 | The oracle and its mark | `tso.rs` |
| 4 | The routing table: epoch-guarded upsert, lookup, liveness | `routing.rs` |
| 5 | Service `0x03` on the wire, with goldens | `esker-proto/src/pd.rs`, `messages.rs`, `error.rs` |
| 6 | `PdService` and `PdChannel`: PD behind a socket | `service.rs`, `esker-proto/src/pd.rs` |
| 7 | `esker pd serve` / `esker pd inspect` | `esker-cli/src/pd.rs`, `args.rs`, `main.rs` |
| 8 | Loopback: bootstrap → heartbeats → GetRegion over real TCP | `tests/loopback.rs` |
| 9 | `kill -9` around the allocator and around the mark | `tests/crash_kill.rs` |
| 10 | DESIGN §7/§9 updated; the ADRs | `docs/DESIGN.md`, `docs/adr/*` |

## 7. Non-goals for this lane, this round

- **Scheduling of any kind** — replica repair, leader balance, region-count balance, operators and
  their timeouts. 4b–4d. Nothing here may grow an operator state machine "because it is easy".
- **PD high availability.** One process, durable state, as §7 of the design permits until 4e. The
  oracle's mark goes through Raft *then*, not now — and until then PD is a single point of failure,
  which is a stated property of 4a rather than an oversight.
- **Splits.** PD hands out ids for them in 4b; it does not choose split keys and does not know what
  a split is in 4a.
- **A safepoint for GC.** Phase 5.
- **Store liveness having consequences.** Recorded, exposed, acted on in 4c.

## 8. Test list

| Area | Tests |
|---|---|
| records | golden bytes for all four records; round trips; an unknown version, a trailing byte and every truncation are errors, never panics |
| keys | the range index orders bounded ends below the unbounded one; the successor trick skips a region ending exactly at the key; a key in the last region finds it |
| bootstrap | first store gets region 1 `["", "")` with one peer on itself; a second `Bootstrap` from the same store is idempotent; from another store it registers and gets `region: None`; the cluster id is stable across a reopen; a foreign cluster id is `ClusterMismatch` on every method |
| allocator | ids are monotone and unique; a reopen never reuses one (the batch's unused tail is skipped); a count of zero is refused; two threads never receive the same id |
| oracle | monotone across a restart **with a clock rigged to go backwards**; the mark stays ≥ 3 s ahead; batch allocation returns `count` consecutive timestamps; logical overflow rolls the millisecond rather than bleeding into it; every ts handed out has `physical < mark` |
| routing | an epoch-guarded upsert in **both** arrival orders; equal epochs take the newer leader hint; `GetRegion` across three regions including the unbounded last one; before bootstrap it is `NotBootstrapped`; store liveness is computed from the last beat |
| wire | golden bytes for all twelve message bodies; unknown method, trailing bytes, truncation; the two new error codes round-trip |
| loopback | `pd serve` on a real socket: bootstrap → store beat → region beat → `GetRegion` → `AllocId` → `Tso`, and a second client sees the first's writes |
| crash | `kill -9` between the allocator's persist and its reply, 200 iterations: no id is ever handed out twice across generations; `kill -9` around the mark, 200 iterations: no timestamp ever repeats and none goes backwards |
| mutation | both crash invariants are checked by breaking them on purpose — persist the batch *after* replying, and drop the `max(clock, mark)` on restart — and watching the test go red |

## 9. Risks

1. **A false sense of safety from a fast disk.** `sync = true` on the mark is the invariant; a test
   that passes because the writes were fast rather than because they were ordered proves nothing.
   The mutation check in §8 is the answer, and it is not optional.
2. **The shared files.** `messages.rs`, `error.rs` and `args.rs` have another writer in the same
   tree. Every edit here is additive, made in one pass late in the lane, and reported.
3. **`Bootstrap` semantics are a cross-lane contract.** Idempotent, registers the store, and
   `region: None` means "already bootstrapped". If the store lane reads it differently the two
   halves disagree about who creates region 1 — escalated the moment it is written, not at
   integration.

## 10. Progress

4a is **landed**. One commit per unit, in this order:

| # | Unit | Commit |
|---|---|---|
| 0 | This plan | `docs(plan): the placement driver's 4a lane, in writing` |
| 1 | Key space, records, errors, the clock seam, goldens | `feat(pd): the key space and the record encodings…` |
| 2 | Bootstrap, the allocator, the record I/O underneath | `feat(pd): bootstrap mints the cluster…` |
| 3 | The oracle and its mark | `feat(pd): the oracle, and the mark that is fsynced ahead of it` |
| 4 | The routing table: epoch-guarded upsert, lookup, liveness | `feat(pd): the routing table, upserted by heartbeat…` |
| 5 | Service `0x03` on the wire, with twelve goldens | `feat(proto): service 0x03, the placement driver on the wire` |
| 6 | `PdService` behind a socket | `feat(pd): the placement driver behind a socket` |
| 8 | Loopback over real TCP | `test(pd): bootstrap, heartbeats and a lookup over a real socket` |
| 9 | `kill -9` loops | `test(pd): kill -9 around the allocator and around the oracle's mark` |
| 7 | `esker pd serve` / `esker pd inspect` | `feat(cli): esker pd serve, and esker pd inspect` |
| 10 | DESIGN §7/§9/§14, ADRs 0010 and 0011 | `docs(design): what the placement driver actually stores…` |

Units 8 and 9 landed before 7 because the CLI's file is shared with the store lane and was in
flux; the order inside the lane is otherwise the one above.

## 11. Changes vs plan

1. **The record I/O landed in unit 2, not unit 4.** Bootstrap has to write a store record and a
   region record with its index entry, atomically, so `routing.rs` could not wait for the
   heartbeat unit. Unit 4 added the epoch guard and the beats on top of it.

2. **`RegionRoute.stores` is `Vec<StoreInfo>`, not `Vec<(u64, String)>`.** `esker-proto` already
   has the type the wire uses, and one vocabulary beats a conversion at the edge.

3. **The test clock needed a `testing` feature and a self-dev-dependency.** An integration test
   links the library as an ordinary dependency, so `#[cfg(test)]` does not reach it. This is the
   shape `esker-engine` already uses for its fault-injecting filesystem, copied deliberately.

4. **A `SIGKILL` cannot prove the fsync.** It kills the process, not the machine: bytes already in
   the kernel are still written back. The kill loop therefore proves the *ordering* and the restart
   rule and cannot tell `sync = true` from `sync = false`; both were mutation-checked in the forms
   it *can* see (resume at the clock, drop the reservation's write), and the limit is written down
   beside the test. The engine's own invariant-1 tests have the same edge, so this is a property of
   the project's test strategy rather than of this lane.

5. **Two `ProtoError` variants and two `Request`/`Response` variants.** Every exhaustive match over
   those enums needed an arm; `esker-store`'s service had already grown one by the time the wire
   landed, so no file outside this lane was edited. The `Method::ALL` sweep, the service-byte test
   and the golden-coverage test in `esker-proto` all went red until the six methods had goldens —
   which is the mechanism working exactly as intended.

6. **`esker pd inspect` prints the range index beside the records.** Not in the plan. A
   disagreement between the two is the failure that makes routing wrong while every other line of
   the report still looks right, so it is printed and counted, with a `WARNING` when they differ.

7. **A store heartbeat from an unregistered store is refused.** Auto-registering one would put a
   store record with no address into the table, and a client cannot be routed to that. Escalated to
   the coordinator: it means a store must call `Bootstrap` — which is registration — before it
   starts beating, on every start.

8. **PD's `Region` encoder is its own.** `Region::encode` in `esker-proto` is `pub(crate)`, and this
   lane may not edit `region.rs`; writing PD's own encoder is also the better answer, because an
   on-disk format and a wire format that merely agree today should not share a definition. ADR 0010
   records it, and the goldens cover both.

9. **No benchmark.** `esker-cli bench`'s workloads are the store lane's file, and PD has no workload
   in 4a. A TSO/`AllocId` throughput number is worth having before phase 5 puts the oracle on every
   transaction's critical path; offered to the coordinator rather than taken.
