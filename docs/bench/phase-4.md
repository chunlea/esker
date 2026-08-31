# Phase 4 — the store lane's numbers: scale-out, repair, split, balance, chaos

Recorded as `docs/bench/README.md` describes: one file per phase, appended to rather than
overwritten. **Not a gate.** Recorded by the `p4-accept` lane as part of the phase-4 acceptance
battery (`prompts/04-multiraft-pd.md` Acceptance, `docs/plans/phase-4.md`). The placement driver's
own allocator numbers (`tso`/`allocid`) are `docs/bench/phase-4-pd.md`'s, recorded by the
`cl-p4-pd` lane; this file is the store side: `fillrandom` at 1/3/5 stores, region counts, split
distribution, repair, and the chaos run substituting for the deterministic multi-store simulator
(`docs/plans/phase-4.md` §15 records that sim as debt this lane does not own).

## Method common to every run below

| Field | Value |
|---|---|
| commit | `7e77058` (esker-store/esker-pd; esker-sql/esker-txn kept committing around this run without touching either crate) |
| machine | Apple M4 Max, 16 cores, 128 GiB, built-in NVMe SSD (APFS) — same machine as phases 1–3 |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| build | `cargo build --release`, the workspace's own `esker-store`/`esker-pd`/`esker-proto`/`esker-client`/`esker-base` |
| **not idle** | Four other agent sessions were active on this machine for the whole of this run, each building/testing the same workspace concurrently (`uptime` load average measured 5–8 throughout, against 16 cores) — unlike phases 1–3's dedicated-machine runs. Every number below should be read as a lower bound on what a dedicated machine would show, not a ceiling |
| harness | `esker-cli bench`/`raw` have no PD-aware routing — `bench --remote` binds one static region (`StaticRegion`) and cannot follow a real cluster's splits or leader moves. A scratchpad-only tool (never committed; per this lane's mandate to write only the two files in `docs/bench/`) supplies a `RegionResolver`/`StoreTransport` backed by a live placement driver and otherwise reuses `esker_client::RawClient` as-is — no retry/backoff/epoch logic was reimplemented, only "how do I reach a store" and "who do I ask." Verified by reading, not by the tool that wrote it: see the Methodology note at the end of this file |
| server knobs | `esker-cli server`/`pd serve` do not expose `region_split_size` or the heartbeat intervals; a second scratchpad wrapper (`storewrap`/`pdwrap`) is `esker-cli`'s own `server.rs`/`pd.rs` with those `StoreOptions`/`PdOptions` fields exposed as flags, otherwise identical |
| topology | every store is started with every store's address in its `--peer` list from the start (the *address book*, `RaftOptions.peers`) even for a store not yet running — this is what lets a peer added later (by `AddPeer`) be dialed at all. Region membership itself is **not** set this way: PD's `Bootstrap` always answers with a single voter, on whichever store registers first (`docs/DESIGN.md` §7); growing that to `target_replicas` is `balance`'s job, not bootstrap's — see the finding below, which affects every run in this file |

## 1. Linear scale-out — 1, 3, 5 stores

**Knobs**: `region_split_size` 1 MiB (default is 96 MiB; lowered so a modest key count still
produces enough regions to spread), 300-byte values, 15,000 keys, 8 client threads, unsynced,
`region_heartbeat` 1 s / `store_heartbeat` 2 s (defaults are 60 s/10 s; lowered so this lane's short
runs could actually observe convergence). Two passes per configuration, mirroring
`bench_remote.rs`'s own untimed-populate/timed-workload split: an untimed **warm-up** pass writes
the 15,000 keys (triggering every split it will trigger), a 20 s **settle**, then a **measured**
pass writes the same 15,000 keys again (now against an already-split, already-settling key range)
and its throughput is the number reported.

| Stores | Regions after settle | Measured ops/s | p50 | p99 | Verify |
|---:|---:|---:|---:|---:|---|
| 1 | 8 | 255.2 | 25.5 ms | 72.0 ms | 14,977/14,977 readable |
| 3 | 8 | 194.4 | 28.5 ms | 168.2 ms | 14,955/14,955 readable |
| 5 | 8 | 175.7 | 29.9 ms | 157.2 ms | 14,975/14,975 readable |

**The curve is falling, not rising — reported honestly, per this lane's instructions, rather than
tuned until it looked like the headline.** Three things are true at once and none of them is a
correctness defect:

1. **1→3 is a replication-cost comparison, not a scale-out comparison, and phase 3 already named
   this cost.** With `target_replicas = 3` (default, unchanged), 3 stores means every region sits
   on *every* store — there is no spreading to benefit from, only the quorum round trip phase 3's
   own bench recorded (`docs/bench/phase-3.md`: a 3-node cluster is 400×+ slower unsynced than a
   lone voter). 1→3 here shows exactly that cost again, at the throughput a real region-split
   workload produces rather than one hand-seeded region.
2. **A newly bootstrapped region starts at one voter, full stop, and growing it to
   `target_replicas` is `balance`'s job — which needs more than one region to act at all.** PD's
   `Bootstrap` gives region 1 a single voter on whichever store registers first
   (`docs/DESIGN.md` §7); nothing in bootstrap grows it further. Region-count balance moves a
   replica only when the gap between the busiest and quietest store is **at least two**
   ([ADR 0018](../adr/0018-balance-moves-the-spread-by-two.md)) — a rule that cannot fire at all
   while the cluster has exactly one region, since the largest possible gap then is one. Every run
   in this file only reaches multiple regions, and therefore only becomes capable of reaching
   3× replication, once splits have happened — so the *early* part of every run is unavoidably
   under-replicated, and how long that lasts depends on how fast the first region grows past the
   split threshold.
3. **Within the settle windows used here, replicas were still catching up as *learners*, not yet
   promoted to voters, and leadership had not moved at all** — see the repair and balance sections
   below, where this is shown directly. Five stores paying the coordination cost of five stores
   (more heartbeats, more addresses, more Raft messages between store pairs) while the *benefit*
   (independent regions actually led from different stores) had not yet materialized is the
   plausible shape of the loss from 3→5, on top of 1→3's already-paid quorum cost.

None of this contradicts the architecture — DESIGN §7 states plainly that repair adds a learner
first and PD promotes it once caught up, and that balance acts on a spread gap, not on
under-replication directly. What it means is that **this run does not demonstrate linear scale-out
as the acceptance prompt asks for**, and the honest reason is a combination of environment (a
shared, contended machine; short settle windows chosen to fit this lane's time budget) and a
mechanism this lane did not appreciate going in (single-region bootstrap never grows on its own).
A dedicated machine and a run long enough for full 3× promotion and several splits' worth of
region-count spread — minutes, not tens of seconds, per the balance section below — is what it
would take to see the rising curve the prompt expects. **Recorded as a gap against this
criterion, not papered over.**

**Every acknowledged write was readable in all three configurations, with zero mismatches.** A
small (0.1–1%, falling as the split threshold was raised across tuning attempts) rate of individual
`put` calls failed outright under heavy concurrent splitting rather than succeeding slowly; every
one was reported to the caller (never silently dropped), and is excluded from the ack log verify
checks by construction.

## 2. Repair end-to-end — 3 stores, kill one permanently, add a 4th

**Knobs**: same split/heartbeat knobs as above; `max_store_down_time` lowered to 5 s (default 30 s)
so this lane's run could fit the time available; `12,000` keys, 4 threads, 300-byte values.

Sequence: 3 stores + PD; write load; wait for the written regions to reach multiple stores;
`SIGKILL` store 3; immediately start store 4 (already in every store's `--peer` address book).

**Observed, precisely:**

- **Store 3's peer was removed from every region it had touched, and store 4 was added as a
  learner to each — both within the first post-kill heartbeat cycle (≈1–2 s).** The dead-peer
  cleanup and the "add the replacement" half of repair both fired immediately and correctly; no
  region was ever left referencing the dead store.
- **No region reached "3 full voters, none on the dead store" within the observation window**
  (150 s of polling after the kill, on top of a 90 s pre-kill wait that itself never got every
  region to 3 voters — most regions still showed the *original* store2 or store3 as a **learner**,
  not yet promoted, at the moment of the kill). The store that led before the kill kept leading
  throughout; the newly added learner (store 4) and the pre-existing learner never visibly promoted
  to voter in this run.
- **Every one of the 11,953 acknowledged writes from before the kill was still readable
  afterward — zero data loss, on the dead store, the surviving stores, or the new one.**

**Read together with the scale-out section's finding #3, this is the same phenomenon from the
other side: learner promotion to voter, in a live PD-driven cluster under this lane's short
observation windows, was not observed to complete.** DESIGN §7's promotion criterion is
`docs/plans/phase-4.md` §13.4/§14.5's "PD's call, re-issuing the same `AddPeer` once the heartbeats
show the learner caught up" — a re-decision on PD's own schedule, gated by
`operator_timeout`/`balance_cooldown` and by the learner's own applied index actually catching up
via real log replay (or a snapshot) over the real network, on a machine sharing its CPU with four
other agents. This lane cannot distinguish, from the outside, "promotion is simply slower under
real wall-clock heartbeats and contention than the minutes this run allowed" from "something about
promotion is stuck" — **the repeated, consistent shape of the finding across an independent
repair run and an independent balance run (below) is worth the coordinator's attention**, but this
lane's mandate is to report the evidence, not to instrument the store to tell them apart.

## 3. Split-under-load, and balance convergence (1 store, then add 4)

**Split-under-load** is not re-run as a separate exercise: the scale-out section above already is
one — 15,000 continuous, unsynced, concurrent writes across a region that split repeatedly (to 8
regions, twice per configuration, six times total across the three configurations), with every
acknowledged write verified readable and zero mismatches every time. That is strictly more splits
under strictly more concurrency than the store-level `tests/split.rs` battery's "ten splits"
(confirmed still green at HEAD — see the main report's item 1), at the scale this lane could afford
on a shared machine, over the real network rather than the in-process harness.

**Balance convergence — knobs**: `region_split_size` 256 KiB (lower than the scale-out runs, to get
several regions from a modest 8,000-key population fast), same heartbeat settings, `8,000` keys on
1 store, then stores 2–5 started together.

Sequence: 1 store + PD; write 8,000 keys (reached 16 regions, all on store 1, all with a single
voter — see finding #2 above, this is bootstrap's shape); start stores 2, 3, 4, 5; poll for region
counts to spread within a gap of 1 across all five stores.

**Observed:** after 240 s of polling, **the spread had not converged** — every leader was still on
store 1 (16/16), and the final listing showed peers being added as learners on stores 2/3/4/5
(confirming the address book and repair-adjacent machinery is reaching them) but not yet promoted.
`docs/DESIGN.md` §7's "balance moves the spread by two" logic also decides region count *before*
leader count, and a replica move is add-then-remove — so with every replica move still sitting at
"learner added, old peer not yet removed" (the majority state observed here), no leader has
anywhere new to go yet. **This is the same "promotion has not been observed to finish" finding as
the repair section, from the balance side of the scheduler.** Every one of the 7,574 acknowledged
writes remained readable throughout.

## 4. Invariant chaos — 5 stores, low split threshold, kill/restart, real processes

The deterministic multi-store simulator prompts/04 asks for (5 simulated stores, 50 regions,
100,000 events per seed) is recorded debt (`docs/plans/phase-4.md` §15) that this lane does not
own; this run is its real-process substitute, per this lane's brief.

**Knobs**: 5 stores + PD, `region_split_size` 128 KiB, `max_store_down_time` 6 s, continuous
`fillrandom` writes (300-byte values, 6 threads) targeted for the run's whole duration. **Scaled
down from the ~5 minutes asked for to 150 s wall clock, in the time this lane had left** — 4
kill/restart cycles (one store per cycle, round-robin, 5 s down before restart, ~28 s of running
time between cycles) rather than continuously for the full window.

**Every invariant check — at t+15s before any kill, and after each of the 4 kill/restart
cycles, and once more at the end with every store back up — passed cleanly:**

| Checkpoint | Regions | Contiguous | Two-peers-one-store violations |
|---|---:|---|---:|
| t+15s, before any kill | 8 | yes (walk completed, no gap) | 0 |
| after killing/restarting store 2 | 8 | yes | 0 |
| after killing/restarting store 3 | 8 | yes | 0 |
| after killing/restarting store 4 | 8 | yes | 0 |
| after killing/restarting store 5 | 8 | yes | 0 |
| final, all 5 stores healthy | 8 | yes | 0 |

Contiguity is checked the same way `esker-cli region ls` checks it: a `GetRegion` walk from `""`
that fails loudly on any gap (none occurred, at any checkpoint); "no two peers of one region on one
store" is checked from PD's own region records (the routing table), by parsing each region's peer
list for a repeated store id (none occurred, at any checkpoint). This lane did not additionally
read each store's raw `'m'` records off disk — no tool exists to dump that format the way
`sst-dump`/`wal-dump`/`manifest-dump` do for the engine's own formats, and PD's routing table is
itself built from real per-store heartbeats, which is the aggregate view the brief asks this
invariant be checked against.

**The continuous-write part of this run did not sustain for the full 150 s.** The scratchpad
harness's placement-driver connection has no reconnect logic (unlike its per-store connections,
which do reconnect on error — see the Methodology note); after roughly 20 s something broke that
one connection and every subsequent write failed for the remainder of the run. **2,441 writes were
acknowledged in that first ~20 s, and every one of them was still readable at the end, after the
four kill/restart cycles — zero data loss.** This is a limitation of this lane's own verification
tool, not a product finding: the same tool sustained hundreds of successful writes per second for
40–130 second populate phases in every other section of this file without the connection breaking,
and the one thing different about the chaos run is that it is the only one that kills *stores*
repeatedly while the *client* keeps running against them, which is exactly the condition this
lane's brief asked for and exactly the condition this lane's own tool turned out not to be built to
survive. Reported rather than quietly re-run into a nicer number.

## Run 2 — 2026-08-31, comparable region counts, and a harness bug fixed

commit `2e109dd4` (esker-store/esker-pd unchanged since Run 1's `7e77058`; only esker-sql kept
moving HEAD between the two).

Reactivated after Run 1 with two directed corrections: the store lane's own retest of the Run 1
harness hit `p4loadgen` failing 10,652/15,000 warm-up puts with `no address known for store N`,
and Run 1's three configurations happened to settle at the same region count (8) but by accident,
not by construction — nothing pinned it, so a rerun was not guaranteed to compare like with like.
Both are addressed below before any new number is trusted.

### The address-book bug, and the fix

`PdResolver::get_region` populated `p4loadgen`'s store→address book from every `GetRegion`
answer, but `RawClient`'s `RegionCache` (production code, unmodified) caches a `Route` and reuses
it across many calls — a `NotLeader` redirect updates *which peer* it believes leads via
`set_leader` without re-fetching that region's `stores` list. A peer added to a region **after**
the cache's last real resolve (a fresh replica from repair or balance, most commonly) could
therefore be named by a stale cached route while the book had never heard its address — and unlike
production's `TcpStores`, whose fixed `--addr`-built book makes "no address known" genuinely
terminal, this book's membership changes mid-run, so failing immediately was wrong.

**Fix**: `DynamicStores` (the `StoreTransport` impl) now holds a shared handle to the same
`PdResolver` used for region resolution. On a book miss it extracts the call's own key via
`esker_client::wire::routing_key` (the identical helper the production router uses internally —
covers every `RawKvReq` variant, not just the `Get`/`Put` this harness happens to send) and issues
a **targeted** `GetRegion` for that exact key, bypassing the possibly-stale cache and going
straight to PD, before retrying the book — bounded at 8s total, retried every 150ms, so a genuine
registration race (the store hasn't reached PD yet) also resolves rather than failing fast.
Verified by a dynamic-join smoke test (a 2nd store started mid-run, immediately hammered) showing
zero `no address known` failures where the unfixed code would have had them, then confirmed at
scale across every run below: **zero `no address known` failures in any of the three
configurations, 100% of every acknowledged write verified.**

### The other bug found along the way: `esker-cli region split` cannot reach a real cluster

Pinning region counts by pre-splitting an empty keyspace needed `esker-cli region split`, which
failed on the first call: `request is for cluster 0, this peer serves cluster <real id>`.
`crates/esker-cli/src/region.rs`'s `walk`/`locate` call `esker_proto::pd::encode(0, ...)` with the
cluster id **hardcoded to `0`** and no discovery or retry — so `region ls`/`region
split`/`region transfer-leader` fail `ClusterMismatch` against any real bootstrapped cluster, whose
id is a `mix64` of bootstrap facts and is essentially never `0` (`docs/DESIGN.md` §7). This is a
genuine defect in the checked-in CLI, independent of any lane's WIP, and worth the coordinator's
attention: as shipped, none of the three `region` subcommands DESIGN §12 lists as deliverables
work against a live cluster. Worked around here, not fixed there (out of this lane's mandate and
write grant): `p4loadgen` gained a `split` subcommand reusing the same `PdResolver` that already
does cluster-id discovery for `load`/`verify`/`regions`.

### Method

Same machine, same `region_heartbeat`/`store_heartbeat` (1 s / 2 s) as Run 1. **Region count is now
pinned at exactly 12 for every configuration**, by pre-splitting the empty keyspace at 11 evenly
spaced boundaries before any data is written (`p4loadgen split`), rather than relying on an
organic size-triggered split to land on a comparable count by chance. `region_split_size` is set to
1 GiB (effectively disabling the automatic splitter) so the region count stays exactly what was
pre-split throughout the run. 300-byte values, 8 client threads, unsynced. 1- and 3-store used
20,000 keys; **5-store used 8,000** — the 3-store measured pass alone took 378s, and cutting the
op count for 5-store was the "quiet" call given the time available, trading absolute op count for
a still-meaningful rate over a shorter run. `uptime`'s load average is recorded at three points per
run (start, immediately before the measured pass, immediately after).

| Stores | Regions (pinned) | Keys measured | Voters entering measurement | Measured ops/s | p50 | p99 | Verify | Load avg (start → pre-measure → post) |
|---:|---:|---:|---|---:|---:|---:|---|---|
| 1 | 12 | 20,000 | 12/12 (trivial, 1 replica each) | **277.0** | 25.9 ms | 70.2 ms | 20,000/20,000 | 5.3 → — → — |
| 3 | 12 | 20,000 | 35/36 (full 3× on all but one region) | **52.9** | 136.0 ms | 371.7 ms | 20,000/20,000 | 6.7 → 7.9 → 5.8 |
| 5 | 12 | 8,000 | 26/38 (still catching up; store 4 never received a replica) | **71.3** | 49.8 ms | 855.9 ms | 7,997/7,997 | 5.8 → 5.4 → 4.3 |

**The first positive signal across every run this lane has made: 5 stores beats 3.** Not a clean
rising line from 1 — 3 remains far below 1, the same quorum-round-trip cost Run 1 and phase 3 both
already found, now measured with confirmed full 3× replication (peer listing: 12/12/11 across the
three stores, matching 12 regions × 3) rather than Run 1's partially-converged state. But **5 beats
3 by 35%, at exactly the point the mechanism says it should**: `target_replicas` is 3, so 3 stores
give every region nowhere to spread (every store already holds everything), while 5 gives regions
somewhere to go — and even the **partial** spread this run achieved (peers landed on 4 of the 5
stores; store 4 received nothing in this run's window; 26 of 38 peers were still learners, not yet
promoted, when measurement began) was already enough to claw back throughput the 3-store
configuration cannot reach. A run given enough time for full promotion and a fifth store actually
receiving load is the version of this experiment likely to show a further rise — not attempted
here on this lane's remaining time budget, and recorded as the natural next step rather than
claimed.

Every acknowledged write was readable in all three configurations, with zero mismatches — the
harness fix did not trade correctness for the lower failure rate; a handful (3, in the 5-store
run) of individual `put`s still failed outright under contention and were reported to the caller
rather than silently dropped, exactly as Run 1's did.

### How to reproduce

```sh
# p4loadgen and p4serverwrap are the same scratchpad tools described in Run 1's methodology note,
# with p4loadgen additionally gaining `split` (this run) and the PD-refresh fix above.
run_scaleout3.sh <n-stores> <pd-port> <base-store-port> <data-dir> <num-keys> <value-size> \
  <threads> <fixed-region-count> <spread-wait-seconds>
```

## Run 3 — 2026-08-31, confirming the landed PD fixes

commit range `8726a1a..f028d2e` landed between Run 2 and this run:
`8726a1a` fix(pd): a repair is not finished by a replica that cannot vote ·
`7316218` fix(pd): a retired operator's load outlives it ·
`1aadebf` fix(cli): esker region asks PD which cluster it is talking to ·
`93a0b77` perf(pd): retiring a settled correction is one question, not one per store ·
`f028d2e` docs(pd): [ADR 0023](../adr/0023-a-retired-operators-load-outlives-it.md).
ADR 0023 names the exact defect Run 2's 5-store run exposed: an operator's load-delta accounting
had a gap the width of one heartbeat, and under it PD grew a region to three replicas and then, in
one four-and-a-half-second thundering-herd cascade, took **all sixteen** of the bootstrap store's
replicas away — the mechanism-level cause of Run 2's "store 4 received nothing." A harness note
from the PD lane also flagged that Run 2's `max_store_down_time=8s` (against a 30s default, with a
2s store-heartbeat) manufactures spurious down-store detection under this machine's contention,
adding repair churn on top of whatever balance was doing. **This run corrects both**: rebuilt
`storewrap`/`pdwrap`/`p4loadgen` against the new commits, and `max_store_down_time` restored to the
30s default (30,000 ms; `operator_timeout` is left at 30s rather than the true 300s default,
because a stuck-operator recognition window of 5 minutes does not fit this lane's time budget —
flagged rather than silently kept, per the same standard just applied to the other knob).

Same pinned-12-region methodology as Run 2, 5 stores, 8,000 keys (matching Run 2's reduced 5-store
count for a fair before/after comparison).

| | Run 2 (pre-fix, `max_store_down_time=8s`) | Run 3 (post-fix, `max_store_down_time=30s`, the default) |
|---|---|---|
| ops/s | 71.3 | **86.6** |
| Voters entering measurement | 26/38 | 27/39 (comparable — convergence remains partial in both) |
| Peers per store | store1=12(9L+3V) store2=12(7L+5V) store3=7V store5=5V **store4=0** | store1=6 store2=12 store3=2 store4=12 store5=4 — **every store holds something** |
| Failures other than address-book | 3 | 0 |
| Load average (start→pre-measure→post) | 5.8 → 5.4 → 4.3 | 7.0 → 10.6 → 7.4 (measurably *more* contended machine, not less) |
| Verify | 7,997/7,997 | 8,000/8,000 |

**The store-emptying defect is gone, directly confirmed**: no store holds zero replicas in this
run, where Run 2 left store 4 completely empty. Throughput rose 21% (71.3 → 86.6 ops/s) **despite
a more heavily loaded machine during this run than during Run 2** (load average peaked at 10.6
here against 5.9 there) — the fix's effect is if anything understated by the raw numbers. 5 stores
now beats 3 stores (52.9, Run 2) by 64%, the clearest scale-out signal this lane has produced.
Voter convergence is still partial (27/39, materially unchanged from Run 2's 26/38) — the fix ADR
0023 describes is about the load-accounting race that emptied a store, not about how fast a
learner is promoted, so this is expected rather than a sign the fix is incomplete for what it
targets.

Two more spot-checks, made possible by rebuilding against the new commits, and directly relevant
to this record even though neither is the 5-store run this section is otherwise about:

- **`cargo doc -D warnings -p esker-client` now passes.** The broken intra-doc links this lane's
  first report flagged (`crates/esker-client/src/txn.rs` lines 9, 26) are gone at current HEAD —
  `just check`'s doc step, red for that reason in the first report, is clear of it now. (Not
  independently attributed to a specific commit here; whichever lane touched `txn.rs` since fixed
  it as a side effect or directly — this record only re-confirms the symptom is gone.)
- **`esker-cli region ls` now succeeds against a live, freshly bootstrapped cluster** (`1aadebf`,
  confirmed live: a 1-store cluster, `region ls` lists region 1 correctly, no `ClusterMismatch`).
  The workaround this lane built into `p4loadgen split` (Run 2) is no longer necessary for `ls`,
  though it is left in place here since `region split`/`region transfer-leader` were not
  individually re-checked and this lane's own `split` is already proven correct across three runs.

**What this run does not re-establish**: the item-3 repair scenario (kill store 3 permanently, add
store 4, time to full repair) and the item-4 balance-convergence scenario (1 store, add 4) were
run once each in this lane's first report, both showing the learner-promotion pattern ADR 0023's
fix is adjacent to but not identical with — `8726a1a`'s "a repair is not finished by a replica
that cannot vote" reads as directly relevant to that exact pattern, but this lane has not re-run
either scenario against the fixed code. This run's evidence is cross-cutting (the same
replica-growth machinery, exercised by 12 regions growing from 1 to 3 replicas apiece) rather than
a literal repeat of those two scripts, and is reported as exactly that — strong, relevant,
corroborating, not a substitute for re-running the two scenarios by name.

## Methodology note: what the scratchpad tooling is, and isn't

None of the four items above could be produced with the checked-in `esker-cli`: `bench`/`raw` only
ever resolve one static region, and `server`/`pd serve` do not expose the split-size or
heartbeat-interval knobs this lane needed to get a meaningful region count without minutes of
writing at the 96 MiB default. Two small scratchpad-only programs filled the gap (never committed,
per this lane's write restriction): a load generator that hands `esker_client::RawClient` a
`RegionResolver`/`StoreTransport` backed by a live PD instead of `StaticRegion`, and two thin
wrappers around `esker_store::Store`/`esker_pd::Pd` — themselves near-verbatim copies of
`esker-cli server.rs`/`pd.rs` — with a handful of existing `StoreOptions`/`PdOptions` fields exposed
as flags. Every retry, backoff, and epoch-invalidation code path above those two seams is the
product's own `esker-client`, unmodified. This lane read every line of both programs before
trusting a single number in this file, independently of the process that first produced them — see
this lane's report for why that independent read was necessary before any of the above was usable.
