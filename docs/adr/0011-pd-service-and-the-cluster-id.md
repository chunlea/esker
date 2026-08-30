# ADR 0011 — service `0x03`, and the cluster id on every placement-driver request

Status: accepted (phase 4a)
Date: 2026-08-30
Context: `docs/DESIGN.md` §7, §9, §10; `docs/adr/0002-formats-are-hand-rolled.md`;
`docs/plans/phase-4.md` §3.2; `docs/plans/phase-4-pd.md` §5

## Context

`docs/DESIGN.md` §9 reserves service `0x03` for `Pd { Bootstrap, StoreHeartbeat, RegionHeartbeat,
GetRegion, AllocId, Tso }` and says nothing else about it. Three questions had to be settled before
the six methods could be written, and each one is a decision a future reader might reverse:

1. **What addresses a PD request.** Every key-value request carries `{ region_id, epoch, peer }`
   (invariant 5), but PD's answers are *about* the routing table rather than served from a region,
   so that header means nothing here.
2. **What `Bootstrap` means when the cluster already exists.** Something has to decide which store
   creates region 1, exactly once in the life of a cluster, and say so to every other caller.
3. **How a client turns a store id into a socket.** §10 says a client addresses a store by id and
   that "resolving one to a socket is PD's job", but the six methods do not obviously include one
   that answers it.

## Decision

**A `Pd` request carries a cluster id instead of a region header; `Bootstrap` is idempotent
registration that hands out a region exactly once; and `GetRegion` answers with the addresses of the
region's peers' stores.**

```rust
Request::Pd { cluster_id: u64, request: PdReq }   // 0x0301 .. 0x0306

PdResp::Bootstrap { cluster_id, region: Option<Region> }
PdResp::GetRegion { region: Option<Region>, leader_peer_id: u64, stores: Vec<StoreInfo> }
```

Two error codes join the wire enum: `NotBootstrapped` (17) and
`ClusterMismatch { expected, actual }` (18). Neither is retryable.

## Rationale

**The cluster id, on every request.** Two clusters sharing an address is a configuration mistake
people make — a stale `--pd` flag, a test PD left running on a familiar port — and without a check
its symptom is not an error but an *answer*: one cluster serving routing for the other, after which
a store writes to a region that belongs to a different cluster. The id is minted once at bootstrap
and checked on every later call, in one place at the top of the service dispatch so that a method
added later cannot skip it. `Bootstrap` may send zero, meaning "not known yet", because asking is
how a caller learns it — and a caller that *does* name a cluster is checked even there, so a store
that already belongs to one cannot quietly create a second on a PD whose state was wiped.

**`ClusterMismatch` and `NotBootstrapped` are not retryable.** Both are refusals a retry loop cannot
improve: one is a misconfiguration, and the other waits on an event — some store bootstrapping —
that may never happen. A caller that *is* waiting for a cluster to appear (a store starting up
beside its siblings) waits on purpose, in its own loop, with its own patience; spending a generic
retry budget on it would only hide the misconfiguration underneath.

**`Bootstrap` is registration, and answers a region exactly once.** The alternative — a separate
`IsBootstrapped` or a hard `AlreadyBootstrapped` error — needs two round trips to say the same thing
and gives a restarting store nothing useful. Making it idempotent means a store calls it on **every**
start: the first call in the life of a cluster comes back with region 1 to create, and every later
one comes back with `region: None` and a refreshed address. That address refresh is the reason to
prefer this shape: it is the only moment PD learns where a store is, so a store that restarts on a
new port is reachable again without a new method.

**`GetRegion` carries the stores.** The region's peers name store *ids*; a client that has one still
cannot open a socket. Answering with the addresses of exactly the stores that host this region's
peers turns two round trips into one, keeps the method list at the six §9 names, and scopes the
answer — a client learns where the stores it is about to talk to are, not the whole cluster's
membership. A store PD has never heard of is absent from the list rather than present with an empty
address, so "I do not know where that is" is not confusable with "it is at the empty address".

**`PdChannel` is thin on purpose.** It encodes, sends, stamps the cluster id and checks that the
answer is the one that was asked for. It does not cache, retry or schedule, because the store lane's
`PdClient` (`docs/plans/phase-4.md` §3.2) wraps it and that is where policy belongs; `pd::encode` and
`pd::decode` are the same protocol with nothing wrapped around it, for a caller with a blocking
transport or a scripted fake.

## Consequences

- **The heartbeat field sets are a cross-lane contract.** They are exactly the ones the phase-4 plan
  pins for `PdClient`, so the store's payloads map onto the wire with no gap. A field added later
  that does not change an existing meaning is allowed and reported; anything else is escalated.
- **`Request` and `Response` grew a variant each**, so every exhaustive match over them had to gain
  an arm — including `esker-store`'s service, which now refuses a `Pd` method rather than being
  unable to compile. A store answering anything but a refusal would let a misconfigured client
  believe it had reached PD and route a cluster from a store's opinion.
- **PD's handshake reports store id zero.** `HelloAck` carries a store id and PD is not a store; zero
  is not a store id anywhere in this codebase, which makes it the honest answer. A client keys its
  connections by that field, and PD is never in that book.
- **Twelve more golden bodies** (six requests, six responses, plus the two `Bootstrap` and
  `GetRegion` shapes that carry an absent region) and two more golden error codes. The sweep that
  demands a golden for every method and every code is what will catch the seventh method that
  arrives without one.
