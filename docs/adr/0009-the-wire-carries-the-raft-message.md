# ADR 0009 — `esker-proto` depends on `esker-raft`, and the wire carries the real `Message`

Status: accepted (phase 3e)
Date: 2026-08-30
Context: `docs/DESIGN.md` §5, §6, §9; `docs/adr/0002-formats-are-hand-rolled.md`;
`docs/adr/0007-raft-message-set.md`

## Context

Phase 3e has to move `esker_raft::Message` between stores. `docs/DESIGN.md` §9 reserves service
`0x04` for `RaftTransport { Batch }`, and §6 says one TCP connection per store pair carries those
frames for every region, batched per tick. What it does not say is *which type* goes on the wire,
and there are only two answers:

1. **`esker-proto` defines its own `RaftMessage`**, mirroring the enum field for field, and
   `esker-store` — which already depends on both — converts between them.
2. **`esker-proto` depends on `esker-raft`** and encodes `esker_raft::Message` directly.

Neither is free. The first keeps the two crates independent at the cost of a duplicated enum. The
second adds an edge to the dependency graph in a direction that looks inverted at first glance: the
wire crate depending on a consensus crate.

## Decision

**`esker-proto` takes a dependency on `esker-raft`, and `RaftTransport::Batch` carries
`esker_raft::Message` itself.** The batch element is

```rust
pub struct RaftMessage {
    pub region_id: u64,
    pub epoch: Epoch,
    pub from_peer: u64,
    pub to_peer: u64,
    pub message: esker_raft::Message,
}
```

— routing that Raft does not know about, wrapped around the message that Raft does.

## Rationale

**A mirrored enum drifts, and the drift is silent.** `Message` has eight variants and thirty-odd
fields, several of which exist for one specific safety rule: `pre_vote` on both vote messages,
`force` on `RequestVote`, `hint_term` on the append response, `context` on the append pair
(ADR 0007). A copy of that shape in another crate is a second place to keep those fields in step,
and the failure mode is not a compile error — it is a field that stops being copied across in the
conversion, which looks like a network that occasionally loses a flag. `hint_term` going missing
turns a term-per-round-trip catch-up into an index-per-round-trip one and nothing fails; `force`
going missing breaks leadership transfer only when a voter's lease happens to be live. These are
exactly the bugs this project spent phase 3a building a simulator to find, and they are not worth
introducing on purpose to preserve a graph edge.

**Type identity is not what freezes the format.** The instinct against this dependency is that a
change to `Message` becomes a wire change. It already is one, whichever option we pick — a new
field has to reach the other end either way. What actually pins the bytes is this crate's
hand-rolled `encode`/`decode` pair and its golden tests (ADR 0002), and those are unchanged: adding
a variant to `Message` breaks the `match` in `encode`, which is a compile error at the exact place
the format decision has to be made. A mirrored enum gives *weaker* protection here, because the
conversion can compile while quietly dropping a field.

**The direction is right, not inverted.** `esker-proto` is the envelope; `esker-raft` is one of the
things it carries, alongside `RawKv` and, later, `TxnKv` and the placement driver. An envelope
knowing the shape of its contents is the normal arrangement — it is what `messages.rs` already does
for `RawKvReq`. The graph stays acyclic by a wide margin: `esker-raft` depends only on
`esker-base`, `bytes`, `thiserror` and `tracing`, and on nothing that depends on `esker-proto`.

**The cost is small and bounded.** `esker-raft` is four dependencies and no I/O, so `esker-client`
— which pulls in `esker-proto` and has no interest in consensus — gains a small pure crate and no
runtime weight. The transitive runtime budget (`deny.toml`, currently 40) is unaffected:
workspace-internal crates are not counted, and `esker-raft` adds no external crate that
`esker-proto` did not already have.

## Consequences

- One encoding of one type, with goldens, and no conversion layer to keep honest.
- Adding a `Message` variant or field is a compile error in `esker-proto::raft`, which is where the
  wire decision belongs. That is the property we are buying.
- `esker-client` and `esker-cli` link `esker-raft` transitively. Neither uses it; both were already
  paying for `esker-proto`, which is much larger.
- `esker-raft` must stay free of anything `esker-proto` cannot tolerate. It already is, by
  `CLAUDE.md` invariant 4 — no threads, no timers, no sockets, no file I/O — and that invariant is
  now load-bearing for one more reason.
- If a future layer needs a Raft message shape that `esker-raft` does not have (a store-level
  wrapper for snapshot chunks, say), it goes beside `RaftMessage` in this crate rather than into
  the core.

## Alternatives considered

- **The mirrored enum (option 1).** Rejected on the drift argument above. It is the right choice
  when the two sides are versioned separately or owned by different teams; here they are one
  repository, one lane, and one release.
- **Making the batch body opaque bytes that `esker-store` encodes.** This keeps the graph edge out
  and moves the format into the store. Rejected because it puts a wire format outside the crate
  whose entire job is wire formats, where the golden-test convention and the `WIRE_VERSION`
  negotiation do not reach it (`docs/DESIGN.md` §9).
- **Moving `Message` into a third crate that both depend on** (a `kvproto`-shaped split). This is
  the arrangement TiKV ends up with, and it would work. Rejected as premature: it buys nothing over
  option 2 today and costs a crate, and `esker-raft` owning its own message set is what makes it
  readable next to the dissertation.
