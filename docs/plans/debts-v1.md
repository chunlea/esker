# Debts still open at v1

**Status: three of the original eight are still open — #1, #2 and #4.** Everything else has been
closed and moved to §2 with the commit that closed it, and the numbers are quoted across lanes, so
a closed row leaves a **gap** rather than renumbering the rows after it. Each remaining row names
its site, a size, and who it belongs to.

> **The original claim on this line was that every row had been verified against the tree rather
> than transcribed. That was true of the sites and not of the symptoms.** #3 and #5 were already
> fixed in the very commit this file was added at — #3's row even said c6 had verified it as the
> one item of eight that HEAD still owed — which makes five rows that turned out to be closed when
> someone looked, not three. Asking "is the site still there" is not asking "is the symptom still
> there", and a register is only as good as the question put to it.

Sources: the c6 wave's verification record (`debt-c6.md`), the coordinator's sightings, and the
code itself. `docs/acceptance/v1.md` carries the numbers; this file carries what is left.

---

## 1. Open

| # | Debt | Site | Size | Owner |
|---|---|---|---|---|
| 1 | **`changed_since_statement` is defaulted on the store path.** The trait's default answers `false` — correct for a backend that takes no locks — and only `MemoryTxn` overrides it. `StoreTxn` does not, so under a real cluster a `READ COMMITTED` re-run proceeds on a value that may be stale, and the check that removed ~100 spurious `40001`s in 1,200 transactions does not run there. | `crates/esker-sql/src/backend/store.rs` (no override); default at `backend/mod.rs:214`; already named in `exec/savepoint.rs:243` | medium — one method, but it needs the store to answer "written since ts" | h1 (txn/locking) |
| 2 | **`Recording` forwards `changed_since_statement` but the same gap reaches it.** With a savepoint open — which Rails opens for *every* nested `transaction do` — a SERIALIZABLE transaction recorded and validated nothing until this was wired, and the statement re-check does not run on the store path for the same reason as #1. | `crates/esker-sql/src/exec/savepoint.rs:243` | small once #1 lands | h1 |
| 4 | **Cross-node deadlock detection.** The wait-for graph is node-local, which covers every deadlock two sessions of one `esker-sql` process can make. A cycle *across* nodes needs a graph both can see. Named in the code as a follow-on, and PD's job. | `crates/esker-sql/src/backend/locks.rs:46` | large — needs a PD-held graph | PD / pdha |

## 2. Reported as open, and closed on inspection

Recorded because the next reader will be handed the same list.

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
