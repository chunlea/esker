# 0118 — A counter is placed at a door, and counts events

Status: **Proposed**, 2026-09-17 — debt [#109](../plans/debts-v1.1.md), the number issued by the
coordinator. **This page stops at the design.** Nothing below is built; what follows is where a
counter belongs, what it may count, what each kind costs, and the test that has to go red when it
stops counting.

## Context — the premise this page opened on has been refuted

0118 was opened to answer a question from #109: an insert costs more the more the store already
holds, and after every `Txn` method was timed a **44–46% remainder** was left that the ledger could
not account for. The fact-finding page (`esker-coord/s1-fill-window-2026-09-16/q118-facts.md`)
recommended **not** adding store-side counters, and its reason was:

> the 46% is the SQL node's own work *between* two `Txn` calls — it does not pass through the store
> at all, so a store-side counter would produce an honest set of numbers unrelated to the remainder.

**A sampling profile of the 20k tier refuted that reason** (`q109-profile.md` §2; 80,083 samples,
0 lost). `tokio-rt-worker` holds 81.9% of the samples, and nearly every symbol inside it belongs to
`esker_engine` — the arena skiplist, `extract_user_key`, `MemTable::store`. In this **single-process**
fixture the store's apply path and the engine's writes run on the tokio workers, and the named
`raft-driver-*` threads are only a part of the store's work. So the remainder is **not** "the SQL
node's own work"; it contains engine work.

**And the opposite conclusion does not follow either.** In production the store is a different
process, so the same share is a **network round trip**, not local engine time. The ledger's
remainder, measured in this fixture, describes neither side's production cost.

So the question this page answers is no longer *where does the time go* — a profiler answers that,
and one just did. It is: **which events should this system count, permanently, so that the next
question of this kind can be answered without a profiler.**

## ① What a counter may count: events, not time

**Events**, because an event survives the move from fixture to production: a round trip is a round
trip whether the store is in this process or across a network. Candidates, all of which a future
question would ask for:

- round trips per statement, and the regions they addressed;
- keys and bytes a scan walked, against the keys it returned;
- lock encounters, with the verdict attached (still inside its lease, past it, already resolved);
- retries, and which class of refusal caused each.

**Not time.** The profile above is exactly why: the same work is local engine time in the fixture
and a network round trip in production, so a *duration* counted here is not a duration there. Time
is a profiler's answer, taken per build and per shape, and read with the build's limits attached —
this round's own numbers are a dev build's, and about 8.9% of that profile is debug assertions that
release removes.

**The door for this already exists and is in use.** `ScanStats` crosses the wire per request today
and is what `EXPLAIN ANALYZE` reports. This page does not invent the mechanism; it says what may
travel through it.

## ② Where a counter goes: at a door, not on a thread

A **door** is a code location where a request crosses a boundary — the `Txn` seam, the wire call in
the client's router, the fragment service's entry, the engine's read and write entries. A counter
named for its door means "how many times this boundary was crossed", which stays true however the
threads are arranged.

**A thread name is not a boundary, and that is this page's hardest-won line.** The instrumentation
plan written before the profile said to separate the store from the SQL node by excluding the store's
thread names. The profile showed that filter is wrong in both directions: `tokio-rt-worker` carries
SQL work *and* engine work, while `raft-driver-*` carries only some of the store's. A counter placed
by thread would have reported a split that does not exist.

## ③ What each kind costs

| Kind | Where it is read | Cost |
|---|---|---|
| **Aggregated** — a running total per store, per day, read from a log line or a diagnostic | Out of band | **No wire change.** This is the cheap kind, and every candidate in ① except the first two is answerable this way |
| **Carried back per request** | In the response | **A destructive wire change**: `TxnKvResp`'s eleven variants carry no statistics, so adding one is a format change with a golden, which `CLAUDE.md` reserves for the human. [ADR 0117](0117-a-fragments-refusal-carries-the-keys-that-stopped-it.md) already needs one; a second would share that bump rather than pay for its own |

**So the standing recommendation survives the refutation of its old reason, with new wording.** The
old reason ("the remainder does not pass through the store") is withdrawn. The new one is narrower
and checkable: **no store-side event identified today needs to be carried back per request.** The
lock-encounter count that [#88](../plans/debts-v1.1.md) wants is a *rate* over days, which the
aggregated kind answers; the engine's service time is *time*, which ① excludes.

**The counterexample this claim would die to, stated so a reader can hunt it:** an event a *single*
statement must react to, that only the store can see. If one is found, this recommendation is wrong
and the wire bump is owed. `ScanStats` is the existing example of exactly that shape — which is why
the claim is "no **new** event needs to cross", not "nothing crosses".

## ④ Relation to 0117, and the threshold registered before any number

0117's third question — *of the locks a fragment meets, what share belong to transactions that have
already finished* — cannot be answered by a test. Any test writer's constants decide that share
(`q117-ratio-design.md`, the correction to §④), and the control arm that would have validated a
measurement cannot be built at all (`q117-planter-result.md` §10). **The only instrument that can
answer it is the aggregated lock-encounter counter of ①**, read over real traffic.

**Registered before the counter exists**, so that its output cannot be read into whatever is
convenient:

- The counter is **worth building** only if the workload produces a usable sample within a week.
  #88 measured roughly **one encounter per 20–30 scans**, so a workload of 10,000 scans a day yields
  300–500 encounters a day. That clears it by two orders of magnitude, and the threshold is recorded
  here so that a workload which does *not* clear it is recognised as such rather than waited on.
- The counter **changes 0117's ruling** only through the share of encounters whose verdict is
  *finished*: option (b′)+(c) buys the fallback back for that share and one round trip for the rest.
- **A share near zero is a result, not a failure**: it rules the mechanism out, which is the outcome
  0117 has been unable to reach for three windows.

## ⑤ The shape of the test that must go red

A counter with no test that reddens when it stops counting is a decoration. The shape is **not**
"assert it is greater than zero" — that passes on a counter wired to the wrong door. It is:

> perform a **known** number of door-crossings, and assert the counter equals that number.

For the lock-encounter counter the fixture already exists, and there is an inversion worth stating.
0117's planted-lock fixture was **void as a measurement** because its population is synthetic — the
share it produced was a property of the planter's constants. **That same fixture is exactly right for
testing a counter**, because the question changes: not "what does the world look like" but "did you
count what I planted". The constants stop being a confound and become the ground truth.

The same rule covers the other candidates: a statement over a known number of regions must report
that many round trips; a scan over a known range must report the keys it walked, not the keys it
returned ([#58](../plans/debts-v1.1.md)'s shape, where steps grew and results did not).

## ⑥ What is decided here, and what is not

**Decided**: counters are placed by door and count events; aggregated counters need no ruling;
a per-request store-side counter needs a wire bump and is not owed by anything identified today.

**Decided 2026-09-17**: the lock-encounter counter of §① **is to be built** — 0117's option B.
It is the **aggregated** kind: node-local, and it does not cross the wire, so §③ owes it no format
change. What it produces is read under the threshold registered in §④ and over an observation
period, not statement by statement.

**This page stays Proposed until that counter lands**; it turns Accepted when the counter and the
test of §⑤ exist, because until then what is written here is a design and not a description.

**Not attempted**: any measurement of where the time goes in production. The profile in
`q109-profile.md` describes a dev build of a single-process fixture, and the next table that would
describe production is the same test under `--release`, which is a window of its own.

**This page stops at the design.**
