# Phase 13 — the rest of `pg_catalog`, the schema-dump functions, and `information_schema`

`pg_type`, `pg_range`, `pg_class` and `pg_namespace` are computed views over the catalog records
(`c88a932`, `9de9519`). This phase finishes the surface: **`pg_attribute`, `pg_attrdef`,
`pg_index`, `pg_constraint`**, the functions that print a definition — **`format_type`,
`pg_get_expr`, `pg_get_indexdef`, `pg_get_constraintdef`** — and **`information_schema`** as five
views over the same records.

Same shape as the four that exist, and it is the whole design: **computed, read-only, derived from
the catalog records, never a second store.** Nothing here writes anything, nothing here is kept in
step with anything, and a `CREATE TABLE` that has not committed is invisible to all of it.

## 0. What decides "done"

`ActiveRecord`'s schema-dump path. Every statement `schema_dumper` / `indexes()` /
`foreign_keys()` / `columns()` / `primary_keys()` sends, over a fixture schema of this node's own
types, returns what PostgreSQL 19beta1 returns for the same schema — or is refused with `0A000`
naming the feature it needs, counted under ADR 0031 category (c). Nothing here may answer a
question with a value nobody measured.

The five statements, verbatim from `activerecord-8.1.3.1`
(`connection_adapters/postgresql/schema_statements.rb`, `postgresql_adapter.rb`):

| Method | Reads | Needs from this phase | Needs from elsewhere |
|---|---|---|---|
| `columns()` | `pg_attribute`, `pg_attrdef`, `pg_type`, `pg_collation` | `format_type`, `pg_get_expr`, `col_description`, `::regclass` | — |
| `primary_keys()` | `pg_index`, `pg_attribute` | `indisprimary`, `indkey` | `= ANY(int2vector)`, `array_position` — **the array lane** |
| `indexes()` | `pg_class`×2, `pg_index`, `pg_namespace` | `pg_get_indexdef/1`, `obj_description` | `ARRAY(SELECT …)`, `generate_subscripts` — **the array lane** |
| `foreign_keys()` | `pg_constraint`, `pg_class`×2, `pg_namespace` | `contype = 'f'` (no rows) | `array_agg`, `conkey[idx]`, `oid::regclass::text` |
| `check_constraints()`, `unique_constraints()` | `pg_constraint`, `pg_class`, `pg_namespace` | `pg_get_constraintdef` | `array_agg` for the column names |
| `pk_and_sequence_for()` | `pg_depend`, `pg_constraint`, `pg_attrdef` | the fallback query's half | `pg_depend`, `~*`, `split_part` |

**What this phase will NOT do**, each named so a reader finds the gap rather than a wrong answer:

* **`pg_depend`, `pg_collation`'s rows, `pg_am`, `pg_extension`, `pg_description`.** `pg_depend` is
  a graph this node does not keep; `pg_collation` is **empty** for the reason §3 gives.
* **Arrays anywhere.** `indkey` is text (§3.3 — measured, and it is what the client does with it);
  `conkey` and `confkey` are **refused by name** (`42703`), because AR's only use of them is
  `c.conkey[idx]`, a real array subscript. A text column named `conkey` would answer
  `conkey[1]` with a character rather than a column number, which is a wrong answer and not a gap.
* **Foreign keys.** No FK record exists in the catalog (`Relation` has four variants and none is
  one), so `pg_constraint` has no `contype = 'f'` row and `information_schema.referential_constraints`
  is empty. Both are correct answers about this node, not omissions. If the type lane lands FK DDL,
  the rows follow the records; this lane does not build FK storage.
* **Expression and partial indexes, `DESC`, `NULLS [NOT] DISTINCT`.** The DDL for them is `0A000`
  today (`parse::lower::index_columns`), so `indpred`, `indexprs` and `indnullsnotdistinct` are
  always NULL/false here — which is what a catalog with no such index says.
* **Comments.** `COMMENT ON` is not a statement this node has, so `col_description` and
  `obj_description` are NULL. Measured: that is exactly what a real server answers for a relation
  with no comment.
* **`DROP COLUMN`.** `attisdropped` is always false, because nothing can drop a column yet.

## 1. The OID strategy, which is the thing that must not be got wrong

Every join `ActiveRecord` writes is an oid join: `t.oid = d.indrelid`, `d.indexrelid = i.oid`,
`a.attrelid = d.adrelid`, `c.conrelid = t.oid`. An oid that differs between two views breaks all of
them silently, and an oid that **collides** breaks them in a way that looks like it works.

**The rule: a relation's oid is the id the catalog already gave it, and every view derives it from
the same function.** One function, `pg_relations::oid_of`, and no view computes one itself.

| Relation | oid | Where it comes from |
|---|---|---|
| table | `table_id` | the name record |
| index | `index_id` | the name record |
| sequence | `SequenceDef.id` | the sequence record, keyed by the column it fills |
| primary key | `PRIMARY_KEY_OID_BASE + table_id` | derived — it has no record of its own |

### 1.1 Two collisions this phase fixes in `pg_class`

`9de9519` gave the **primary key** and the **sequence** the *table's* id as their oid:

```rust
Relation::PrimaryKey { table_id } => (table_id, "i"),
Relation::Sequence { table_id, .. } => (table_id, "S"),
```

So `t`, `t_pkey` and `t_id_seq` are three rows of `pg_class` with one oid between them. Nothing read
it before this phase, which is why it survived; every statement in §0 reads it. Measured on
19beta1, `SELECT c.oid <> p.oid FROM pg_class c, pg_class p WHERE c.relname='cb' AND
p.relname='cb_pkey'` is `t`, and `pg_index` for `cb` has `indexrelid = 'cb_pkey'::regclass`
distinct from `indrelid = 'cb'::regclass`.

* **The sequence** has an id already — `SequenceDef.id`, from the same tenant relation-id sequence
  tables and indexes draw from. It was simply not read. `pg_class_rows` now reads the sequence
  records (one prefix scan per tenant, `'m' ++ "sql" ++ 'q' ++ tenant`) and reports it.
* **The primary key** has none, and cannot be given one without a catalog format change: the name
  record for it is `Relation::PrimaryKey { table_id }` and a new field would bump
  `CATALOG_FORMAT_VERSION`. It does not need one. Its oid is derived, in a reserved band that
  nothing a user creates can reach — relation ids come from a per-tenant sequence that starts at 1:

  ```rust
  /// `1 << 62`. A primary key's oid, which it has no record to carry.
  const PRIMARY_KEY_OID_BASE: u64 = 0x4000_0000_0000_0000;
  ```

  Stable for the life of the table, equal in every view, and reversible — `oid - BASE` is the table
  whose key it is, which is what `pg_index.indrelid` and `pg_constraint.conrelid` need.

**This changes an observable answer** (`SELECT oid FROM pg_class WHERE relname = 't_pkey'`) and no
golden test pins it; a test that the three oids of one table are distinct lands with the change.

### 1.2 The namespace oid stays 11

A real server's `public` is `2200` (measured). Ours is `11`, chosen by `9de9519` in the same
reserved band as the view ids. What has to be true is only that `pg_class.relnamespace =
pg_namespace.oid = pg_constraint.connamespace`, which is the join AR writes, and it is. Declared,
not changed — moving it would churn a corpus for a number no client compares to a constant.

## 2. Units

Each unit: capture PG19 first, then a corpus under `tests/corpus/pg19_catalog_*.txt`, then the
implementation, then a test file. Each commits on its own.

### U1 — `format_type`, then `pg_attribute` + `pg_attrdef`

`format_type` **first and on its own commit**: it is statement 193 of `schema.rb` and the whole of
`columns()` stops on it, so landing it early is what lets the type lane's later units be measured
through `schema.rb` at all.

**U1a — `format_type(oid, typmod)`.** Oracle: `esker-rails-harness/captures/pg19_format_type.txt`
(28 statements, captured by r1) replayed here as `tests/corpus/pg19_catalog_format_type.txt` with
the rows for types this node does not have declared as divergences. The facts that decide it:

* It almost never fails. An unknown oid is `???`, oid `0` is `-`, a NULL oid is NULL. **Not an
  error** — so a node that raised would break `columns()` on the first unusual column.
* A typmod on a type that takes none is **ignored**: `format_type(23, 4)` is `integer`.
* Each family's "no typmod" threshold differs: `varchar` is length+4 so `0`, `1` and `4` all print
  bare and `5` is `character varying(1)`; `timestamp` is the precision directly so **`0` is real**
  and prints `timestamp(0) without time zone`; only a negative is bare.
* It does not clamp: `format_type(1114, 7)` is `timestamp(7) without time zone`, a precision no
  `CREATE TABLE` will store.
* The arity is exact: `format_type(23)` is `42883 … does not exist` naming the *number* of
  arguments.

This is a pure function of `(oid, typmod)` and needs no catalog, so it is a new
`plan::Expr::CatalogFunc` arm evaluated in `exec::cursor::evaluate_in` like any other row function.
It answers for **every oid in `pg_type`**, which is this node's types, plus the `???`/`-`/NULL rules
above for everything else.

**U1b — `pg_attribute` and `pg_attrdef`.** Measured (fixture `ca`/`cb`/`cc`, §5):

| Column | Value here | Measured fact |
|---|---|---|
| `attrelid` | the relation's oid (§1) | rows exist for **indexes too**: `cb_x_idx` has one attribute, `x`, attnum 1 |
| `attname` | the column name | |
| `atttypid` | `ColumnType::oid()` | |
| `attnum` | 1-based position | **`attnum <= 0` rows exist on a real server**: `ctid` -1, `xmin` -2, `cmin` -3, `xmax` -4, `cmax` -5, `tableoid` -6. `count(*)` for a 15-column table is 21. This node has none of those six columns, so it has none of those rows — a declared divergence, and one AR cannot see because every statement it writes says `attnum > 0` |
| `attnotnull` | `ColumnDef::not_null` | `t` for a `bigserial` primary key and for a `NOT NULL` column |
| `atthasdef` | `default.is_some()` **or** a sequence fills it | `t` for `bigserial`; **`f` for an identity column** — an identity has no `pg_attrdef` row |
| `atttypmod` | `ColumnDef::typmod`, verbatim | `varchar(5)` → 9, `character(3)` → 7, `timestamp(3)` → 3, everything else -1 (ADR 0033 stores exactly this) |
| `attisdropped` | `false` | nothing can drop a column |
| `attidentity` | `''`, `'d'`, `'a'` | **empty string, not NULL**, for an ordinary column; `d` for `GENERATED BY DEFAULT`, `a` for `GENERATED ALWAYS`. `SequenceDef.identity` already holds which |
| `attgenerated` | `''` | no generated columns |
| `attcollation` | `0` | §3 |

`pg_attrdef`: one row per column with a **default**, not per column. `adrelid`, `adnum`, `adbin`.

* `pg_get_expr(adbin, adrelid)` prints `nextval('ca_id_seq'::regclass)` for a `bigserial`,
  `'hi'::text` for a text literal, and **bare `7`** for an integer literal — the literal is printed
  with its cast where the type is not the literal's own.
* A **`GENERATED … AS IDENTITY` column has no `pg_attrdef` row at all** and `atthasdef` is `f`.
* `adbin` is `pg_node_tree` on a real server and holds the parse tree. Here it holds **the printed
  expression as text**, and `pg_get_expr` is the identity on it. Declared divergence: `SELECT adbin`
  differs, `SELECT pg_get_expr(adbin, adrelid)` does not — and AR only ever writes the second.

### U2 — `pg_index` + `pg_get_indexdef`

Rows: one per index of every table, **plus one for the primary key**, whose `indexrelid` is §1.1's
derived oid. Measured: `cb_pkey` is `indisprimary t`, `indisunique t`, `indkey 1`; a unique index is
`f`/`t`; a plain two-column index is `f`/`f` with `indkey` `3 4`.

* `indkey` is `int2vector` on a real server and **prints space-separated**. AR reads it as
  `row[2].split(" ").map(&:to_i)` — a text operation on the rendered value — so `text` here has the
  same characters and the same meaning. Declared as a type divergence, like `typinput`'s `regproc`.
* `indnatts` is the count; `indpred`, `indexprs` are NULL; `indisvalid` is `t` for a
  `SchemaState::Public` index and **`f` for anything earlier** — which is the honest reading of
  ADR 0020's states and is what AR's `indisvalid` column is for.
* `pg_get_indexdef(oid)` → `CREATE [UNIQUE ]INDEX <name> ON public.<table> USING btree (<cols>)`.
  Measured, and the primary key's is `CREATE UNIQUE INDEX cb_pkey ON public.cb USING btree (id)`
  even though the row key *is* the primary key here.
* `pg_get_indexdef(oid, colno, pretty)` → the **column name alone** for `colno >= 1`, the whole
  definition **without the `public.` qualifier** for `colno = 0`, and the **empty string** past the
  last column. All three measured.

### U3 — `pg_constraint` + `pg_get_constraintdef`

| contype | Row here | From |
|---|---|---|
| `p` | one per table with a declared primary key | `TableDef::primary_key_name`, `primary_key` |
| `n` | **one per `NOT NULL` column**, named `<table>_<column>_not_null` | `ColumnDef::not_null` |
| `u` | one per unique index that a `UNIQUE` constraint made | `IndexDef::unique` |
| `c`, `f`, `x` | none | no `CHECK`, no FK, no `EXCLUDE` in this node |

**PostgreSQL 19 has a `pg_constraint` row for `NOT NULL`**, contype `n`, `pg_get_constraintdef` =
`NOT NULL id`. That is new in this major and is the row an emulation written from an older memory
would be missing; it is measured and it is here. A table with no primary key and no `NOT NULL`
column has **no rows at all** (measured: `cc`).

`conindid` is the index's oid — for a primary key, §1.1's derived one, and
`conindid = 'cb_pkey'::regclass` is `t` on a real server. `conkey`/`confkey` are refused (§0).
`pg_get_constraintdef(oid)` prints `PRIMARY KEY (id)`, `NOT NULL id`, `UNIQUE (x)`; the two-argument
`(oid, pretty)` form answers the same here.

### U4 — `information_schema`

Five views, and their **names carry the schema**: `information_schema.tables`. A bare `tables` is
not a relation on a real server and must stay `42P01` here. `pg_catalog.pg_attribute` resolves to
`pg_attribute` for the same reason — `parse::lower::object_name` refuses every qualified name
today, and this unit teaches it these two schemas and no others (`public.t` stays refused, named).

* **`tables`**: `table_catalog` (the database name), `table_schema` `public`, `table_name`,
  `table_type` `BASE TABLE`. **Only relkind `r`** — an index and a sequence are not in it
  (measured: `count(*) FROM information_schema.tables WHERE table_name = 'cb_x_idx'` is 0).
* **`columns`**: `column_name`, `ordinal_position`, `is_nullable` (`YES`/`NO`, not a boolean),
  `data_type`, `character_maximum_length`, `numeric_precision`, `numeric_scale`,
  `datetime_precision`, `column_default`, `udt_name`, `is_identity`, `identity_generation`,
  `is_generated`. Measured spellings, all fourteen of this node's types:

  | `data_type` | `udt_name` | precision | scale | max length | datetime |
  |---|---|---|---|---|---|
  | `bigint` | `int8` | 64 | 0 | | |
  | `integer` | `int4` | 32 | 0 | | |
  | `smallint` | `int2` | 16 | 0 | | |
  | `text` | `text` | | | | |
  | `character varying` | `varchar` | | | n, or NULL when unqualified | |
  | `character` | `bpchar` | | | n | |
  | `json` / `jsonb` | `json` / `jsonb` | | | | |
  | `boolean` | `bool` | | | | |
  | `bytea` | `bytea` | | | | |
  | `timestamp with time zone` | `timestamptz` | | | | 6, or the declared p |
  | `timestamp without time zone` | `timestamp` | | | | 6, or the declared p |
  | `double precision` | `float8` | 53 | NULL | | |
  | `real` | `float4` | 24 | NULL | | |

  An **integer's `numeric_scale` is 0 and a float's is NULL** — the one asymmetry a reader would
  get wrong. `column_default` is the same text `pg_get_expr` prints. An identity column is
  `is_identity YES`, `identity_generation BY DEFAULT`/`ALWAYS`, `column_default NULL`,
  `is_generated NEVER`.
* **`table_constraints`**: `constraint_name`, `constraint_type`, `table_name`, `is_deferrable`
  (`NO`), `initially_deferred` (`NO`). **A `NOT NULL` constraint appears here as `CHECK`**, measured
  — `cb_id_not_null | CHECK`. A primary key is `PRIMARY KEY`.
* **`key_column_usage`**: the primary key's and unique constraints' columns, with
  `ordinal_position` 1-based and `position_in_unique_constraint` NULL.
* **`referential_constraints`**: empty, and that is a valid answer (§0).

Every one of these declares its column types as `text` where a real server declares a domain
(`information_schema.sql_identifier`, `cardinal_number`, `yes_or_no`, `character_data`). Measured
via `pg_typeof`; the values are identical and only `RowDescription`'s OID differs, which is the
trade `pg_type` already makes for `name` and `regproc`.

### U5 — the `ActiveRecord` schema-dump path

The acceptance test. A fixture schema of this node's types, and every statement from §0's table put
to both. What is reached and agrees is pinned; what is refused is refused with `0A000` naming its
feature and counted (c). `::regclass` lands here if U1 has not needed it sooner: it is a name to an
oid, resolved by the executor before the plan is built — the same place a sequence call is resolved
— because that is where the catalog is in reach. `'nosuchrel'::regclass` is `42P01`.

### U6 — ADR and `DESIGN.md`

ADR (number claimed at main's HEAD when this merges): **the catalog surface is computed, and an
oid is the id the record already carries**. The two collisions, the derived primary-key oid, the
`indkey`-as-text / `conkey`-refused rule, and the list of what is refused by name.

## 3. Decisions taken once, here, so no unit re-takes them

1. **`pg_collation` is empty and every `attcollation` and `typcollation` is 0.** AR's
   `columns()` writes `LEFT JOIN pg_collation c ON a.attcollation = c.oid AND a.attcollation <>
   t.typcollation`. Measured on 19beta1: `attcollation` is 0 for `int8` and 100 for `text`,
   `typcollation` matches, so `a.attcollation <> t.typcollation` is **false for every column** and
   `collname` comes back NULL for all five. With both columns 0 here the join gives the same NULL,
   for the same reason, and `pg_collation` is empty because this node has no collation feature at
   all — the argument `pg_range` already makes. Declared: `count(*) FROM pg_collation` is 880 there
   and 0 here.
2. **A column is added to `pg_type`** (`typcollation`), which is the first time one of the four
   existing views grows. `SELECT *` over it expands in PostgreSQL's column order (`7be39ca`), so
   the new column goes **last** and the corpus for it is extended rather than rewritten.
3. **Rows come from one snapshot, taken once per statement.** A new `catalog::pg_relations`
   holds every relation of the tenant with its oid, kind, name and `TableDef`, built from one scan
   of the table records plus one of the sequence records. Every view in this phase reads it and no
   view scans the catalog itself. That is what makes an oid equal across views by construction
   rather than by five functions agreeing.
4. **The snapshot is bounded.** A catalog scan is a scan like any other: past
   `MAX_CATALOG_RELATIONS` it answers `53400` rather than allocating on the client's behalf, the
   same rule `Sort`, the group table and the savepoint block already follow. `pg_class_rows` is
   unbounded today and gets the bound with them.
5. **Every write is `42501`**, for the new relations as for the four that exist —
   `pg_catalog::refuse_write` already answers for any name it knows, so a view added to `ALL` is
   refused by being added.

## 4. Files

New:

```
crates/esker-sql/src/catalog/pg_relations.rs        the snapshot, and oid_of
crates/esker-sql/src/catalog/pg_attribute.rs        pg_attribute, pg_attrdef
crates/esker-sql/src/catalog/pg_index.rs            pg_index
crates/esker-sql/src/catalog/pg_constraint.rs       pg_constraint
crates/esker-sql/src/catalog/information_schema.rs  the five views
crates/esker-sql/src/catalog/def_functions.rs       format_type, pg_get_*, col_description
crates/esker-sql/tests/pg_catalog_attribute.rs      U1
crates/esker-sql/tests/pg_catalog_index.rs          U2
crates/esker-sql/tests/pg_catalog_constraint.rs     U3
crates/esker-sql/tests/pg_catalog_information.rs    U4
crates/esker-sql/tests/activerecord_schema_dump.rs  U5
crates/esker-sql/tests/corpus/pg19_catalog_*.txt    one per unit
```

Touched, and as little as possible:

```
crates/esker-sql/src/catalog/pg_catalog.rs   the view registry; the two oid fixes
crates/esker-sql/src/catalog/mod.rs          the new modules
crates/esker-sql/src/catalog/record.rs       a table-record range scan
crates/esker-sql/src/plan/expr.rs            Expr::CatalogFunc
crates/esker-sql/src/parse/lower.rs          the function names; the two schema qualifiers
crates/esker-sql/src/exec/cursor.rs          evaluating a catalog function
crates/esker-sql/src/exec/query.rs           resolving ::regclass before the plan
```

## 5. The fixture every corpus is captured over

```sql
CREATE TABLE ca (id bigserial PRIMARY KEY, a int8, b int4, c int2, d text, e varchar(5),
                 f varchar, g character(3), h json, j bool, k bytea, l timestamptz,
                 m timestamp(3), n float8, o float4);
CREATE TABLE cb (id int8 PRIMARY KEY, x int4 NOT NULL, y text DEFAULT 'hi',
                 z int8 DEFAULT 7, w timestamp);
CREATE TABLE cc (a int4, b text);                       -- no key, no NOT NULL, no default
CREATE TABLE cd (id int8 GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
                 q int8 GENERATED ALWAYS AS IDENTITY, r text);
CREATE UNIQUE INDEX cb_x_idx ON cb (x);
CREATE INDEX cb_yz_idx ON cb (y, z);
```

Fourteen types, a sequence-backed key, both identity kinds, a unique index, a multi-column index, a
table with nothing at all. Captured inside one `BEGIN … ROLLBACK` with `ON_ERROR_ROLLBACK on` and
`\gdesc` inline, so the shared `esker-pg19` oracle is left exactly as it was found.

## 6. Risks

* **An oid that disagrees between two views.** The reason for §3.3's single snapshot. The test that
  catches it is not a value assertion — it is `pg_class ⋈ pg_index ⋈ pg_attribute` returning the
  rows a client expects, which is what AR writes anyway.
* **A view that looks right on the fixture and is wrong on a schema.** `schema.rb` is 60 tables;
  U5's acceptance runs the real statements over the real fixture, not over one table.
* **Growing `pg_type`.** Column order is observable through `SELECT *`; the new column goes last.
* **The array lane's boundary.** Three of the five AR methods need arrays this lane must not build.
  They are named, refused and counted — not approximated.
