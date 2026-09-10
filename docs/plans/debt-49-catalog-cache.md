# #49, option (b): give the catalog cache the readers it does not have

Status: **proposed, 2026-09-10 — waiting on the user.** Nothing here is built. Its target was
re-priced by run 117 the same day and the plan survived it; the last section is where that is
recorded.

[ADR 0106](../adr/0106-what-a-statement-reads-below-the-sql.md) is the decision document and this is
the plan for its option (b), written so that a lane can start on the day it is chosen.

## The finding this plan rests on

**The cache exists.** `catalog::Catalog` holds a `Cache { version, names, tables }` keyed on the
summed catalog version; `View::relation` and `View::table_by_id` answer from it; `usable_at` drops
it when the version moves; a transaction that has written the catalog gets `catalog: None` and
reads its own DDL. The version read is the one [ADR 0102](../adr/0102-the-catalogs-read-path.md)
timed at **0.4%** of a statement, and run 117 measured at **2.3%** of a whole file — small either
way, and **not** the reads this plan removes.

**What the census found is who goes around it** ([`docs/bench/statement-reads.md`](../bench/statement-reads.md)):

| what bypasses the cache | what it costs in the census |
|---|---|
| `catalog::schema_exists(txn, …)` — a free function on `&dyn Txn` | **16** identical `schema(t1,"esker")` point reads in one statement |
| `pg_relations::Relations::read(txn, tenant)` — loads the tenant's whole catalog itself | the **5×** repeated bundle; **27** call sites in the crate |
| `catalog::views`, `user_types`, `schemas`, `table_sequences` — free functions called from `hydrate` and from `pg_catalog`'s row builders | **15** tenant-wide range scans, twelve of them exact repeats |

So the work is **routing**, not construction. That is why this plan is a file list and not a
design.

## Scope

**In:**

1. `Cache` gains four maps beside `names` and `tables`: schemas, views, user types, and one
   table's sequences.
2. `schema_exists` and the four free functions take a `&View` instead of a `&dyn Txn`, and answer
   from the cache.
3. `Relations::read` takes a `&View`, and its per-table load goes through `View::table_by_id` so
   that a table hydrated once in a statement is hydrated once.
4. `hydrate`'s own second reads (`table_sequences`, `column_user_types`) go through the same view.

**Out, and each for a stated reason:**

* **No new invalidation mechanism.** The version counter and the schema lease
  ([ADR 0028](../adr/0028-the-schema-lease.md)) are what already invalidate; this plan adds
  nothing to them and must not.
* **No cross-statement cache of *rows*.** Only catalog records. A row is the data.
* **No change to how many catalog views a statement opens.** The census shows two per statement
  (two `version` reads, two `layout`); halving that is a separate, smaller row and mixing it in
  would make this plan's numbers unreadable.
* **No batching and no pipelining** — those are ADR 0106's options (a) and (c), and B removes the
  reads (a) would batch.
* **No `pg_catalog` row-builder rewrite.** The builders keep their shape; only the reader they call
  changes.

## The file list

| file | change |
|---|---|
| `crates/esker-sql/src/catalog/mod.rs` | `Cache` gains `schemas`, `views`, `types`, `sequences`; `View` gains the four accessors; `schema_exists`, `schemas`, `views`, `user_types`, `table_sequences` move behind them; `hydrate` takes a `&View` |
| `crates/esker-sql/src/catalog/pg_relations.rs` | `Relations::read(&View, tenant)`; `load_table` calls `View::table_by_id` |
| `crates/esker-sql/src/catalog/pg_catalog.rs` | 8 `Relations::read` call sites |
| `crates/esker-sql/src/catalog/information_schema.rs` | 4 |
| `crates/esker-sql/src/catalog/pg_attribute.rs` | 2 |
| `crates/esker-sql/src/catalog/pg_index.rs`, `pg_constraint.rs` | 1 each |
| `crates/esker-sql/src/exec/ddl.rs` | 4 `Relations::read`, and every `schema_exists` |
| `crates/esker-sql/src/exec/mod.rs` | 3 `Relations::read`; the per-statement view is already there |
| `crates/esker-sql/src/exec/typedef.rs` | 3 `Relations::read`, 1 `schema_exists` |
| `crates/esker-sql/src/exec/cursor.rs` | 1 |

**27 `Relations::read` call sites and 8-plus `schema_exists` ones**, counted 2026-09-10. A count
that has moved by the time this is picked up is the first thing to re-take: it is one `grep`, and
this plan's size is that number.

## Public API sketch

```rust
// catalog/mod.rs — the cache gains what the census says it is missing.
struct Cache {
    version: u64,
    names: BTreeMap<(u64, String), Option<Relation>>,
    tables: BTreeMap<(u64, u64), Arc<TableDef>>,
    /// `(tenant, name)` -> whether it exists. `None` is "known not to exist", which is what
    /// `schema_exists` asks sixteen times.
    schemas: BTreeMap<(u64, String), bool>,
    /// The tenant-wide lists, whole, because every reader of them wants all of them.
    views: BTreeMap<u64, Arc<Vec<ViewDef>>>,
    types: BTreeMap<u64, Arc<Vec<TypeDef>>>,
    /// `(tenant, table_id)` -> that table's sequences.
    sequences: BTreeMap<(u64, u64), Arc<Vec<SequenceDef>>>,
}

impl View<'_> {
    /// Whether a schema exists. `public` and the reserved ones answer without a read, as today.
    pub fn schema_exists(&self, name: &str) -> Result<bool>;
    /// Every view of this tenant, by stored name.
    pub fn views(&self) -> Result<Arc<Vec<ViewDef>>>;
    /// Every user-defined type of this tenant, in name order.
    pub fn user_types(&self) -> Result<Arc<Vec<TypeDef>>>;
    /// One table's sequences.
    pub fn table_sequences(&self, table_id: u64) -> Result<Arc<Vec<SequenceDef>>>;
}

// catalog/pg_relations.rs — the catalog views' loader stops being a second reader.
impl Relations {
    /// Was `read(txn: &dyn Txn, tenant: u64)`. Every table it needs comes from the view, so a
    /// table hydrated once in a statement is hydrated once.
    pub fn read(view: &View<'_>, tenant: u64) -> Result<Relations>;
}
```

**`Arc` on the three list-shaped entries, not `Clone`.** They are O(catalog) and the whole point is
that a statement stops paying that per relation; handing back a clone would move the cost from the
store to the allocator.

**The free functions stay, taking a `&dyn Txn`**, and become the cache's *fill* path — one caller
each, from `View`. Deleting them would put the key layout's readers in two places, which is the
thing `describe_key` was placed next to its builders to avoid.

## Test list

Every one of these is a test that can go red, and the counterfactual is named beside the ones where
a passing test would otherwise prove nothing.

1. **`a_statements_catalog_reads_do_not_grow_with_its_from_list`** — `pk_and_sequence_for` over a
   five-relation `FROM`, with `ESKER_STMT_STATS=1`: assert the point reads and range scans against
   a **one**-relation control, as a ratio. Today it is 5×; after, it must be 1×.
   *Counterfactual*: revert the `Relations::read` routing and the ratio returns to 5, or the test
   is measuring something else.
2. **`the_first_statement_after_a_ddl_sees_the_new_catalog`** — the counterfactual this plan is
   most at risk from, and it is the one `a-catalog-write-must-bump-the-version` records: session A
   caches, session B runs `CREATE TABLE` / `ALTER TABLE … ADD COLUMN` / `CREATE SCHEMA` /
   `CREATE TYPE` / `CREATE VIEW` / `ALTER SEQUENCE`, and A's **next** statement must see it — one
   case per new cache map, because a map that is not invalidated is invisible until someone asks
   it. *Counterfactual*: pin the version (skip the `usable_at` drop) and every one of the six must
   go red. A test suite where only one does is a suite that is testing one map.
3. **`a_ddl_transaction_reads_its_own_catalog_writes`** — `BEGIN; CREATE TABLE t …; SELECT … FROM t;`
   inside one transaction: the new maps must be bypassed exactly as `names` and `tables` are
   (`catalog: None`). *Counterfactual*: hand a DDL transaction the cache and this goes red.
4. **`a_view_older_than_the_cache_reads_through`** — `usable_at` already refuses a view whose
   version is *behind* the cache; one test per new map that the same rule holds, because a cache
   that answers a stale reader from a newer snapshot is the time-travel failure and no version
   check catches it.
5. **`schema_exists_is_one_read_a_statement`** — the sixteen, asserted as one.
6. **The existing suite is the rest of it.** `pk_and_sequence_cost.rs` (its timing assertions),
   `catalog_reads.rs`, `pg_catalog*.rs`, `information_schema*.rs`, `relation_resolution.rs`,
   `rolled_back_ddl_cache.rs`, and every parity corpus that reads a catalog view. The self-check
   list is computed from the diff, not from this paragraph.
7. **`statement_reads.rs` is re-run and `docs/bench/statement-reads.md` is replaced**, so the
   census that motivated the change is also the record of what it did. A plan whose "after" number
   is not measured the same way as its "before" has not been measured.

## Risks

* **A stale cache is a wrong answer, not a slow one**, and it is invisible until the *next*
  statement. This is the whole risk and test 2 is the whole mitigation. The window is one statement
  wide and is the width the node already trusts for `names` and `tables` — but it is now four more
  maps wide, and each is a separate way to be wrong.
* **A map that is filled and never invalidated looks exactly like a map that works**, until a DDL.
  Test 2 asks one case per map for that reason; a single combined case would pass on any one of
  them working.
* **`Relations::read`'s signature change touches 27 sites**, and a mechanical change of that size
  is where a wrong `tenant` or a wrong view gets threaded. The compiler catches the type, not the
  argument.
* **`Arc<Vec<…>>` changes who owns the lists**, and a caller that mutated its copy would now mutate
  the cache. Every current caller reads; the review has to confirm that, not assume it.
* **The measurement is on the in-process node.** The count is topology-independent, the *time* is
  not, and only a real-topology run says what the counts bought. r1's is the number that closes
  this, not mine.

## What would have made this plan the wrong one, and what run 117 said instead

**Written before the numbers:** *"if run 117's per-statement prices show the 907 ordinary statements
dominated by the TSO, the commit or the wire rather than by their reads, then eight catalog reads
out of nine is a true fact about a cost that does not matter, and this plan should not be
started."*

**Run 117 answered it 2026-09-10, and the answer was neither of the two this expected.** Transaction
control is 0.8% of the file at p50 0.1 ms, so nothing is TSO- or commit-bound; but the ordinary
statements are not where the time is either — **catalog introspection is 72.4%** and
`pk_and_sequence_for` alone is **71.8%**, 483 statements at p50 2,382 ms. So *"eight of nine reads
are catalog"* is indeed a true fact about a cheap statement, **and the plan is still the right one**
— for the statement it now turns out to be about.

**Two things in this plan change because of that, and neither is its shape:**

1. **The first test is the one that matters most, not the ratio test.** `pk_and_sequence_for` in
   `transactions_test` is the target: 483 x 2.4 s. Test 1's five-relation ratio is the mechanism;
   the acceptance number is r1's, and it is **1,466 s → 496 s** at 35 → 3 round trips.
2. **The `other` bucket is the second thing to look at and is nobody's row yet.** 592 statements,
   median 8.2 ms, **p95 1,078 ms** — 184 of its 189 seconds are in a tail nobody has opened. That
   is larger than `app SELECT` and `DDL DROP TABLE` put together and it is not this plan's; it
   wants one `sort` before it wants a design.

**And the risk this plan carries because of run 117**: the report's own catalog-stats section reads
*"the catalog READ is 2.3% of this file"*, which taken alone would say none of this is worth doing.
It is the **version** read — the one ADR 0102 instrumented — and `pk_and_sequence_for` makes 60 KV
reads of which two are it. The other 58 are what this plan removes. Anyone picking this up will meet
that 2.3% and should meet the explanation with it.
