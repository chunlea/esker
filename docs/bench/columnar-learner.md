# Columnar learner — what the apply target costs and what the read buys

`esker-store`'s columnar apply target and `esker-columnar`'s merged read.
[ADR 0022](../adr/0022-columnar-learner-replica.md) milestone 3. Plan:
[`docs/plans/phase-8-learner.md`](../plans/phase-8-learner.md).

Not a gate. `CLAUDE.md` keeps benchmarks runnable and recorded so regressions are visible, and out
of the test gate so nobody tunes before correctness is proven. `#[ignore]`d, run deliberately:

```text
cargo test --release -p esker-store --test bench_columnar \
    -- --ignored --nocapture --test-threads=1
```

`--test-threads=1` is not optional: the three cases each push rows through an engine, and run
concurrently they measure each other rather than their subjects.

## Run 1 — 2026-08-31, 20,000 rows, `id int8` + `name text`

```text
learner fragment scan, 1 of 2 columns              8.69 M rows/s       2.30ms
voter row scan, whole row                          4.38 M rows/s       4.57ms

columnar apply + seal                              0.84 M rows/s      23.91ms
row apply                                          0.00 M rows/s      95.94s

visible scan across 1 run(s)                       5.78 M rows/s       3.46ms
visible scan across 2 run(s)                       3.53 M rows/s       5.67ms
visible scan across 4 run(s)                       3.39 M rows/s       5.89ms
visible scan across 8 run(s)                       3.09 M rows/s       6.47ms
```

## The ingestion number is not a speedup, and must not be quoted as one

95.94 s for 20,000 single-row puts is **4.8 ms a row**, which is not the row engine writing. A
sample of the same benchmark at 200,000 rows put **2075 of 2114 stacks inside
`esker_engine::db::write::DbInner::commit_group` itself** — not the WAL flush (31 stacks), not the
memtable (1). Every single-row `put` forms its own commit group, and with one writer and nothing
to contend with it still pays for the coordination.

So the honest reading is *"this is what a one-entry-at-a-time apply costs on each side today"*,
which is the right question for a Raft apply loop, and **not** a claim about either engine's
throughput. Dividing one by the other produces a four-figure ratio that means nothing about
columnar storage and everything about a write path being used a row at a time.

### And the sharper reading, which is not "ingestion is slow"

**The run had `WalSyncMode::Never`.** If durability waiting is switched off and `commit_group`
still holds 2075 of 2114 stacks at 4.8 ms a put — one writer, nothing to contend with — then
whatever it is waiting on, **it is not an `fsync`**. So the finding is not "fsync-per-write is
expensive". It is either that *a mode whose whole purpose is to remove the durability wait is not
removing it*, or that the cost is somewhere else entirely and the mode's name is a red herring for
whoever picks this up. Those are distinguishable, and the sample counts above are what
distinguishes them.

That is a correctness-of-configuration question about `esker-engine` and a more interesting one
than a slow path. Neither this lane nor `esker-sql`'s owns that crate, so it is recorded as a lead
rather than acted on — and recorded as a lead deliberately, because "ingestion is slow" is a shrug
and "`WalSyncMode::Never` may not be disabling what it names" is something somebody can pick up.

(Framing owed to lane wy-c2, who pointed out that the disabled-sync detail makes the number mean
something quite different from what this document first said about it.)

## The scan number, with the half that cuts the other way

**2.0×** for one column of two — 8.69 against 4.38 M rows/s. That is the shape ADR 0022's cost
rule predicts, and the reason it keeps point reads on a row replica.

It is also the flattering half, and [`columnar-m2.md`](columnar-m2.md) already records the other:
reading *every* column is **slower** columnar than row-wise (13.56 against 17.04 M rows/s there).
A projection of one column in two is close to the narrowest table where columnar can win at all;
the win grows with the columns a query does not read. Quoting 2.0× without that sentence would be
the same mistake as quoting m2's 3.5× alone.

## The merged path, which is the number this milestone owed

Resolving MVCC visibility is a property of the **region**, not of a file, so a read merges every
live run before it resolves versions ([`scan::merged`]). One run keeps the borrowed fast path;
several must materialise each row to merge them. Measured rather than asserted:

| runs | rows/s | against one run |
|---|---|---|
| 1 | 5.78 M | — |
| 2 | 3.53 M | 0.61× |
| 4 | 3.39 M | 0.59× |
| 8 | 3.09 M | 0.53× |

**The cost is the first step, not the count.** Going from one run to two costs 39%; going from two
to eight costs a further 12%. That is the borrowed-to-owned transition being the expense, and the
k-way merge itself being cheap — which is what the design predicted, and is worth having measured
because the opposite would have argued for a different structure entirely.

What it says operationally: compaction earns its keep by getting a region **off one run**, and
after that the marginal run matters little. A region that has drifted to eight runs is not in
trouble; a region that never compacts at all still pays only about half.

## What would move these numbers

* **The row-side ingestion figure** wants a batched write path, which is what the store's real
  apply loop has and what this benchmark deliberately does not use — it measures the same
  one-at-a-time shape on both sides so the comparison is like for like.
* **The merged path** materialises through `Value`, which owns its `Text` and `Bytea`. A borrowed
  merged row is possible and is a larger change than this milestone earned.
* **20,000 rows** is small, chosen so the row side finishes. The columnar figures are stable across
  sizes; the row side is linear in the same coordination cost throughout.

---

# The story end to end, on a real cluster — 2026-08-31

Not a benchmark. This is the transcript the phase-8 wiring lane's gate is written against
(`docs/plans/phase-8-learner.md` §wiring): a placement driver, four stores and a SQL node, all as
separate processes, with `ALTER TABLE ... SET (columnar_replicas = N)` as the only thing anybody
asks for. It is recorded because the numbers above say what a columnar replica costs and this says
what it takes to get one.

## Two commands and a psql session

```text
$ esker cluster start --nodes 4 --data-dir /tmp/smoke/cluster --base-port 21160 --pd
esker cluster: 4 nodes started
  placement driver on 127.0.0.1:21164 (pid 11471)
  node 1 on 127.0.0.1:21160 (pid 11472)
  node 2 on 127.0.0.1:21161 (pid 11473)
  node 3 on 127.0.0.1:21162 (pid 11474)
  node 4 on 127.0.0.1:21163 (pid 11475)
esker cluster: a SQL node over this cluster is
  esker-sql 127.0.0.1:5432 127.0.0.1:21160 127.0.0.1:21161 127.0.0.1:21162 127.0.0.1:21163 --pd 127.0.0.1:21164
```

**Four nodes, not three.** A columnar learner is placed on the healthiest store *without a peer*,
so a three-store cluster with a three-voter region has nowhere to put one.

```text
$ esker-sql 127.0.0.1:5442 127.0.0.1:21160 ... 127.0.0.1:21163 --pd 127.0.0.1:21164
INFO esker_sql: connecting to the cluster stores=[...]
INFO esker_sql: holding a schema lease pd=127.0.0.1:21164 lease_ms=5000 step_ms=8000 removal_extra_ms=3600000
INFO esker_sql: re-driving orphaned schema-change jobs step_ms=8000 removal_extra_ms=3600000
INFO esker_sql::pgwire::server: esker-sql is listening address=127.0.0.1:5442
```

Those two middle lines are the whole of this lane. Before it, the second said *"no schema step
interval published to this node"* and the first did not exist: the lease was never fetched, so
fail-closed never armed, and the re-driver ticked against nothing.

```text
$ psql postgresql://esker@127.0.0.1:5442/esker?sslmode=disable
CREATE TABLE readings (id int8 PRIMARY KEY, sensor text NOT NULL, value int8);
INSERT INTO readings VALUES (1, 'north', 10), (2, 'south', 20), (3, 'east', 30);
SELECT * FROM readings ORDER BY id;
 id | sensor | value
----+--------+-------
  1 | north  |    10
  2 | south  |    20
  3 | east   |    30
(3 rows)

ALTER TABLE readings SET (columnar_replicas = 1);
SELECT * FROM esker_columnar_replicas();
  table   | columnar_replicas
----------+-------------------
 readings | 1
(1 row)
```

## What the cluster did about it, without being asked again

```text
$ esker region ls --pd 127.0.0.1:21164          # before
  region       epoch  peers
       1     5,1      *2@127.0.0.1:21161 3@127.0.0.1:21160 4@127.0.0.1:21162

$ esker region ls --pd 127.0.0.1:21164          # after
  region       epoch  peers
       1     8,1      *2@127.0.0.1:21161 4@127.0.0.1:21162 5C@127.0.0.1:21163 6L@127.0.0.1:21160

1 regions; * is the leader, L a learner, C a columnar learner
```

`5C` on `127.0.0.1:21163` is the columnar replica, on the store that had no peer — placed because
a `psql` statement said so and for no other reason. Setting the flag back to `0` takes it away
again, because a report is a full assertion and a table set to zero is simply absent from the next
one:

```text
ALTER TABLE readings SET (columnar_replicas = 0);

$ esker region ls --pd 127.0.0.1:21164
       1     9,1      *2@127.0.0.1:21161 4@127.0.0.1:21162 6L@127.0.0.1:21160
```

## How long it takes, and why

Two minutes to place, five to retire — and none of it is the SQL node, which reports in the same
millisecond the `ALTER` commits. PD can only reach a store by **answering its region heartbeat**,
and a real store sends one every 60 s (`esker_store::REGION_HEARTBEAT_MS`, `docs/DESIGN.md` §14).
Every operator therefore costs a heartbeat, and the removal below cost five minutes because it
queued behind an operator that had to time out first. The in-process gate does the same sequence in
under four seconds with `region_heartbeat` at 20 ms, which is the same code and a different clock.

## The defect this transcript found

`esker pd inspect` after the run, with the history PD keeps of its own operators:

```text
regions (1)
     1  [, +inf)  epoch (9, 1)  leader 2  term 1  applied 23
        peer 2 on store 2 (Voter)
        peer 4 on store 3 (Voter)
        peer 6 on store 1 (Learner)

operator history (12)
   1788233801172 ms  region 1  AddPeer     issued     store 1  peer 3
   1788233801375 ms  region 1  AddPeer     done       store 1  peer 3
   1788233801375 ms  region 1  AddPeer     issued     store 3  peer 4
   1788233801874 ms  region 1  AddPeer     done       store 3  peer 4
   1788233921876 ms  region 1  AddLearner  issued     store 4  peer 5
   1788233921975 ms  region 1  AddLearner  done       store 4  peer 5
   1788233921975 ms  region 1  RemovePeer  issued     store 0  peer 3
   1788233922074 ms  region 1  RemovePeer  done       store 0  peer 3
   1788233922074 ms  region 1  AddPeer     issued     store 1  peer 6
   1788234222175 ms  region 1  AddPeer     timed out  store 1  peer 6
   1788234222175 ms  region 1  RemovePeer  issued     store 0  peer 5
   1788234222276 ms  region 1  RemovePeer  done       store 0  peer 5
```

**A voter went in the same millisecond the columnar learner landed.** No store was down — all four
were heart-beating throughout, and `schedule::repair_for` only removes a *dead* peer. What did it is
`balance::region_balance`, which asks `region.peers.len() > cluster.target_replicas` over **every**
peer: a healthy three-voter region that gains a columnar learner is four peers against a target of
three, so balance sheds "the replica on the busiest store" and repair has to put one back.

That is the third instance of the family wave A named — *a count taken over `peers` rather than
over voters* — after `schedule::urgency_for` and `schedule::repair_for`, both of which were fixed
and both of which read healthy on a cluster that was not. The cost here is not cosmetic: the region
sat at **two** voters for the five minutes the replacement's `AddPeer` took to time out, one
failure from losing quorum, and every other operator for that region — including the removal the
next `ALTER` asked for — waited behind it.

Reproduced in under three seconds, in process, as
`esker-sql/tests/joint_gate.rs::a_columnar_learner_does_not_cost_the_region_a_voter` — left red on
purpose for whoever owns `esker-pd`, and **fixed in 571b2b0**: `balance::region_balance` counts
voters now, on both the shed decision and the "is this region worth moving" one.

It is why PD's own columnar tests run with `balance: false`, and why the gate beside it does too:
the harness that would have caught this had the switch turned off. Those switches stay — a balance
move arriving in the same slot makes "PD asked for nothing" and "PD asked for something else"
indistinguishable, which is the assertion half those tests rest on — and the case they hid has its
own test beside them, with balance on.

# The same run, after wave A closed — 2026-09-01

Same commands, same four nodes, one region, one `ALTER`. What is different is everything the
operator history does *not* say:

```text
$ esker region ls --pd 127.0.0.1:21264
  region       epoch  peers
       1     6,1      *2@127.0.0.1:21263 3@127.0.0.1:21260 4@127.0.0.1:21262 5C@127.0.0.1:21261

$ esker pd inspect --data-dir .../pd
     1  [, +inf)  epoch (6, 1)  leader 2  term 1  applied 16
        peer 2 on store 4 (Voter)
        peer 3 on store 1 (Voter)
        peer 4 on store 3 (Voter)
        peer 5 on store 2 (ColumnarLearner)

operator history (6)
   1788250747608 ms  region 1  AddPeer     issued     store 1  peer 3
   1788250748010 ms  region 1  AddPeer     done       store 1  peer 3
   1788250808010 ms  region 1  AddPeer     issued     store 3  peer 4
   1788250808809 ms  region 1  AddPeer     done       store 3  peer 4
   1788250868811 ms  region 1  AddLearner  issued     store 2  peer 5
   1788250868911 ms  region 1  AddLearner  done       store 2  peer 5
```

**Six operators and not one `RemovePeer`.** Three voters and a columnar learner, which is what was
asked for; the run above shed a voter in the millisecond after `AddLearner done` and spent five
minutes replacing it. `PdOptions::balance` is on by default, so `esker pd serve` is balancing
throughout — this is the rule running and declining to act, not the rule switched off.

## What the learner received, which is the part that was empty

The blocker `docs/plans/phase-8-learner.md` §store left — *a PD-placed columnar learner never
receives the region's existing data* — is a snapshot that carried one column family out of three
([ADR 0032](../adr/0032-a-snapshot-carries-every-column-family.md)). The learner's own write-ahead
log is where that shows, because a store this size has flushed nothing yet:

```text
$ esker wal-dump .../node-2/000005.wal
  1            52      611  seqno 2        9 entries
         cf=2   Put  seqno 2   "xmsqll   ÿ…"                       (15 value bytes)
         cf=2   Put  seqno 3   "xmsqln …reaÿdings …"                  (15 value bytes)
         cf=2   Put  seqno 4   "xmsqln …reaÿdings_pkÿey …"         (15 value bytes)
         cf=2   Put  seqno 5   "xmsqls …"                                   (13 value bytes)
         cf=2   Put  seqno 6   "xmsqlt …"                                   (70 value bytes)
         cf=2   Put  seqno 7   "xmsqlv …"                                   (13 value bytes)
         cf=2   Put  seqno 8   "xt …r     ÿ …" (30 value bytes)
         cf=2   Put  seqno 9   "xt …r     ÿ …" (30 value bytes)
         cf=2   Put  seqno 10  "xt …r     ÿ …" (29 value bytes)
  2           670       82  seqno 11       3 entries
         cf=3   Put     seqno 11  "m       "          (19 value bytes)
         cf=3   Put     seqno 12  "s       "          (13 value bytes)
         cf=3   Delete  seqno 13  "p       "
```

`cf=2` is `write`; the keys are the `'x'` namespace. Six catalog records and the three rows
`readings` holds, arriving in one batch — the snapshot — followed by the adopt: the region record,
the raft state, and the deletion of the pending-snapshot marker, which are the last two of the
four durable steps. **Under version 1 of the stream this file would have had nine fewer records
and the learner would have adopted an empty region**, because the walk was `default` under `'r'`
and this region has nothing in `'r'` at all.

## What a fragment answers, and why that number is not here

Nothing on a real cluster can ask one yet: no `esker` subcommand sends a `Fragment`, and the SQL
planner does not choose a columnar replica — that is a later phase, and it is the honest gap in
this transcript. The differential that closes wave A therefore runs in
`esker-sql/tests/joint_gate.rs::the_learner_answers_fragments_that_agree_with_a_row_scan`, which is
a real `esker-pd`, four real stores over real sockets and the real SQL node wiring in one process:

```text
running 5 tests
test a_columnar_learner_does_not_cost_the_region_a_voter ... ok
test a_fragment_is_refused_by_a_voter_and_by_a_learner_that_is_behind ... ok
test a_learner_that_dies_comes_back_to_the_same_job ... ok
test an_alter_places_a_columnar_replica_and_clearing_it_takes_it_back ... ok
test the_learner_answers_fragments_that_agree_with_a_row_scan ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.35s
```

Nothing `#[ignore]`d, which is the sentence wave A was missing. The fragment resolves visibility in
`esker-columnar`'s evaluator over the learner's runs; the reference reads the same instant through
Percolator's `write` records on a voter and decodes with the row codec, and the two agree over a
corpus with an update, a delete, and an `ADD COLUMN region text NOT NULL DEFAULT 'unknown'` with
rows on both sides of it — the case that reads the default row-side and `NULL` columnar-side if
anything in the path forgets the column's missing value. Something did: the fragment service passed
no widening at all, so it read every run as the *oldest* one's schema and refused the wider ones
outright (`bfab24f`).

## One more thing the real binaries said

`esker cluster start --nodes 4 --pd` started its placement driver and then spawned all four stores
without waiting for it to bind. Three of them exited immediately:

```text
esker server: opening …/node-2: request not sent: connecting to 127.0.0.1:21264:
              Connection refused (os error 61)
esker pd: listening on 127.0.0.1:21264 — no cluster yet, waiting for a store to bootstrap one
esker server: esker server: opening …/node-3: request not sent: connecting to 127.0.0.1:21264:
              Connection refused (os error 61)opening …/node-1: … Connection refused (os error 61)
esker server: store 4 listening on 127.0.0.1:21263
```

Three "connection refused" lines *above* the driver's own "listening", and the fourth node — the
last one spawned — through. `cluster.rs`'s comment says it two lines above the loop: *"The driver
first, because every store below is about to ask it whether to bootstrap. A store whose PD is not
up yet fails to open"*. Starting the driver first is not the same as waiting for it.

None of it is visible while it happens. The children inherit the supervisor's stdio and the
supervisor then blocks in `wait_for_interrupt`, so the announcement ("4 nodes started", with four
pids) is printed and the failures behind it are not flushed until the whole thing is stopped — and
two of them interleave mid-line, as the third line above shows. What is left running is one store,
which registers, bootstraps region 1 alone and heartbeats happily: a cluster that looks up and is a
quarter of one. Started by hand with a second between the driver and the stores, the same binaries
formed the cluster above in under a minute.

Not fixed here: `esker-cli` is another lane's. Recorded because a smoke command that silently
starts one node out of four is worse than one that fails, and because this is the second time the
harness was the thing that was wrong.
