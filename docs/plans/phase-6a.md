# Phase 6a plan — a stateless node that speaks PostgreSQL

Status: **in progress** — written before implementation; §10 records progress and §11 what changed.
Spec: `prompts/06-sql-serverless.md` §6a. Constitution: `CLAUDE.md`. Design: `docs/DESIGN.md` §13
(the sketch), §3 (the `'t'` and `'m'` key layout), §8 (the transactions this sits on).

Every crate below this one implements a format **we** chose, and the only way to be wrong about it is
to disagree with our own documentation. `esker-sql` is the first crate that implements a format
**someone else** chose and shipped to millions of clients. There is no room to be creatively
different: a byte we get wrong is a `psql` that will not connect, and a NULL we sort in the wrong
place is a query that silently returns the wrong answer. So this plan leads with the compatibility
contract and treats the feature list as the thing that grows underneath it.

## 1. The compatibility contract (binding from the first commit)

The target is **PostgreSQL 19**. The contract has three layers, each independently testable, and it
is absolute from the first commit even though the executed-feature set in §4 grows one unit at a
time. Directive from the project owner, recorded here because it constrains every later decision.

### C1 — Syntax: no valid PG-19 statement is rejected as a parse error

Anything PostgreSQL 19 accepts, we accept *as syntax*. Parsing is delegated to `sqlparser`'s
PostgreSQL dialect (ADR 0014) at the newest published version.

This layer is bounded by something outside our control — `sqlparser`'s own coverage — so the
enforceable form of C1 is: **no gap is silent.** A valid PG-19 statement that `sqlparser` cannot
parse goes on the tracked upstream-gap list in §9 with its statement class and a decision (patch
upstream / pre-transform / accept), and its test stays in the corpus as a known-failing case. C1 is
violated by an *untracked* rejection, never by a tracked one. A gap discovered and not written into
§9 in the same commit is the failure mode this rule exists to prevent.

Tested by the **syntax corpus** (§7.1, §9): 353 statements across 17 classes — DDL, DML, DCL, TCL,
CTEs, window functions, set operations, `MERGE`, JSON, arrays, `LATERAL`, `RETURNING`, partitioning,
`EXPLAIN` variants. Every one of them was put to a real **PostgreSQL 19beta1** server and kept only
because that server's parser accepted it, so the corpus is a record of what PostgreSQL does rather
than of what we believe it does. It asserts `parse(sql).is_ok()` and nothing more; what a statement
*does* is C2's or C3's business.

### C2 — Honesty: parsed but unimplemented is PostgreSQL's own rejection

A statement that parses and that we cannot execute returns an `ErrorResponse` with SQLSTATE
**`0A000` `feature_not_supported`**, whose message names the feature. Never a parse error, never a
panic, never a wrong answer, never a silent no-op. After it, the session state machine is exactly
where PostgreSQL would leave it:

- outside a transaction → `ReadyForQuery('I')`;
- inside a transaction block → the transaction is aborted, `ReadyForQuery('E')`, and every
  subsequent statement until `ROLLBACK`/`Sync` gets `25P02 in_failed_sql_transaction`.

This is the layer that makes an unfinished executor safe to ship: the set of statements we execute
is small and grows, but the set we *mishandle* is empty from the first commit.

### C3 — Parity: what we do execute matches PostgreSQL 19 exactly

For the implemented subset, 100% behavioural parity, asserted in tests against documented PG
behaviour rather than against our own intuition:

| Surface | What parity means | Where asserted |
|---|---|---|
| Value text format | `INT8` decimal, `BOOL` as `t`/`f`, `BYTEA` as `\x…` hex, `TIMESTAMPTZ` as `2026-08-30 15:04:05.123456+00`, `DOUBLE` shortest-round-trip (`float8` `extra_float_digits=1` behaviour), NULL as a `-1` length in `DataRow` and never an empty string | `tests/parity_text_format.rs` |
| NULL semantics | three-valued `WHERE` (NULL is not false-but-visible), `NULLS LAST` for `ASC` and `NULLS FIRST` for `DESC`, `NULL = NULL` is unknown, unique indexes permit many NULLs | `tests/parity_null.rs` |
| SQLSTATE codes | the exact five-character code per condition (`23505` unique_violation, `42P01` undefined_table, `42703` undefined_column, `42P07` duplicate_table, `22P02` invalid_text_representation, `0A000`, `25P02`, `54001`, `08P01`) | `tests/parity_sqlstate.rs` |
| Command tags | `INSERT 0 3`, `UPDATE 2`, `DELETE 0`, `SELECT 5`, `CREATE TABLE`, `BEGIN`, `COMMIT`, `ROLLBACK` — including the `INSERT` tag's legacy leading OID zero | `tests/parity_command_tag.rs` |

Where PG's documented behaviour and a real local `psql`/server disagree, the real server wins and
the discrepancy is noted in the test.

One deliberate exclusion: the *message text* of a syntax error. `42601` is promised, and so is the
error's shape and the session state after it, but the prose ("syntax error at or near ...") and the
caret position come from a different parser and will not match token for token. Message text is
promised for the conditions in the table above, which are the ones users and drivers read.

### The version the goldens actually come from

PostgreSQL 19 is the *target*; the newest client on this machine is **psql 18.6**, so captured
byte-stream goldens are real bytes from 18.6, and the 3.0/3.2 wire surface they exercise is
unchanged in 19. Anything where 19 is known to differ is tracked in §9 rather than assumed away.
Goldens that could not be captured from a real client are marked
`// TODO(verify-against-real-psql)` and listed in the unit's report.

## 2. What the wire already told us

Two facts were measured against real `psql` 18.6 before this plan was written, because both change
the design and neither is safe to assume:

1. **`psql` 18.6 sends protocol 3.0 by default** (`0x00030000`), and **3.2** (`0x00030002`) when the
   connection string carries `max_protocol_version=latest`. So a 3.0-only server is reachable by
   today's default client — but the moment a client asks for 3.2, a server that answers with an
   error instead of `NegotiateProtocolVersion` is *unreachable*, and that default is expected to
   move. We implement the negotiation now, and golden-test it against the captured 3.2 startup
   packet (unit 2). The reply is `NegotiateProtocolVersion(newest_supported = 3.0, unrecognised
   `_pq_.` options)` followed by the ordinary 3.0 startup sequence — a downgrade, not a refusal.
2. **`sslmode=disable` sends no `SSLRequest`**; the default `sslmode=prefer` sends the 8-byte
   `SSLRequest` first. Both paths are golden-tested; we answer `SSLRequest` with a single `N` byte
   and continue in cleartext, which is the documented "server does not support SSL" path and is
   what lets a default-configured `psql` connect without `sslmode=disable`.

## 3. Scope

**In**, as executed features: `CREATE TABLE` / `DROP TABLE` (`INT8`, `TEXT`, `BOOL`, `BYTEA`,
`TIMESTAMPTZ`, `DOUBLE`; `PRIMARY KEY`, `UNIQUE`, `NOT NULL`), `CREATE INDEX` / `DROP INDEX`,
`INSERT` (multi-row, `RETURNING` later), `SELECT` with projection / `WHERE` / `ORDER BY` / `LIMIT` /
`OFFSET`, `UPDATE`, `DELETE`, `BEGIN` / `COMMIT` / `ROLLBACK`, `EXPLAIN`.

**Out**, as executed features — and therefore *in* as C2 `0A000` responses, which is a deliverable,
not an omission: joins, aggregates and `GROUP BY`, subqueries, CTEs, window functions, set
operations, `MERGE`, `COPY`, views, triggers, sequences and `SERIAL`, DCL (`GRANT`/`REVOKE`),
`ALTER TABLE`, savepoints, cursors, every type outside the six, and every `SET` that would change
behaviour we do not implement.

**Not in this crate at all**: the store-side execution of transactions (phase 5), region routing
(phase 4), and `COPY`-based bulk load (the acceptance target uses batched `INSERT`).

## 4. Units, and what each one commits

Each unit compiles, is tested, and is committed on its own.

| # | Unit | Lands |
|---|---|---|
| 1 | Plan + ADR 0014 + the dependency | this file, `docs/adr/0014-sqlparser.md`, `sqlparser` pinned in `[workspace.dependencies]`, the crate skeleton |
| 2 | `src/pgwire/` | framing, startup + `NegotiateProtocolVersion` + `SSLRequest`, trust and cleartext auth, simple query, extended query, `ErrorResponse`, goldens, decoder fuzz |
| 3 | `src/row.rs` | memcomparable PK into the `'t'` layout; the versioned tuple value format |
| 4 | `src/catalog.rs` | table/index definitions in `'m'`, versioned cache with a per-transaction check |
| 5 | `src/backend.rs` | the transactional trait the executor needs, plus an in-memory MVCC fake |
| 6 | `src/plan/` + `src/exec/` | rule-based planner, pull-based executor, `EXPLAIN` |
| 7 | tests | the `.slt`-style harness over every supported statement |

The C1/C2/C3 contract is not a unit. C2's `0A000` path lands in unit 2 (it is a wire behaviour and
needs no executor), the syntax corpus lands in unit 1 with the dependency, and C3's parity tests
land with the unit that implements each surface.

## 5. Layout and API sketch

```text
crates/esker-sql/
  src/
    lib.rs          crate docs + the invariants
    sqlstate.rs     SQLSTATE constants, named after PG's own condition names
    error.rs        SqlError -> (SQLSTATE, severity, message); the single place a code is chosen
    pgwire/
      mod.rs        the session state machine
      message.rs    frontend decode / backend encode, pure, no I/O
      startup.rs    startup, SSLRequest, NegotiateProtocolVersion, auth
      server.rs     the tokio edge: one socket -> one session
    parse.rs        sqlparser wrapper: the depth guard (§8 risk 1) + statement classification
    row.rs          tuple encode/decode, PK and index key building
    catalog.rs      TableDef/IndexDef, 'm'-space encoding, the versioned cache
    backend.rs      trait Backend + trait Txn; the in-memory fake behind #[cfg(any(test, feature))]
    plan/           logical -> physical, rule-based
    exec/           pull-based iterators
  src/bin/esker-sql.rs
  tests/            goldens, corpus, parity, .slt harness
```

The two seams that matter:

```rust
/// Everything the executor may ask of storage. Shaped to match `esker-client`'s TxnClient
/// (phase 5); the fake and the real client are the only implementations.
pub trait Backend: fmt::Debug + Send + Sync {
    fn begin(&self) -> Result<Box<dyn Txn>>;
}

pub trait Txn: fmt::Debug + Send {
    fn get(&mut self, key: &[u8]) -> Result<Option<Bytes>>;
    fn scan(&mut self, range: KeyRange, limit: usize) -> Result<Vec<(Bytes, Bytes)>>;
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&mut self, key: &[u8]) -> Result<()>;
    /// The unique-index seam: "this key must not exist at commit". Real enforcement is
    /// Percolator's conflict detection; the fake simulates it.
    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
    fn commit(self: Box<Self>) -> Result<()>;
    fn rollback(self: Box<Self>) -> Result<()>;
}
```

Synchronous, because `esker-client`'s `RawClient` is synchronous and `tokio` is meant to stay at the
socket edge (`CLAUDE.md`). The session runs the executor on a blocking task.

## 6. On-disk and on-wire formats this phase introduces

Both get a version byte first and a golden test, and an unknown version is a typed error, never a
panic (`CLAUDE.md` invariants 2 and 9).

- **Row value** — `version:u8=1 ++ null_bitmap:ceil(n/8) ++ non-null column values in column
  order`. Per type: `INT8` 8-byte LE two's complement; `BOOL` one byte 0/1; `DOUBLE` 8-byte LE
  IEEE-754; `TIMESTAMPTZ` `i64` LE microseconds since 2000-01-01 UTC (PG's own epoch, so a value
  round-trips through PG's binary format unchanged); `TEXT` and `BYTEA` varint length ++ bytes.
  The bitmap is first so a projection can skip a NULL column without decoding it.
- **Primary key** — `'t' ++ tenant:u64 ++ table_id:u64 ++ 'r' ++ memcomparable(pk columns)` via
  `esker-keys`, which is already prefix-free, so a composite PK cannot alias.
- **Index key** — `'t' ++ tenant ++ table_id ++ 'i' ++ index_id ++ memcomparable(index columns)
  [++ memcomparable(pk)]`. The trailing PK is present for a non-unique index (it is what makes the
  key unique) and absent for a unique one (its absence is what makes the uniqueness a key
  collision, which is exactly the conflict Percolator detects).
- **Catalog** — `'m' ++ "sql" ++ kind ++ tenant ++ id`, value a versioned record. A monotone
  `catalog_version:u64` at `'m' ++ "sql" ++ 'v'` is read once per transaction; a cached definition
  from an older version is discarded.

## 7. Tests

Required kinds per `docs/DESIGN.md` §11, plus the two the contract adds.

1. **Syntax corpus** (C1) — 353 statements per statement class, each verified against a real
   PostgreSQL 19beta1 server, asserting only that the parse succeeds. Known upstream gaps live in
   one `KNOWN_GAPS` table beside the test and are held from both sides: an *unlisted* failure fails
   the build, and so does a *listed* gap that has started passing, so a `sqlparser` upgrade that
   closes one is noticed instead of silently absorbed.
2. **Contract tests** (C2) — every unimplemented statement class returns `0A000` naming the feature,
   and the session state machine is correct afterwards, inside and outside a transaction block.
3. **Parity tests** (C3) — the four tables in §1.
4. **Golden byte streams** — real captured `psql` 18.6 sessions for: 3.0 startup, 3.2 startup →
   `NegotiateProtocolVersion`, `SSLRequest` → `N`, simple query, extended query, error mid-transaction.
5. **Fuzz** — the frontend message decoder never panics on arbitrary bytes; the row and catalog
   decoders never panic on arbitrary bytes (invariant 9). Run as a bounded proptest in `just check`.
6. **Property tests** — row and index encodings round-trip; encoded key order equals logical column
   order for every type and for composite keys.
7. **`.slt` harness** — our own runner over `tests/slt/*.slt`, one file per statement class, against
   the fake backend. The real `sqllogictest` crate and the `psql` smoke test are phase-6a
   acceptance, once the real backend is wired.

## 8. Risks

1. **The parser's stack-overflow protection is unavailable to us.** `sqlparser`'s
   `recursive-protection` feature pulls `psm`, which compiles assembly, and 17 other crates
   including `cc` -- a hard violation of the pure-Rust rule (ADR 0014 records the measurement).
   Without it, deeply nested SQL overflows the stack, and that is invariant 9 broken in the least
   recoverable way available. **Resolved in unit 1**, by measurement rather than by guess. On the
   2 MiB stack a `tokio` blocking thread gets, `sqlparser` costs ~6 KiB of stack per nested
   parenthesis, ~7 KiB per nested `CASE`, and **~40 KiB per nested subquery** -- `SELECT * FROM
   (SELECT * FROM (...))` overflows 2 MiB at depth 60. So:
   - `parse::nesting_depth` scans the statement before the parser is entered, understanding
     PostgreSQL's quoting, dollar quoting and nested comments so that a `(` inside a string is not
     miscounted -- miscounting there would reject a valid statement, which is C1 broken;
   - past `MAX_NESTING_DEPTH` (1,000, chosen to be PostgreSQL's own order of magnitude) the
     statement is refused with `54001 statement_too_complex`, which is exactly what PostgreSQL
     raises when `max_stack_depth` is exceeded -- the guard is a parity behaviour, not a deviation;
   - below 16 levels the parse runs on the caller's stack; deeper, it runs on a 64 MiB thread,
     verified to survive 1,000 levels of the worst construct. Ordinary statements never leave the
     caller's thread and pay nothing.

1b. **`sqlparser`'s own recursion limit defaults to 50 and rejects valid PostgreSQL.** Found in unit
   1 while measuring the above: `SELECT ((((...1...))))` with 51 parentheses comes back
   `recursion limit exceeded` from the default parser, and PostgreSQL accepts it. Left alone this
   would have broken contract C1 on the first deeply-parenthesised query a client sent, silently
   and as a *syntax* error. The limit is raised above anything the depth guard admits, and if it
   fires anyway it is reported as `54001`, never as a syntax error. This is the clearest evidence
   so far that C1 needs the corpus: nothing about the dependency advertised this.

2. **`sqlparser` gaps against PG 19** are discovered, not predicted. §9 is the register; the corpus
   is what finds them.
3. **The real backend does not exist yet** (phase 5 lane). Mitigation: unit 5's trait, and reading
   that lane's landed commits before finalising it. A mismatch is reported, not guessed at.
4. **`ORDER BY` on a non-indexed column is an in-memory sort** with a documented row limit for v1;
   beyond it, `0A000`-adjacent `53400 configuration_limit_exceeded` rather than an OOM.
5. **Extended-protocol lifecycle leaks.** Portals and prepared statements are per-session state with
   destroy rules that are easy to get subtly wrong; the error-mid-transaction golden is the test that
   pins it.

## 9. Upstream gap register — `sqlparser` 0.62.0 vs PostgreSQL 19

### How this was measured

Not from memory. A real **PostgreSQL 19beta1** server (`postgres:19beta1`, the target release in
beta at the time of writing) was run locally and every candidate statement put to it. A statement
earns its place in `tests/corpus/pg19.sql` when that server answers with anything other than
`42601 syntax_error` — "no such table" means the grammar was satisfied and only the catalog was
not. Two candidates were thrown out that way (`FETCH FIRST … WITH TIES` without `ORDER BY`,
`EXCLUDE CURRENT ROW` without a frame clause); both looked correct, which is the argument for
having an oracle rather than an opinion.

The corpus is **353 statements across 17 classes**. Run through `sqlparser` 0.62.0's PostgreSQL
dialect, **281 parse and 72 do not** — 79.6% coverage. Those 72 are below, grouped into the 30
features they come from, and each is in `KNOWN_GAPS` in `tests/syntax_corpus.rs`.

### What the shape of the gap means

Nearly all of it is administrative surface: replication, foreign data wrappers, `VACUUM`, role
management, two-phase commit. Esker will not execute any of it, and a stateless SQL node in front
of a distributed store is not where an operator runs `ALTER SYSTEM`.

The rows marked **on the query path** are the ones that matter, because they sit inside the kind of
statement phase 6a *does* execute — `TABLE t` is a `SELECT`, `GROUP BY DISTINCT` is a query, and
`SELECT a FROM t FOR KEY SHARE` is a read a real application writes. Today each of those comes back
as a **syntax error**, which is precisely the contract C1 failure the register exists to make
visible: the honest answer is `0A000`, naming the feature, and it cannot be given for a statement
that never parsed. These are therefore the gaps to close first, and closing them is what the
decision column tracks.

### The register

| # | Feature | Statements | Minimal repro | Priority |
|---|---|---|---|---|
| G01 | partition maintenance | 2 | `ALTER TABLE t ATTACH PARTITION p FOR VALUES FROM (1) TO (10);` | admin / DDL only |
| G02 | unlogged / logged tables | 2 | `CREATE UNLOGGED TABLE t (a int8);` | admin / DDL only |
| G03 | CREATE TABLE LIKE / OF | 2 | `CREATE TABLE t (LIKE u INCLUDING ALL);` | admin / DDL only |
| G04 | exclusion constraints | 2 | `CREATE TABLE t (a int8, EXCLUDE USING gist (a WITH =));` | admin / DDL only |
| G05 | index maintenance | 4 | `CREATE INDEX i ON ONLY t (a);` | admin / DDL only |
| G06 | views: recursive, materialized | 3 | `CREATE RECURSIVE VIEW v (n) AS SELECT 1;` | admin / DDL only |
| G07 | sequence options | 2 | `CREATE SEQUENCE s START WITH 1 INCREMENT BY 1;` | admin / DDL only |
| G08 | routine bodies | 5 | `CREATE FUNCTION f() RETURNS int8 BEGIN ATOMIC SELECT 1; END;` | admin / DDL only |
| G09 | INSERT OVERRIDING | 1 | `INSERT INTO t (a) OVERRIDING SYSTEM VALUE VALUES (1);` | **on the query path** |
| G10 | MERGE ... DO NOTHING | 1 | `MERGE INTO t USING u ON t.id = u.id WHEN MATCHED AND u.a > 0 THEN DO NOTHING;` | **on the query path** |
| G11 | GROUP BY DISTINCT | 1 | `SELECT a FROM t GROUP BY DISTINCT a;` | **on the query path** |
| G12 | row-level locking clauses | 2 | `SELECT a FROM t FOR NO KEY UPDATE OF t NOWAIT;` | **on the query path** |
| G13 | TABLE as a query | 1 | `TABLE t;` | **on the query path** |
| G14 | SELECT with no list | 1 | `SELECT;` | **on the query path** |
| G15 | JOIN USING alias | 1 | `SELECT * FROM t JOIN u USING (id) AS j;` | **on the query path** |
| G16 | ROWS FROM | 1 | `SELECT * FROM ROWS FROM (generate_series(1, 2), generate_series(3, 4)) WITH ORDINALITY;` | **on the query path** |
| G17 | recursive CTE SEARCH/CYCLE | 2 | `WITH RECURSIVE w (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM w WHERE n < 5) SEARCH DEPTH FIRST BY n SET o SELECT * FROM w;` | **on the query path** |
| G18 | window frame EXCLUDE | 2 | `SELECT sum(a) OVER (ORDER BY b GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM t;` | **on the query path** |
| G19 | BETWEEN SYMMETRIC | 1 | `SELECT a BETWEEN 1 AND 10, a NOT BETWEEN SYMMETRIC 10 AND 1 FROM t;` | **on the query path** |
| G20 | TRIM keyword forms | 1 | `SELECT TRIM(BOTH ' ' FROM b), TRIM(LEADING FROM b), TRIM(TRAILING 'x' FROM b) FROM t;` | **on the query path** |
| G21 | JSON_QUERY wrapper | 1 | `SELECT JSON_QUERY('{"a":1}', '$' WITH WRAPPER);` | **on the query path** |
| G22 | transaction modes | 3 | `BEGIN WORK ISOLATION LEVEL SERIALIZABLE READ WRITE DEFERRABLE;` | admin / DDL only |
| G23 | two-phase commit | 3 | `PREPARE TRANSACTION 'gid';` | admin / DDL only |
| G24 | role grants and privileges | 5 | `GRANT alice TO bob WITH ADMIN OPTION;` | admin / DDL only |
| G25 | VACUUM / CLUSTER / CHECKPOINT | 4 | `VACUUM (FULL, ANALYZE, VERBOSE) t;` | admin / DDL only |
| G26 | cursor MOVE | 1 | `MOVE BACKWARD 1 IN c;` | admin / DDL only |
| G27 | database and system admin | 8 | `CREATE DATABASE d WITH OWNER alice ENCODING 'UTF8';` | admin / DDL only |
| G28 | extended statistics | 2 | `CREATE STATISTICS st ON a, b FROM t;` | admin / DDL only |
| G29 | logical replication | 6 | `CREATE PUBLICATION pub FOR TABLE t;` | admin / DDL only |
| G30 | foreign data wrappers | 2 | `CREATE FOREIGN TABLE ft (a int8) SERVER srv;` | admin / DDL only |

**Decision, for every row above:** carry the gap, do not fork. The corpus keeps each statement, and
`every_known_gap_is_still_a_gap` fails the build the day an upstream release starts parsing one, so
a `sqlparser` upgrade is checked against the register automatically rather than by someone
remembering to look. For the on-the-query-path rows, the alternative if upstream stays quiet is a
pre-parse rewrite in `parse.rs` for the handful that are simple aliases — `TABLE t` is
`SELECT * FROM t` and nothing else — which is cheap and contained. Nothing here justifies a fork of
the parser, and nothing here is a reason to reconsider ADR 0014: a hand-written parser would have
its own gap register, and it would be longer.

## 10. Progress

- [x] 1 — plan, ADR 0014, dependency, crate skeleton, SQLSTATE table, error type, parse guard
- [x] 1b — the syntax corpus: 353 statements, oracle-verified, 72 gaps registered in §9
- [ ] 2 — pgwire
- [ ] 3 — row and tuple encodings
- [ ] 4 — catalog
- [ ] 5 — backend trait and fake
- [ ] 6 — planner and executor
- [ ] 7 — `.slt` harness

## 11. What changed, and why

**Unit 1.** Three things came out differently from the sketch above.

- *The SQLSTATE table moved up a level.* It was going to live in `pgwire/codes.rs`, but `error.rs`
  needs it and `pgwire` does not exist yet, so it is `src/sqlstate.rs`. The layout in §5 says so.
- *Risk 1 turned into a design instead of a mitigation.* The plan expected a depth guard; the
  measurements (§8.1) showed a single constant could not be both safe on a 2 MiB stack and
  generous enough to be PostgreSQL-shaped, so the parse is two-tier: inline when shallow, on a
  64 MiB thread when deep.
- *A second risk appeared and was closed the same day* (§8.1b): the dependency's own recursion
  limit is 50, which rejects SQL that PostgreSQL accepts. It was found by measuring, not by
  reading, which is the argument for building the corpus next rather than last.

**Unit 1b.** The corpus was going to be written from the PostgreSQL documentation. It is instead
written against a **running PostgreSQL 19beta1**, because a container of the target release turned
out to be one `docker run` away and an oracle beats a recollection — it immediately rejected two
statements that had looked right. The result is the first hard number this contract has: of 353
statements real PostgreSQL 19 accepts, `sqlparser` parses **281**. The other 72 are registered in
§9, and the thirteen of them that sit on the query path are now the most concrete piece of work
this plan has, because each is a statement a user could write today and get a syntax error for
where the contract promises `0A000`.
