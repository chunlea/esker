# Phase 6 — SQL surface and serverless hooks

Two independent tracks that both sit on top of the finished KV. Do them in separate plans
(`docs/plans/phase-6a.md`, `phase-6b.md`); 6b can start as soon as phase 4 is stable.

## 6a — `esker-sql`: a stateless PostgreSQL-speaking node

Goal: `psql` connects to an Esker SQL node and runs a useful subset of PostgreSQL against tables stored
in the `'t'` key space (`docs/DESIGN.md` §3) through `esker-client` transactions. The node holds no state
except caches; any number can run behind a load balancer.

1. Wire protocol implemented in-house (`esker-sql/src/pgwire/`, PostgreSQL protocol v3): startup and
   authentication (trust + cleartext password), simple and extended query protocols, `psql`-compatible
   error responses, `SSLRequest` refused cleanly. Golden-tested against captured `psql` byte streams.
2. Parsing with the `sqlparser` crate (PostgreSQL dialect) — the one large dependency exception; write
   `docs/adr/00NN-sqlparser.md` before adding it, stating what a replacement would cost. Supported in the
   first milestone: `CREATE/DROP TABLE`
   (INT8, TEXT, BOOL, BYTEA, TIMESTAMPTZ, DOUBLE; PRIMARY KEY; UNIQUE), `CREATE INDEX`, `INSERT`,
   `SELECT` with projection, `WHERE`, `ORDER BY`, `LIMIT`, single-table `JOIN` later; `UPDATE`,
   `DELETE`, `BEGIN/COMMIT/ROLLBACK`, `EXPLAIN`.
3. Catalog in the `'m'` key space, versioned, cached per node with a version check per transaction.
4. Row encoding: memcomparable primary key in the key; the row as a hand-rolled versioned tuple format
   in the value (golden-tested); secondary index keys per DESIGN.md §3; unique indexes enforce via
   Percolator conflict detection.
5. Planner: rule-based; point lookup / index range scan / full scan; predicate pushdown into `Scan`
   ranges; simple cost by estimated key count. Executor: pull-based iterators over the txn client.
6. Tests: the `sqllogictest` crate against a curated subset of the SQLite logic tests plus your own
   `.slt` files for every supported statement; `psql` smoke test in CI; the bank test rewritten in SQL.

Acceptance: `psql -h localhost -p 5432` creates a table, inserts 1M rows through `COPY`-free batched
inserts, runs indexed and unindexed queries with `EXPLAIN` output showing the chosen plan, and every
`.slt` file passes.

## 6b — Serverless hooks: SST tiering and scale-to-zero

1. `FileSystem` trait already used by the engine (DESIGN.md §13) gets an S3-backed implementation with
   an in-house minimal S3 client (PutObject, ranged GetObject, ListObjectsV2, DeleteObject; SigV4 with
   in-house SHA-256/HMAC, golden-tested against the AWS signing test vectors): SSTs written locally then
   uploaded on flush/compaction completion; reads served from a local disk cache with the block cache in
   front; manifest records the SST's location. WAL and Raft log stay local. Two ADRs first: failure
   semantics (upload fails → SST stays local; local disk full → backpressure) and **TLS** (DESIGN.md §13
   lists the options; plain HTTP to MinIO is acceptable for the first milestone).
2. Region hibernation in PD (ADR + design only, unless time allows): cold regions drop their Raft
   heartbeat frequency; SQL nodes are stateless so idle tenants cost nothing above storage.
3. Bench: `readrandom` with a cold local cache and SSTs on MinIO; record the p99 and the cache hit
   rate in `docs/bench/phase-6b.md`.

Acceptance: a store can be started with `--sst-store s3://bucket/prefix` against MinIO, lose its local
disk, and rebuild from object storage plus its Raft peers with no data loss; bench recorded.
