# 0036 — a write may have no opinion about durability

Status: accepted. Debt wave c3, unit 4, from `docs/plans/debt-c3.md` §4 — recorded as
"`WalSyncMode::Never` does not disable what it names".

## Context

`WalSyncMode` is a database-wide policy with three settings — `PerWrite`, `Interval(d)`, `Never` —
and `WriteOptions::sync` was a per-write `bool` that defaulted to `true`. One line decided what a
write did:

```rust
let sync = options.sync || self.options.wal_sync_mode == WalSyncMode::PerWrite;
```

An **OR**, so the mode could only ever *add* syncing and never remove it. And since
`WriteOptions::default()` said `sync: true`, and `Db::put` and `Db::delete` take the default, no
write in this repository ever left the mode anything to decide. `WalSyncMode::Never` was a no-op
for every caller that had not gone out of its way to pass `sync: false`.

That is not a subtle cost. `crates/esker-store/tests/bench_columnar.rs` opens `Never` and writes
through `Db::put`, and `docs/bench/columnar-learner.md` recorded the result as 4.8 ms per
single-row put "with sync switched off", concluding from a stack sample that whatever the write
path waited on "it is not an `fsync`". It was an `fsync` — the sample was reading the syscall's
frames as `commit_group`'s, which calls `wal.writer.sync()` under the WAL lock. Measured both ways
on one machine: **90.37 s before, 170.26 ms after, for the same 20,000 rows**
(`docs/bench/debt-c3.md`).

A second defect came with it. `WalSyncMode::Interval(d)` promises "sync in the background at this
interval", and **nothing read that variant** — the single line above compared the mode against
`PerWrite` and nothing else. So `Interval` was `Never` wearing another name: a database configured
for *bounded* loss had unbounded loss, and said nothing about it.

The root cause is a missing state rather than a wrong comparison. A `bool` can say "durable" and
"not durable"; it cannot say **"no opinion"**. Without that third state there is no such thing as a
write the policy is entitled to decide, so a policy is decoration.

## Decision

**1. `WriteOptions` carries a `Durability`, not a `bool`.**

```rust
pub enum Durability {
    /// Whatever the database's `WalSyncMode` says. The default.
    Policy,
    /// Durable before acknowledgement, whatever the policy says.
    Durable,
    /// Acknowledged before the bytes are durable, whatever the policy says.
    Buffered,
}
```

`WriteOptions::default()` is `Policy`. `synced()` and `unsynced()` keep their names and their
meanings and are now the only way to express a demand — every `WriteOptions { sync: … }` literal in
the tree became one of them, which preserves each site's intent exactly because each site had
already written down which one it meant.

**2. The precedence lives in one function**, `WriteOptions::wants_sync(mode)`, so the mode and the
per-write demand cannot drift apart:

| | `PerWrite` | `Interval(d)` | `Never` |
|---|---|---|---|
| `Durable` | sync | sync | sync |
| `Buffered` | sync¹ | no | no |
| `Policy` | sync | no | no |

¹ A group is one log record, so a `Buffered` write that shares a group with a `Durable` one is
synced with it: one caller cannot un-ask for another's durability. What `Buffered` buys is that
*this* caller never waits.

**3. `Interval` is a background thread.** `DbInner::wal_sync_loop` syncs the log every interval,
waiting on the flush condvar rather than sleeping so that `Drop` — which sets `shutdown` under that
lock and notifies — ends it within one wake rather than within one interval. Flush activity can
wake it early and it then syncs sooner than asked, which only makes writes durable sooner; the
contract this mode offers is an *upper bound* on what a crash may lose. A sync that fails is logged
and the loop continues, because a syncer that exits on one bad sync turns a transient I/O error
into exactly the unbounded loss the mode exists to avoid.

**4. A clean close syncs the log, whatever the mode.** `Never` and `Interval` trade durability away
*for a crash*; an orderly shutdown is not one. Without this, closing a database cleanly could lose
its most recent writes, which is not a trade either mode offers.

## Consequences

**Invariant 1 is untouched, and this is checkable rather than asserted.** The invariant says a
write is acknowledged only once its bytes are durable "unless the caller explicitly passed
`sync = false`". `Durable` is that guarantee and outranks every mode; `Buffered` is that opt-out and
stays explicit. What changed is only what a caller with *no* opinion gets, and under the default
mode that is what it always was.

**`esker-store`'s behaviour is bit-for-bit what it was.** It has no `WriteOptions::default()` write
site: every write in the crate says `synced()` or `unsynced()`. It has always opened its engine
`Never` and has always done its own syncing, so the store's durability was never resting on this
knob. `esker-engine`'s 409 tests pass, `crash_kill` included.

**A database opened `Never` is now genuinely fast and genuinely fragile.** That is the point, and
it is worth saying plainly: before this, `Never` was a lie in the safe direction, and code that
relied on the lie would now lose data. Nothing in this repository does — the audit above is what
says so — but an external caller opening `Never` and writing through `Db::put` gets what it asked
for from this release on.

## Alternatives rejected

**Make `Never` override an explicit `sync: true`.** The smallest possible change, and it weakens
invariant 1: a caller that demanded durability would silently not get it. `CLAUDE.md` requires
asking a human before weakening an invariant, and there was no need to — the tri-state fixes the
knob without touching the guarantee.

**Leave it and correct the documentation.** The standing order for this wave is that a named bug is
fixed rather than documented, and a durability knob that cannot be turned off through the ordinary
write path is a bug in the knob, not in its description. The 531× measurement is what it was
costing.
