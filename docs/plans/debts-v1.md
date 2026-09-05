# Debts still open at v1

**Status: one of the original eight is still open — #4.** Everything else has been
closed and moved to §2 with the commit that closed it, and the numbers are quoted across lanes, so
a closed row leaves a **gap** rather than renumbering the rows after it. Each remaining row names
its site, a size, and who it belongs to.

> **The original claim on this line was that every row had been verified against the tree rather
> than transcribed. That was true of the sites and not of the symptoms.** #3 and #5 were already
> fixed in the very commit this file was added at — #3's row even said c6 had verified it as the
> one item of eight that HEAD still owed — and **#2 makes six**: it was fixed by `8126d450` at
> 09:21 and this file was written at 09:48, twenty-seven minutes later. So six of the eight rows
> turned out to be closed when someone looked, not three. Asking "is the site still there" is not
> asking "is the symptom still there", and a register is only as good as the question put to it.
>
> **#1 failed the other way, and it is the cheaper failure to fix.** It was genuinely open when
> written, closed five hours later by `c77619ba` in another lane, and left standing here for a day
> — while [ADR 0067], written by the lane that closed it, opens with *"closes `debts-v1.md` #1 and
> #2"*. The closure was recorded; it was recorded somewhere this file does not read. A register
> that is only written to by the lane that opens a row will always lag the tree, so the rule that
> follows is the one at the top of §2: **close the row where the row lives, in the commit that
> closes it.**
>
> [ADR 0067]: ../adr/0067-the-check-mutation-and-the-latest-commit-question.md

Sources: the c6 wave's verification record (`debt-c6.md`), the coordinator's sightings, and the
code itself. `docs/acceptance/v1.md` carries the numbers; this file carries what is left.

---

## 1. Open

| # | Debt | Site | Size | Owner |
|---|---|---|---|---|
| 4 | **Cross-node deadlock detection.** The wait-for graph is node-local, which covers every deadlock two sessions of one `esker-sql` process can make. A cycle *across* nodes needs a graph both can see. Named in the code as a follow-on, and PD's job. | `crates/esker-sql/src/backend/locks.rs:46` | large — needs a PD-held graph | PD / pdha |

## 2. Reported as open, and closed on inspection

Recorded because the next reader will be handed the same list. **Close a row here in the commit
that closes it** — every lag this file has had came from the closure being recorded elsewhere.

### #1 and #2 — **closed, and the harder half was proving a test could tell**

| # | Closed by | Already in the tree this register was written against? |
|---|---|---|
| 1 | `c77619ba` *the read set reaches the store, and a loser cleans up after itself* — `StoreTxn::changed_since_statement` asks the store via `latest_commit` ([ADR 0067]) | No — five hours after, and left standing here for a day |
| 2 | `8126d450` *a savepoint must not turn validation off — two more methods the wrapper swallowed* | **Yes — by twenty-seven minutes** |

Verified by reading the tree, not the record: both sites override the default and the register was
describing a shape that no longer existed. What was genuinely missing was a **test that could tell
the difference**, and three were written and thrown away before one could:

| The test | Why it proved nothing |
|---|---|
| A savepoint open, two statements, no wait | Passed with `Recording`'s forward removed. READ COMMITTED hands the second statement a fresh snapshot, so it reads the new value whether or not anything was asked. |
| One writer genuinely blocked behind another | Passed with `StoreTxn`'s override removed. A statement that **waited** restarts unconditionally; the check is consulted only on the branch where the lock was taken at once. |
| The same contention at 4 × 15 | Passed with the override removed. The window is rarer than 60 increments. |

At 8 × 50 it separates: **1 refusal in 400 without the override, 4/4 runs clean with it**
(`tests/store_locking.rs`). And the assertion had to change shape as well as size — a lost update
is not reachable here, because the per-key read stamp (ADR 0057 §4) makes first-committer-wins
refuse a write computed from a stale value at prewrite. The cost of the missing check is a `40001`
nobody needed, so **counting refusals is the only assertion that can see it**; the first version
filtered on `is_ok()` and threw exactly that evidence away.

[ADR 0067]: ../adr/0067-the-check-mutation-and-the-latest-commit-question.md

### The third instance of one shape, and the one that was live

`Txn` has seven defaulted methods, which makes every wrapper of it a silent opt-out — `8126d450`'s
subject says "two more methods the wrapper swallowed" and it was not the last. Two more were found
while checking these rows:

* **`Recording::locks` was missing, and that was a wrong answer.** Every `pg_locks` read taken while
  a savepoint is open returned `LockView::default()` — empty — and Rails opens a savepoint for every
  nested `transaction do`. The view exists to answer what a stuck session holds; that is the state
  it is most likely to be stuck in. Fixed in `42344f78`, red-first, asking the same question either
  side of one `SAVEPOINT`.
* **`GatedTxn` in `tests/redrive.rs` forwarded fifteen methods and none of the seven with defaults.**
  So those tests ran with row locking off (`lock`'s default answers `Taken` to everybody), with no
  per-statement snapshot, and recording nothing for SERIALIZABLE. Latent, not live: `exec::redrive`
  calls none of the seven and the racing test races on the storage's write-write conflict, as its
  module doc says. Forwarded anyway in the same commit — the test that should catch whoever adds
  the first `lock()` to the re-driver was the one that had quietly stopped locking.

| Sighting | What the tree says |
|---|---|
| **promotion under load** | **Closed.** `promotion.rs` reports *"20 of 20 runs green now"*, after six defects each found by reading a trace and pinned by a unit test — "none was found by counting runs". The `#[ignore]` in that file is in prose describing how it was kept failing during the investigation, not an attribute on the test. |
| **SERIALIZABLE range validation deferred by h1** | **Closed.** Ranges are recorded and validated: `read_ranges` and `record_range` in `crates/esker-sql/src/backend/mod.rs:569,628`, and `serializable.rs` has a phantom test (`a_phantom_in_a_range_two_transactions_read_is_a_conflict`) plus one asserting a savepoint does not lose the check. ADR 0062 §"Phantoms" marks it caught. |
| **`esker-s3`'s duplicate TLS client** | **Not found as a duplicate.** `tls.rs` (597 lines) is the second implementor of the transport trait ADR 0025 designed for, with its own keep-alive pool — `client.rs` (923 lines) is the S3 protocol above it, not a second copy of it. If the sighting meant the *pool* logic specifically, name the two functions and it can be re-checked; nothing in the tree today reads as a duplicated client. |

### A view named inside an expression subquery — **closed the round after it was opened**

Opened as #9 while `view_test.rb` was being closed, and fixed in the next unit rather than carried:
`SELECT id FROM t WHERE id IN (SELECT id FROM v)` was `42P01` on **both** protocols, where
PostgreSQL 19 returns the row. `expand_views` walked `FROM` and the joins and `each_relation_name`
with it, so neither saw a name that appears only inside a `WHERE` — the cheap check reported no view
and the expansion was therefore never run. Both now recurse through expression subqueries, reusing
`exec::subquery`'s existing walks rather than adding a third. `tests/describe_over_a_view.rs`.

### `ORDER BY <name>` preferring an output column — **closed the unit after it was opened**

`order_keys` implemented the narrow half of PostgreSQL's rule (two output columns of one name are
`42702`) and its own comment said the *preference* would be invented because nothing had measured
it. An `ALTER TYPE` capture measured it by accident: the corpus wrote `SELECT m::text FROM t ORDER
BY m`, and the two servers disagreed about the **ordering** rather than about the enum, because
`m::text` is *named* `m` and PostgreSQL resolves `ORDER BY` against the select list first. One
keystroke — an alias — changes the answer, which is what makes an enum the sharpest way to see it.

Only a projection whose derived name is not its own column's is substituted, which is the whole of
what differs and leaves every aggregated query on the path it was already taking.
`tests/order_by_output_column.rs`.

### #3, #5, #6 and #8 — **closed, and two of them were closed before the register named them**

Verified by **ancestry**, not by commit date: `git merge-base --is-ancestor <fix> c6641c25`, where
`c6641c25` is the commit that added this file.

| # | Closed by | Already in the tree this register was written against? |
|---|---|---|
| 3 | `616954e8` *an ingest is refused for a shared key, not for a shared range* (ADR 0068), with `40a42384` making the disjoint case one seek | **Yes** |
| 5 | `46166886` *the crash loop kills after acknowledged writes, not after milliseconds*, on top of `d8a7b252` | **Yes** |
| 6 | `58ed28af` *a port that answers is not a driver that answers* and `2a82232e` *a cluster is announced when its stores answer, not when none has died yet* | No — closed after |
| 8 | `189d4117` *the Miri gate stops needing a flag to run at all* | No — closed after |

**#3 and #5 were already fixed in the very tree this file was written against.** #3's row went
further and said c6 had "verified this as the one item of eight that HEAD still owes"; the fix was
nine hours old and reachable from the same commit. So §2 above, which records three sightings that
were closed on inspection, was itself two rows short — a debt register verified against a tree is
still only as good as the question asked of it, and "is the site still there" is not the same
question as "is the symptom still there".

**#6 was two defects, not one flake.** A readiness check that connected to a port rather than
proving *which* process answered, and a cluster announced before its stores could answer. Both are
the shape a timeout hides: the test was recorded as a standing flake with an owner.

**#8's fix is three lines**, which is what the row estimated, and its value is not the three lines:
`config.failure_persistence = None` under `cfg(miri)` means the documented Miri command runs the
module instead of aborting 22 tests in. `docs/acceptance/v1.md` §0 now dates the flag rather than
requiring it.

### What each row said, beside what was true

The verdicts above say what closed each debt; they do not say what the register believed. Both are
kept, because the interesting part of a stale row is usually **which question it asked** — and a
reader who sees only the correction cannot tell a row that was wrong from a row that was answering
a different question.

**#3, as it stood:**

> **`Db::ingest` refuses any overlap, tombstones included.** `DbInner::place` refuses three ways:
> against another file in the same ingest, the memtable, and any level of the current version. c6
> verified this as the one item of eight that HEAD still owes.

`DbInner::place` returns a **level**, and its own doc comment says so: *"Placement, not permission:
`first_conflicting_key` has already decided whether the file may be adopted at all."* The row read
three range comparisons as three refusals. The refusal is `first_conflicting_key`, and since
`616954e8` it refuses on a shared **key** — which is the whole point, because `esker-txn` puts the
MVCC version *in* the key, so two files can interleave across a range and share nothing.

**#5, as it stood:**

> **`crash_through_the_client` starves under load.** Fails 6 runs in 10 under 24 spinning threads
> **in one container** … The child is killed on a **wall clock** while the writes it should
> interrupt are CPU-bound.

Every sentence was true of the tree c6 measured and none of it was true of the tree the register
was written against: `46166886` had already replaced the wall clock with a count of acknowledged
writes, three hours earlier. The row is a correct diagnosis carried past its own fix.

**#6, as it stood:**

> Passed in an exclusive run after failing on a 60 s timeout in both contended ones; c6 carries it
> as a **standing flake** with an owner and treats the exclusive pass as evidence it is the same
> contention rather than a defect of its own.

The exclusive pass was evidence of the opposite. Four loads made it a cliff — 0.38 / 0.31 / 0.32 s
and then 60.05 s FAILED — and a cliff is a state change, not contention. Behind it were two product
defects, and the second was only found by measuring whether the first had closed the hole.

**#8, as it stood:**

> Setting `failure_persistence: None` under `cfg(miri)` … would make the plain documented command
> true — and matters because the failure looks like the gate *failing* rather than the gate *not
> running*. Not urgent: `docs/bench/skiplist.md` §3 and `docs/acceptance/v1.md` §0 now both state
> the flag.

The row was right about the fix, right about why it mattered, and wrong only in the last sentence:
documenting the flag is what made it look not-urgent, and a gate that reads as failing is worth
three lines the day it is noticed. Both documents now carry the bare command with the flag dated
rather than required.

### #7 `join_cost` — **closed by c7's diagnosis and a change to the test's shape**

Recorded as "unexplained, not diagnosed": one failure in a full parallel run, 3/3 in isolation.
`docs/plans/debt-c7.md` §7 made it deterministic instead of counting runs — green at load 0, **red
at 14**, green again at 40 and 80. Non-monotonic, so a race in the *measurement*, and the cost
model was never in question.

The test took its four cells strictly in sequence — subject small, subject large, then control
small, control large — so **the control was measured after the subject rather than beside it**, and
load arriving or departing between the arms moved `growth` and `control` independently. Cancelling
load common to both arms is the one thing a control is for.

**The first reshape fixed that and still could not see the bug**, which is what the red-first run
found before it landed. Disabling the inner-side grouping so a materialised join is a cross product
again, the test **passed in 131 seconds**:

| | subject ÷ control at 1,000 | at 8,000 | growth ÷ growth |
|---|---:|---:|---:|
| grouped | 3.3 | 2.8 | 0.9 |
| cross product | 130 | 321 | **2.9** (bound 3.0) |

Two facts no amount of reading would have produced. **The cross product is already fully visible at
the small size**, so a growth term carries only the extra eight-fold rather than the fault. And
**`count(*)` is not linear across these sizes** — it grew 26x for 8x the rows — so dividing by its
growth removed most of what was left.

So what is asserted is the **level, not the growth**: at each size, the median over five rounds of
one *adjacent* pair — the join, then a scan of the same rows — must be under twenty scans. It needs
nothing to be linear, only that a scan and a join of the same rows meet the same machine. Measured
on both sides: **3.3 and 2.8 scans grouped, 130 and 321 as a cross product**, and the red-first run
now fails at **163.8 scans in 2.6 seconds** where it used to pass in 131. Both sizes are still
measured, smallest first, because a plan that is linear at one size and quadratic at the next is
what one size cannot see. `tests/join_cost.rs`.

## 3. ADR numbering

Checked mechanically over `docs/adr/*.md`:

| Check | Result |
|---|---|
| files matching `NNNN-*.md` | **63** |
| distinct numbers | **63** |
| range | **0001 – 0063** |
| gaps in 1..63 | **none** |
| numbers used by two files | **none** |
| numbered above 0063 | **none** |

So the index in `docs/acceptance/v1.md` §6 is complete and one-to-one.

> Numbering has collided twice in this project's history (ADR numbers and, separately, the catalog
> record version). Both were caught at merge, not at write time — the standing rule is to claim the
> number out loud in the report the moment it is taken.

## 4. How to re-take this

```sh
# ADR contiguity and duplicates
ls docs/adr/[0-9]*.md | sed 's|.*/||' | cut -c1-4 | sort | uniq -d      # duplicates
seq -f '%04g' 1 63 | while read n; do ls docs/adr/$n-*.md >/dev/null 2>&1 || echo "gap $n"; done

# who overrides changed_since_statement
grep -rn "fn changed_since_statement" crates/esker-sql/src/
```
