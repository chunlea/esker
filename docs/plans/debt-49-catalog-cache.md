# #49, option (b): give the catalog cache the readers it does not have

Status: **built, 2026-09-10** (lane b4; chosen by the user 16:00, ADR 0106 accepted with it).
*"What it turned out to be"* at the end of this file is what changed against the plan and what was
measured; everything before it is the plan as written, kept because the difference is the record.

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

## A DDL statement's own reads — h1's question, answered by call site

Taken 2026-09-10 with `ESKER_STMT_STATS_CALLERS=1`, which adds the **call site** beside each
key. `esker-coord/h1-ddl-cost.md` measured that a DDL's own catalog reads are **40–66% of its
round trips** and asked whether those readers are the ones this plan routes. A count cannot
answer that; a key and a caller can.

```text
=== CREATE TABLE
    17 reads: 17 point, 0 range
        1 x  get  layout   <- esker_sql::catalog::refuse_an_older_layout
        2 x  get  layout   <- esker_sql::catalog::stamp_layout
        1 x  get  name(t1,"d2")   <- esker_sql::catalog::View>::relation
        1 x  get  name(t1,"d2")   <- esker_sql::catalog::create_table
        1 x  get  name(t1,"d2_id_seq")   <- esker_sql::catalog::create_sequence
        1 x  get  name(t1,"d2_id_seq")   <- esker_sql::catalog::name_exists
        1 x  get  name(t1,"d2_pkey")   <- esker_sql::catalog::create_table
        1 x  get  name(t1,"d2_pkey")   <- esker_sql::catalog::name_exists
        2 x  get  next-id(t1)   <- esker_sql::catalog::allocate_id
        2 x  get  schema(t1,"esker")   <- esker_sql::catalog::schema_exists
        1 x  get  version(t1)   <- esker_sql::catalog::Catalog>::view_at::{closure#0}
        2 x  get  version(t1)   <- esker_sql::catalog::bump_version
        1 x  get  version(t18446744073709551615)   <- esker_sql::catalog::Catalog>::view_at::{closure#0}
=== ALTER TABLE ... DISABLE TRIGGER ALL
    17 reads: 16 point, 1 range
        3 x  get  layout   <- esker_sql::catalog::refuse_an_older_layout
        1 x  get  layout   <- esker_sql::catalog::stamp_layout
        2 x  get  name(t1,"d0")   <- esker_sql::catalog::View>::relation
        2 x  get  schema(t1,"esker")   <- esker_sql::catalog::schema_exists
        1 x  get  table(t1,#16384)   <- esker_sql::catalog::View>::table_by_id
        3 x  get  version(t1)   <- esker_sql::catalog::Catalog>::view_at::{closure#0}
        1 x  get  version(t1)   <- esker_sql::catalog::bump_version
        3 x  get  version(t18446744073709551615)   <- esker_sql::catalog::Catalog>::view_at::{closure#0}
        1 x  scan sequence(t1,#16384)..sequence(t1,#16384,…)   <- esker_sql::catalog::table_sequences
=== DROP TABLE
    29 reads: 19 point, 10 range
        3 x  get  layout   <- esker_sql::catalog::refuse_an_older_layout
        1 x  get  layout   <- esker_sql::catalog::stamp_layout
        2 x  get  name(t1,"d1")   <- esker_sql::catalog::View>::relation
        2 x  get  schema(t1,"esker")   <- esker_sql::catalog::schema_exists
        1 x  get  table(t1,#16384)   <- esker_sql::catalog::pg_relations::load_table
        1 x  get  table(t1,#16386)   <- esker_sql::catalog::View>::table_by_id
        1 x  get  table(t1,#16386)   <- esker_sql::catalog::pg_relations::load_table
        1 x  get  table(t1,#16389)   <- esker_sql::catalog::pg_relations::load_table
        3 x  get  version(t1)   <- esker_sql::catalog::Catalog>::view_at::{closure#0}
        1 x  get  version(t1)   <- esker_sql::catalog::bump_version
        3 x  get  version(t18446744073709551615)   <- esker_sql::catalog::Catalog>::view_at::{closure#0}
        1 x  scan fk-backref(t1,#16386)..fk-backref(t1,#16386,…)   <- esker_sql::exec::ddl::drop_table
        1 x  scan name(t1)..name(t1,…)   <- esker_sql::catalog::pg_relations::Relations>::read
        1 x  scan row(t1,#16386)..sql-index   <- esker_sql::exec::for_each_page::<esker_sql::exec::ddl::drop_one_table::{closure#0}>
        1 x  scan sequence(t1,#16384)..sequence(t1,#16384,…)   <- esker_sql::catalog::table_sequences
        2 x  scan sequence(t1,#16386)..sequence(t1,#16386,…)   <- esker_sql::catalog::table_sequences
        1 x  scan sequence(t1,#16389)..sequence(t1,#16389,…)   <- esker_sql::catalog::table_sequences
        1 x  scan type(t1)..type(t1,…)   <- esker_sql::catalog::user_types
        2 x  scan view(t1)..view(t1,…)   <- esker_sql::catalog::views
```

### The verdict, reader by reader

| reader | in this plan's list? | cacheable? |
|---|---|---|
| `catalog::schema_exists` | **covered** | yes |
| `pg_relations::Relations::read` / `load_table` | **covered** | yes |
| `catalog::views`, `user_types`, `table_sequences` | **covered** | yes |
| `View::relation`, `View::table_by_id` | already cached today | yes |
| `catalog::refuse_an_older_layout`, `stamp_layout` | **NOT covered — add** | yes, it is a store-wide **constant** |
| `catalog::name_exists`, `create_table`, `create_sequence` | **NOT covered — add** | yes, the same `names` map `View::relation` fills |
| `exec::ddl::drop_table` (`fk-backref` prefix scan) | **NOT covered — add** | yes, per table |
| `Catalog::view_at` (`version` x2 per view) | out of scope | **no — it is the validator**, and it is #50's key |
| `catalog::bump_version` | out of scope | **no** — a DDL's own read-modify-write |
| `catalog::allocate_id` (`next-id`) | out of scope | **no** — a counter that must be fresh |
| `exec::for_each_page` (row scan in `DROP`) | out of scope | no — that is data |

### How much of each statement this plan reaches

| statement | reads | already in the plan | cacheable, **to add** | irreducible |
|---|---|---|---|---|
| `CREATE TABLE` | 17 | 2 (12%) | **8** (3 layout, 5 name) | 6 (4 version, 2 next-id) |
| `ALTER … DISABLE TRIGGER ALL` | 17 | 3 (18%) | **4** (layout) | 7 (version) |
| `DROP TABLE` | 29 | 13 (45%) | **5** (4 layout, 1 fk-backref) | 7 (version) + 1 row scan |

`DROP` is the one this plan already reaches, which matches h1's reading that its cost is catalog
enumeration. `CREATE` is the one it barely touches, and what it misses there is not exotic: **five
name-existence point reads and three reads of a store-wide constant.**

### The bound nobody had stated: a DDL turns the cache off on purpose

`Catalog::view_at` takes `cached: bool`, and a transaction that has **written** the catalog gets
`catalog: None` so that it reads its own uncommitted DDL. That is correct and must stay. It means
**option (b) helps a DDL statement only up to its first catalog write** — the resolution and
enumeration phases, which for `DROP` is most of the statement and for `CREATE TABLE` is the front
half. Any estimate that ignores this is too generous, and the numbers above are counted from the
real trace rather than from the rule, so they already include it.

### And a bigger DDL item that belongs to #50, not here

**Every DDL statement opens three catalog views**, not two: `version(t1)` and
`version(t<cluster>)` are read three times each in `ALTER` and `DROP`, plus `bump_version`'s own —
**7 of 17 reads in an `ALTER`, 41% of the statement.** For an ordinary `SELECT` it is 2 of 9. That
is the key [ADR 0105](../adr/0105-a-catalog-read-never-waits.md) and #50 are about, and going from
three views to one would save four reads per DDL — **more than this plan saves on `CREATE TABLE`.**
Recorded here because the measurement found it; it is not this plan's to take.

### What it does to the milestone's arithmetic

Run 117 puts DDL at about **19%** of `transactions_test` (`DROP TABLE` 5.4%, the trigger pair
~11.5% inside `other`, `CREATE TABLE` 1.5%, `CREATE INDEX` 0.6%). Weighting each statement's
covered fraction by its share:

    this plan as written        0.45 x 5.4 + 0.18 x 11.5 + 0.12 x 1.5   = ~4.7% of the file
    with the three readers added  + 0.17 x 5.4 + 0.24 x 11.5 + 0.47 x 1.5 = ~9% of the file

**And for DDL that arithmetic is more trustworthy than it is for introspection.** A DDL's reads are
almost all **point** reads — 17 of 17 in `CREATE TABLE`, 16 of 17 in `ALTER` — so read count tracks
round trips tracks time. Introspection's are tenant-wide scans, where one read is O(catalog) and
the count says nothing about the clock.

So: **#49 (b) is worth about 9% of the file on DDL, on top of introspection's 72.4%** — and the
three readers that take it from 4.7% to 9% are five name lookups, a constant, and one prefix scan.
The milestone's account should carry both numbers, because only the second one is a decision about
this plan's scope.

## Scope

**In:**

1. `Cache` gains four maps beside `names` and `tables`: schemas, views, user types, and one
   table's sequences.
2. `schema_exists` and the four free functions take a `&View` instead of a `&dyn Txn`, and answer
   from the cache.
3. `Relations::read` takes a `&View`, and its per-table load goes through `View::table_by_id` so
   that a table hydrated once in a statement is hydrated once.
4. `hydrate`'s own second reads (`table_sequences`, `column_user_types`) go through the same view.

**Added 2026-09-10 after the DDL census above**, because a DDL statement's own reads turned out to
be 40-66% of its round trips and mostly *not* in the list this plan started with:

5. **The layout marker is read once per process**, not once per catalog view.
   `refuse_an_older_layout` and `stamp_layout` read a store-wide **constant** one to three times a
   statement; every DDL measured pays four.
6. **`name_exists`, `create_table` and `create_sequence` resolve names through `View::relation`**
   rather than reading `name_key` themselves — the same `names` map the cache already fills. Five
   of `CREATE TABLE`'s seventeen reads.
7. **`exec::ddl::drop_table`'s `fk-backref` prefix scan** joins the per-table entries.

**Out, and each for a stated reason:**

* **No new invalidation mechanism.** The version counter and the schema lease
  ([ADR 0028](../adr/0028-the-schema-lease.md)) are what already invalidate; this plan adds
  nothing to them and must not.
* **No cross-statement cache of *rows*.** Only catalog records. A row is the data.
* **No change to how many catalog views a statement opens.** Two per ordinary statement and
  **three per DDL** (`version` x2 each, plus `bump_version`'s own — 7 of an `ALTER`'s 17 reads),
  which [ADR 0105](../adr/0105-a-catalog-read-never-waits.md) states independently from the other
  side. That key is **#50's**, not this plan's: option (b) keeps exactly one version read per
  statement because it is the validator, and #50's fix makes that read not wait. A boundary, not a
  deferral — **and for DDL it is worth more than this plan is**, which is recorded above rather
  than quietly taken.
* **No change to `allocate_id` or `bump_version`.** Both are read-modify-writes of a counter that
  must be fresh; caching either would be a wrong answer, not a saving.
* **No batching and no pipelining** — those are ADR 0106's options (a) and (c), and B removes the
  reads (a) would batch.
* **No `pg_catalog` row-builder rewrite.** The builders keep their shape; only the reader they call
  changes.

## The file list

| file | change |
|---|---|
| `crates/esker-sql/src/catalog/mod.rs` | `Cache` gains `schemas`, `views`, `types`, `sequences`; `View` gains the four accessors; `schema_exists`, `schemas`, `views`, `user_types`, `table_sequences` move behind them; `hydrate` takes a `&View`. **And, from the DDL census**: `refuse_an_older_layout`/`stamp_layout` memoise the layout constant; `name_exists`, `create_table`, `create_sequence` resolve through `View::relation` |
| `crates/esker-sql/src/catalog/pg_relations.rs` | `Relations::read(&View, tenant)`; `load_table` calls `View::table_by_id` |
| `crates/esker-sql/src/catalog/pg_catalog.rs` | 8 `Relations::read` call sites |
| `crates/esker-sql/src/catalog/information_schema.rs` | 4 |
| `crates/esker-sql/src/catalog/pg_attribute.rs` | 2 |
| `crates/esker-sql/src/catalog/pg_index.rs`, `pg_constraint.rs` | 1 each |
| `crates/esker-sql/src/exec/ddl.rs` | 4 `Relations::read`, every `schema_exists`, and `drop_table`'s `fk-backref` scan |
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

1. **Written, red, and committed: `crates/esker-sql/tests/catalog_read_slope.rs`.** Two tests,
   both `#[ignore]`d until this plan lands, both run and read before being committed:

   * **`one_statement_reads_no_key_twice`** — no key may be read more than twice in one statement,
     two being the catalog views a statement opens. **Red at 7 offenders in 60 reads**: sixteen
     `schema(t1,"esker")` and five each of the four tenant-wide reads. Deterministic, no clock.
   * **`a_repeated_statement_stops_tracking_the_catalog`** — the slope, on the **second** run at an
     unchanged version, which is the run that has to become flat. **Red at 6.2x for 5x the
     catalog** (20 relations 4.09 ms, 100 relations 25.33 ms; control 0.57 → 3.38 ms).

   **Two and not one**, because neither catches the other: a tenant-wide scan is one read whatever
   it walks, so the count barely moves with the catalog while the cost moves faster than linearly.
   *Counterfactual for the first*: revert the `Relations::read` routing and the offenders return.
   **Delete the `#[ignore]`s when this lands** — nothing else about them should need changing, and
   if something does, the change is not option (b).
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
2. **The `other` tail was opened, and half of it is this plan's after all.** One pass over
   `run-117-transactions-tap.txt`: the 184 s is `DISABLE`/`ENABLE TRIGGER ALL` (200 statements,
   seven `ALTER TABLE`s each at ~106 ms — ordinary DDL, and **no option in ADR 0106 touches it**),
   and beside it sits **`AR tables()` at 208 x 437 ms = 91 s**, which *is* the `Relations::read`
   path this plan routes. So the plan's target is four shapes, not one:

   ```text
   pk_and_sequence_for   1,129 s     AR tables()   91 s
   AR columns()             16 s     AR primary key 9 s      = 1,245 s of 1,596 s = 78%
   ```
3. **Every one of them is O(catalog), measured.** Mean ms by decile of arrival through the file:
   `pk_and_sequence_for` 1432→3377 (**2.36x**), `tables()` 248→635 (**2.56x**). The 2.36x within
   the file is the 2.3x r1 measured *between* files — one mechanism, and it is the tenant-wide
   scans this plan removes. **The acceptance test should therefore be a slope, not a p50**: run the
   target statement against a catalog of n and of 5n and assert the ratio stops tracking n.

**And the risk this plan carries because of run 117**: the report's own catalog-stats section reads
*"the catalog READ is 2.3% of this file"*, which taken alone would say none of this is worth doing.
It is the **version** read — the one ADR 0102 instrumented — and `pk_and_sequence_for` makes 60 KV
reads of which two are it. The other 58 are what this plan removes. Anyone picking this up will meet
that 2.3% and should meet the explanation with it.

---

## What it turned out to be

Built the same day it was chosen. **The shape held** — routing, not construction — and three things
were not as written.

### 1. The bundle had to be cached, not only its parts

The Scope above gives `Cache` four new maps and has `Relations::read` take a `&View` so its
per-table load goes through `View::table_by_id`. That removes the point reads and the hydration
bundles; it leaves **the scan of the name records**, once per `Relations::read`, five times in a
statement that names five catalog relations. And the scan is the term that grows with the catalog.

So `Cache` holds `relations: BTreeMap<u64, Arc<Relations>>` as well — the whole bundle, at a
version. It is a pure function of the records at that version, which is the same argument that
makes `tables` safe. **The acceptance test is what forced it**: with the parts cached and the
bundle not, `one_statement_reads_no_key_twice` is red at five `scan name(t1)…`.

Six maps landed rather than four: `schemas`, `schema_lists`, `views`, `types`, `sequences`,
`relations`.

### 2. Two readers the census did not name, and both were cheap

* **The layout marker is memoised on the `Catalog`, not per process.** The plan says "once per
  process"; a `Catalog` is one per node and one node is one store, which is the assumption its
  version cache already rests on. Per process would have leaked one store's answer into another's,
  which is a real shape in this crate's own tests — many `MemoryBackend`s in one process.
* **A transaction that has written the catalog now builds its views reading nothing at all**
  (`Catalog::view_written`). Such a view never consults the cache, so the version it would read is
  a number nobody compares. Without this, routing `schema_exists` through a view would have *added*
  two reads per call inside a DDL statement — the opposite of the point.

### 3. The clock acceptance test measures a different mechanism, and it is still red

`a_repeated_statement_stops_tracking_the_catalog` was the plan's headline test. After option (b),
the second run of `pk_and_sequence_for` reads **4 keys at 20 relations and 4 at 100** — flat — and
takes **1.96 ms and 14.38 ms**. What it times is a **cross product between two computed catalog
views** (`pg_class × pg_depend` on `oid = objid`: 1.55 ms → 24.6 ms for five times the catalog,
15.9x), which is a planner defect that no option in ADR 0106 touches.

On the in-process node a KV read is a `BTreeMap` lookup; on the real topology it is **232 µs**.
So the clock here cannot see this row's change at all. The test is left in place, `#[ignore]`d with
that reason and those numbers, and
[`a_repeated_statement_reads_only_the_version_keys`](../../crates/esker-sql/tests/catalog_read_slope.rs)
is the acceptance test that does translate — red at 60 before, green at 4 now, asserted at two
catalog sizes. `esker-coord/QUESTION-b4.md` is where the rewrite is asked for rather than taken.

### And a wrong answer the tests found on the way

`CREATE TYPE`, `DROP TYPE` and a standalone `DROP SEQUENCE` never bumped the catalog version. With
those records read from the store every statement it could not be seen; with them cached, a type
session B declares is missing from session A's next statement. Fixed in the writers, where every
other catalog write already does it, and
`crates/esker-sql/tests/catalog_cache_invalidation.rs` is the test that was red on it — nine cases,
one per map, with the counterfactual recorded in its header (and the *first* counterfactual written
for it, which passed because it switched the cache off instead of making it stale).

### The numbers

| statement | before | after | after, cache warm |
|---|---|---|---|
| `pk_and_sequence_for` | **60** | 12 | **4** |
| `SELECT a FROM pk0 WHERE id = 1` | 9 | 5 | 5 |
| `INSERT … RETURNING id` | 8 | 5 | 5 |
| `CREATE TABLE` | 17 | 16 | 16 |
| `ALTER TABLE … DISABLE TRIGGER ALL` | 17 | **10** | 10 |
| `DROP TABLE` | 29 | **22** | 22 |

`CREATE TABLE` moves least, exactly as the DDL census predicted: what is left there is `next-id`,
the version counters and its own name writes. **r1's number on the real cluster is what closes this
row**; the counts are this lane's.
