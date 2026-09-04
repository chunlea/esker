# Debts still open at v1

**Status: draft. Every row was verified against the tree at this commit**, not transcribed from a
list — three items that were reported as open turned out to be closed, and three that were not on
the list are open (#5, #8 and #9). Each row names its site, a size, and who it belongs to.

Sources: the c6 wave's verification record (`debt-c6.md`), the coordinator's sightings, and the
code itself. `docs/acceptance/v1.md` carries the numbers; this file carries what is left.

---

## 1. Open

| # | Debt | Site | Size | Owner |
|---|---|---|---|---|
| 1 | **`changed_since_statement` is defaulted on the store path.** The trait's default answers `false` — correct for a backend that takes no locks — and only `MemoryTxn` overrides it. `StoreTxn` does not, so under a real cluster a `READ COMMITTED` re-run proceeds on a value that may be stale, and the check that removed ~100 spurious `40001`s in 1,200 transactions does not run there. | `crates/esker-sql/src/backend/store.rs` (no override); default at `backend/mod.rs:214`; already named in `exec/savepoint.rs:243` | medium — one method, but it needs the store to answer "written since ts" | h1 (txn/locking) |
| 2 | **`Recording` forwards `changed_since_statement` but the same gap reaches it.** With a savepoint open — which Rails opens for *every* nested `transaction do` — a SERIALIZABLE transaction recorded and validated nothing until this was wired, and the statement re-check does not run on the store path for the same reason as #1. | `crates/esker-sql/src/exec/savepoint.rs:243` | small once #1 lands | h1 |
| 3 | **`Db::ingest` refuses any overlap, tombstones included.** `DbInner::place` refuses three ways: against another file in the same ingest, the memtable, and any level of the current version. c6 verified this as the one item of eight that HEAD still owes. | `crates/esker-engine/src/db/ingest.rs:353` (`DbInner::place`; c6's record says `:126`, and the file has moved since) | medium | c6 / engine |
| 4 | **Cross-node deadlock detection.** The wait-for graph is node-local, which covers every deadlock two sessions of one `esker-sql` process can make. A cycle *across* nodes needs a graph both can see. Named in the code as a follow-on, and PD's job. | `crates/esker-sql/src/backend/locks.rs:46` | large — needs a PD-held graph | PD / pdha |
| 5 | **`crash_through_the_client` starves under load.** Fails 6 runs in 10 under 24 spinning threads **in one container**, so it is not the network-namespace contention the harness fix addressed. It is **not a durability failure**: the round's own guard `acked > 0` fires, and the durability assertion at `:311` fired in none of the six. The child is killed on a **wall clock** while the writes it should interrupt are CPU-bound. | `crates/esker-client/tests/crash_through_the_client.rs:331` (c6's record says `:305`; the file has moved) | small — measure the kill point in acknowledged writes, or retry a round that acked none | client |
| 6 | **`esker-cli::cluster_start a_driver_that_cannot_listen_is_a_failure_and_not_a_cluster`.** Passed in an exclusive run after failing on a 60 s timeout in both contended ones; c6 carries it as a standing flake with an owner and treats the exclusive pass as evidence it is the same contention rather than a defect of its own. | `crates/esker-cli/tests/cluster_start.rs` | small, and may be closed by the per-container network namespaces | cli |
| 7 | **`esker-sql::join_cost::a_materialised_join_costs_what_it_pairs_and_not_the_cross_product`.** One failure in a full 3,281-test parallel run; 3/3 in isolation and green on the next two full runs. A timing-**ratio** test with a control, so load-sensitive by construction. Unexplained, not diagnosed. | `crates/esker-sql/tests/join_cost.rs` | small to diagnose; unknown to fix | h1 (join cost) |
| 8 | **The Miri gate needs `-Zmiri-disable-isolation`, which the code could make unnecessary.** proptest's default `FileFailurePersistence` calls `std::env::current_dir` to place a `.proptest-regressions` file, and Miri refuses `getcwd` under isolation, so the run aborts with 22 tests unrun. Setting `failure_persistence: None` under `cfg(miri)` in the memtable's `ProptestConfig` would make the plain documented command true — and matters because the failure looks like the gate *failing* rather than the gate *not running*. Not urgent: `docs/bench/skiplist.md` §3 and `docs/acceptance/v1.md` §0 now both state the flag. | `crates/esker-engine/src/memtable/differential.rs:316` (`ProptestConfig::with_cases`) | ~3 lines | engine |
| 9 | **A view named inside an *expression* subquery is not expanded.** `SELECT id FROM t WHERE id IN (SELECT id FROM v)` is `42P01` where PostgreSQL 19 returns the row — measured on both protocols, so it is not a describe gap. `expand_views` walks `FROM` and the joins and `each_relation_name` with it, so neither sees a name that appears only in a `WHERE`. Found while closing `view_test.rb`, which never reaches it. | `crates/esker-sql/src/exec/mod.rs` (`expand_views`, `each_relation_name`) | medium — the walk has to reach expression subqueries, and `plan_subqueries` runs after it | esker-sql |

## 2. Reported as open, and closed on inspection

Recorded because the next reader will be handed the same list.

| Sighting | What the tree says |
|---|---|
| **promotion under load** | **Closed.** `promotion.rs` reports *"20 of 20 runs green now"*, after six defects each found by reading a trace and pinned by a unit test — "none was found by counting runs". The `#[ignore]` in that file is in prose describing how it was kept failing during the investigation, not an attribute on the test. |
| **SERIALIZABLE range validation deferred by h1** | **Closed.** Ranges are recorded and validated: `read_ranges` and `record_range` in `crates/esker-sql/src/backend/mod.rs:569,628`, and `serializable.rs` has a phantom test (`a_phantom_in_a_range_two_transactions_read_is_a_conflict`) plus one asserting a savepoint does not lose the check. ADR 0062 §"Phantoms" marks it caught. |
| **`esker-s3`'s duplicate TLS client** | **Not found as a duplicate.** `tls.rs` (597 lines) is the second implementor of the transport trait ADR 0025 designed for, with its own keep-alive pool — `client.rs` (923 lines) is the S3 protocol above it, not a second copy of it. If the sighting meant the *pool* logic specifically, name the two functions and it can be re-checked; nothing in the tree today reads as a duplicated client. |

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
