# Debt wave, lane c3: the recorded engine, store and client debts

Seven items from the inventory taken at `4334403`, in the order the brief gave them. Each one is a
deterministic repro first, shown red against the old code, then the fix, then the same repro green.
Nothing here is documented instead of fixed.

## 1. A retired region's data is never reclaimed — and nobody tells a removed peer

Inventory #2. `crates/esker-store/src/server.rs`, `retire_region`, carrying `TODO(post-v1)`.

### What the repro found, which is not what was recorded

The debt as written is a disk leak: `retire_region` stopped the peer and destroyed its Raft state
and left the region's keys on disk, in all three column families, under no region. Its comment
blamed the engine for having no range tombstones, which stopped being true in phase 5
([ADR 0017](../adr/0017-range-tombstones.md)); `snapshot::clear_range` had been doing this exact
operation, verification pass and all, since then.

`crates/esker-store/tests/retire.rs` builds the recorded shape — two stores, a region placed on the
second by `AddPeer`, four committed rows, one uncommitted prewrite and one `RawKV` put so `default`,
`write` and `lock` all hold keys, then `RemovePeer` — and it failed **before it reached the
assertion it was written for**:

```
timed out waiting for the second store to stop hosting the region
```

with the leader's own log showing the change applying on the leader and nowhere else:

```
a region's membership changed region_id=1 index=14 peers=1 removed_self=false
an operator applied region_id=1 node=2 kind=Remove
```

Store 2 never logged a membership change at all. **A removed peer is never told it was removed.** A
configuration change takes effect when it is *appended* — §4.1, and `esker-raft` asserts it in
`conf::tests::a_leader_removed_by_a_committed_change_steps_down` ("applied at append") — so the
instant the leader appends `Remove(n)` its `Progress` has no entry for `n`, and the entry that says
`n` is gone is the first one `n` is not sent. The commit needs a quorum of the *new* configuration,
which `n` is not in. It is not a race and it does not depend on the group's size: removing peer 3
of `{1,2,3}` commits on `{1,2}`, and peer 3 hears nothing.

`retire_region` had exactly one caller — a peer applying the conf change that removed it — so on
the operator path **it never ran at all**. That is why the leak was never noticed: the retirement
that was supposed to leak was not happening either. And the leak is the least of what it leaves:
the peer stays, with no leader, campaigning for ever against a group that has replaced it, and a
restart does not heal it, because `docs/plans/phase-4.md` §6 race 3 tombstones a record whose peer
list does not name this store and this record still names it.

### The fix

[ADR 0034](../adr/0034-a-removed-peer-is-swept-and-its-range-reclaimed.md), in two halves.

**The sweep.** `Store::sweep_orphaned_regions`, on the store-heartbeat schedule beside
`promote_caught_up_learners` and for the same reason that one is there. A region whose peer has had
no leader for 50 consecutive rounds is asked about once, and PD's answer retires it only when it is
about the same region id, carries a strictly greater `conf_ver`, and names no peer on this store.
The leaderless count is a throttle; all the safety is in those three conditions, and every other
answer — including no answer — keeps the region.

**The reclamation.** `retire_region` destroys the Raft state and the region record first, synced,
and then empties the range through `clear_range`, which covers `default`, `lock` and `write` across
both physical namespaces. A crash between the two leaves keys under no record, which is where this
store used to live permanently; the other order would leave a record pointing at a half-emptied
range.

Two gates in front of the delete. The membership must name no peer on this store — and the record
that answers that has to come from the caller, because the store the sweep found holds the record
from *before* the change and always will. This is not theoretical: with the sweep working and the
membership still read from the map, the run said

```
a region was retired while its record still names a peer on this store; its range is left alone
```

and the test failed on the reclamation assertion. The second gate is that no region this store
still hosts may overlap the range, which is what catches a stale record — a parent narrowed by a
split would otherwise delete its child's keys.

### The test, and the two reds

`crates/esker-store/tests/retire.rs::a_removed_peer_reclaims_the_range_in_every_column_family`.
1.2 s, three consecutive runs, and it asserts both halves of the rule: the shed store's range is
empty **per column family**, and the store that still hosts the region is unchanged, counted per
family and read back through the front door. Counting the families before the removal is what makes
"empty afterwards" mean *emptied* rather than *never filled*.

Red twice, each red isolating one half of the fix:

1. without the sweep — `timed out waiting for the second store to stop hosting the region`;
2. with the sweep and without the authoritative membership — `timed out waiting for the shed range
   to be reclaimed`, with the gate's own warning above it.

The probe the assertions are built on is new: `esker_store::snapshot::key_counts` answers how many
keys each shipped column family holds in a range. `clear_range`'s own emptiness check is the one-bit
version, and one bit cannot tell a "two of three column families" miss from a clean sweep — which is
the shape [ADR 0032](../adr/0032-a-snapshot-carries-every-column-family.md) found in the snapshot
stream this week.

### Left owed, deliberately

A retired region's **columnar runs** are not reclaimed. They are immutable files under
`<data_dir>/columnar/<region_id>/` swept by their own manifest, and removing a tree of them needs a
`FileSystem` capability the trait does not have — `delete` takes a file, and nothing answers "is
this a directory". It is derived, rebuildable state and only exists on a store that held a columnar
learner, so it is a smaller leak than the one this unit closes. Adding `remove_dir_all` to
`FileSystem` touches four implementations and belongs with inventory #10, which is the other reason
to open that trait.
