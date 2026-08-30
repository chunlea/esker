# 0016 — `TxnKv` on the wire, and where a lock stops being opaque

Status: accepted (phase 5). Supersedes nothing. See `crates/esker-proto/src/txn.rs`,
`docs/DESIGN.md` §9, `docs/txn-spec.md`, [ADR 0015](0015-txn-record-encodings.md).

## Context

`docs/DESIGN.md` §9 reserved service `0x02` for `TxnKv` in phase 2 and named its eight methods.
This ADR fills the seam in. The method numbers were decided then; what was not decided is how a
locked key is reported, what a request carries, and how much of Percolator `esker-proto` is allowed
to know.

## Decision 1: a lock in the way is an `Error` frame, not a response variant

A `Get`, `Scan` or `Prewrite` that meets another transaction's lock answers
`ProtoError::Locked { lock_info }`, with the payload a [`LockInfo`]. It is not a `TxnKvResp`
variant.

**Options.** (a) A response variant — `TxnKvResp::Locked { locks }` — which would let one answer
carry every lock a batch collided with, and would let a `Scan` return the rows it did read
alongside. (b) The error frame, which `docs/DESIGN.md` §9 already lists `Locked{lock_info}` in.

**(b).** The client's retry machinery classifies *errors*, in one place, through
`ProtoError::is_retryable` and `ProtoError::outcome` (`docs/DESIGN.md` §10). A refusal delivered as
a successful response would be the one refusal that bypasses it, and every caller would have to
remember to look. `Locked` is already `RequestOutcome::NotApplied`, which is exactly right: the
store wrote nothing.

**Consequences.** One lock is reported at a time, so a batch that collides with several costs one
round trip per lock. That is acceptable because prewrite is idempotent (`docs/txn-spec.md` §5.2) so
a retry after resolution is free, and because contention on many keys of one batch is the case
where backing off is wanted anyway. Batching them later means adding a response variant, not
changing this one — the format allows it without a version bump.

### The other half of the line: what *is* a response

A lock is a refusal to **serve**. So are `NotLeader`, `EpochNotMatch` and `ServerIsBusy`: the
client's routing and retry machinery handles all four, uniformly, and the caller never sees them.

The rest of what a `Prewrite`, `Commit` or `Rollback` can say is not that. "A commit landed after
your snapshot", "you were rolled back", "you already committed", "your lock is gone" are
*determinations about this transaction*, which no retry changes and which the caller must act on.
They travel in the response, as a `TxnStatus` byte:

```
Ok 1 | Conflict 2 ++ commit_ts | RolledBack 3 | Committed 4 ++ commit_ts | LockNotFound 5
```

Putting them in the error channel would mean a client retrying, backing off and exhausting a budget
against an answer that will never differ — its classifier has no way to tell that a `Locked` is
worth another go and a conflict is not, because both would be `ProtoError`s. Putting a lock in the
response instead would mean the one refusal that bypasses the retry machinery. The line is *which
layer acts on the answer*, and it puts each of them where its reader is.

The five map one-to-one onto `esker_txn::TxnError`'s protocol variants, so the store handler this
lane defers is a `match` and not a translation with judgement in it.

## Decision 2: `LockInfo` lives in `esker-proto`, and is not `esker_txn::LockRecord`

`esker-proto/src/error.rs` carries a `TODO(phase-5)` proposing that `ProtoError::Locked`'s opaque
bytes become "the typed `LockInfo` of DESIGN §8". Half of that is done here and half is not.

The **type** is defined here, because a lock in an error payload is a wire message: every peer that
speaks the protocol has to read one to make progress, and a payload only one crate can decode is not
a protocol. But it is a *different type* from `esker_txn::LockRecord`, deliberately:

| | `LockRecord` (`esker-txn`) | `LockInfo` (`esker-proto`) |
|---|---|---|
| `key` | no — it is the key it is filed under | **yes** — the reader needs to name it |
| `kind`, `short_value` | yes — that is what commit writes | no — nobody resolving a lock cares |
| `primary`, `start_ts`, `ttl_ms` | yes | yes |

Merging them would put a storage record on the wire and an inline value in an error frame.

The **variant** stays `Locked { lock_info: Bytes }`. Changing it to `Locked { lock_info: LockInfo }`
would change the error frame's bytes, and `crates/esker-proto/tests/golden/messages.hex` has pinned
them since phase 2 — a format change needing its own ADR and a version bump, for no behaviour. The
bytes inside are `LockInfo::encode()` and have their own golden line, so nothing about them is
actually untested; `LockInfo::into_error` and `LockInfo::from_error` are the only two places that
cross the boundary.

`from_error` answers `Option<Result<…>>`, and the nesting is the point: `None` means "not a lock
error", `Some(Err(_))` means "a lock error nobody can read". Collapsing them would let an
undecodable refusal be treated as no lock at all, which is the one reading that loses data.

## Decision 3: a `ResolveLock` with `commit_ts == 0` rolls back

`ResolveLock` carries the stuck transaction's `start_ts` and its `commit_ts`, and zero means "roll
it back" rather than "commit it at timestamp zero".

**Options.** (a) An optional field — `present:u8 ++ varint` — two bytes and an extra shape.
(b) A separate `ResolveRollback` method, spending a method number on a boolean. (c) Zero as the
sentinel.

**(c).** No transaction commits at timestamp zero: the oracle's first timestamp is above it, and a
commit timestamp is strictly above a `start_ts` which is itself allocated. The slot is free and
saying so costs nothing. Both cases have a golden (`txn-resolve-commit`, `txn-resolve-rollback`),
because a sentinel with a golden for only one side of it is a sentinel nobody has tested.

## Consequences

- `Request` gains a `TxnKv { header, request }` variant, so `esker-store`'s dispatch match stops
  being exhaustive. It gains a `TxnKv` arm answering `Unsupported` naming the method — the store
  half of phase 5 is deliberately not built while phase 4 is open (`docs/plans/phase-5.md` §1), and
  a client that reaches for it should meet a refusal that says so rather than a default or silence.
- Every `TxnKv` request carries the same `{ region_id, epoch, peer }` header as a `RawKv` one, so
  `CLAUDE.md` invariant 5 holds unchanged and the client's routing needs no second path.
- Keys on the wire are **user keys**. The `'x'` namespace and the version suffix are the store's, as
  `'r'` is (`docs/DESIGN.md` §10). A client that namespaced its own would double-prefix, and the
  damage would not show until a scan came back full of keys nobody wrote.
- The goldens for all eight methods were produced by an encoder written separately from the one
  under test, as the rest of `messages.hex` was.
