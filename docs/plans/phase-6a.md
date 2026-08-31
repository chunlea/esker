# Phase 6a plan — a stateless node that speaks PostgreSQL

Status: **complete** — written before implementation; §10 records progress and §11 what changed.
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

This layer is bounded by something outside our control — `sqlparser`'s own coverage — so C1's
enforceable form is stronger than "no gap is silent". It is: **no statement PostgreSQL 19 accepts is
ever answered with a syntax error.** There are exactly two permitted answers, and a gap in the
parser changes which one is given, never whether the answer is honest:

* the statement parses; or
* it comes back `0A000 feature_not_supported` **naming the construct** — C2's answer, reached
  through the recognizer table in `parse.rs` (§9).

`42601 syntax_error` about valid PostgreSQL is forbidden outright, because it is untrue and
unactionable: it tells a user to fix a statement that is already correct, and points at a keyword
that is perfectly valid. Measured over the whole corpus: **307 parse, 95 are refused by name, none
is a syntax error.** Every gap is also a tracked row in §9, so a gap that is merely *undocumented*
still fails the build even though the client would have been answered correctly.

Tested by the **syntax corpus** (§7.1, §9): 402 statements across 17 classes — DDL, DML, DCL, TCL,
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
`OFFSET`, one **inner `JOIN`** of two tables, `UPDATE`, `DELETE`, `BEGIN` / `COMMIT` / `ROLLBACK`,
`EXPLAIN`.

**Out**, as executed features — and therefore *in* as C2 `0A000` responses, which is a deliverable,
not an omission: outer, natural and `USING` joins and any second join in one statement, aggregates
and `GROUP BY`, subqueries, CTEs, window functions, set
operations, `MERGE`, `COPY`, views, triggers, sequences and `SERIAL`, DCL (`GRANT`/`REVOKE`),
savepoints, cursors, every type outside the six, and every `SET` that would change behaviour we do
not implement. `ALTER TABLE` was here too; it is now in, for `ADD COLUMN` of a nullable column
only, and every other action of it is a `0A000` naming itself (§11, the ALTER continuation).

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
/// Everything the executor may ask of storage. Shaped against `esker-client`'s real
/// `TxnClient`/`Transaction`, read from the phase-5 lane's landed commits rather than guessed.
pub trait Backend: fmt::Debug + Send + Sync {
    fn begin(&self) -> Result<Box<dyn Txn>>;
}

pub trait Txn: fmt::Debug + Send {
    fn get(&self, key: &[u8]) -> Result<Option<Bytes>>;
    fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>>;
    /// Buffered client-side, so there is nothing to fail yet — see the note below.
    fn put(&mut self, key: &[u8], value: &[u8]);
    fn delete(&mut self, key: &[u8]);
    /// The commit timestamp, or `None` for a transaction that wrote nothing.
    fn commit(self: Box<Self>) -> Result<Option<u64>>;
    fn rollback(self: Box<Self>) -> Result<()>;
}
```

### What the real `TxnClient` says

`esker-client::txn` landed while this plan was being written, so unit 5's trait is aligned to it
rather than to the sketch it replaced. Three differences worth recording, because each would
otherwise be discovered as a compile error at wiring time:

- **`put` and `delete` return nothing.** Writes are buffered on the client until commit, so there
  is no failure to report at the call site; a conflict surfaces from `commit`. The trait matches,
  which also means the executor must not be written as though a write can fail where it is issued.
- **`commit` returns `Option<u64>`** — the commit timestamp, and `None` when the transaction wrote
  nothing and therefore never needed one.
- **`get` and `scan` take `&self`**, because read-your-writes is served from the buffer without
  mutating it.

### Unique indexes, with no new client method (decided)

The sketch wanted a `put_if_absent` as the unique-index seam. **Decided: there is no such method,
and there will not be one.** Uniqueness composes out of what `TxnClient` already has, and the
composition covers both of the ways a duplicate can arrive:

1. **`get` the index key inside the transaction; it must come back absent.** This is what catches a
   duplicate that is *already committed* — the read is at the transaction's snapshot, so a
   committed index entry is visible and the executor raises `23505 unique_violation` before writing
   anything.
2. **Then an ordinary `put`.** This is what catches a *concurrent* duplicate, and it needs no help:
   two transactions inserting the same value both read the index key as absent, both prewrite the
   same key, and Percolator's write-write conflict detection means exactly one of them commits. The
   loser gets a conflict from `commit`, which the executor reports as `23505`.

So the enforcement was never the client's job to expose — it is what the protocol in
`docs/DESIGN.md` §8 already does, reached by writing the index entry like any other key. A
dedicated method would have been a second name for prewrite.

`TODO(post-v1)`: the read in step 1 costs a round trip per unique index per row, and a bulk insert
into an empty table pays it for every row to learn nothing. The optimisation is to presume the key
absent and let the prewrite conflict be the whole check, reporting `23505` when it fires — correct
for the concurrent case already, and for the committed case only once the prewrite can distinguish
"a value exists" from "someone else is writing". Not v1: it trades a clear error for a faster one.

Synchronous, because `esker-client`'s `RawClient` is synchronous and `tokio` is meant to stay at the
socket edge (`CLAUDE.md`). The session runs the executor on a blocking task.

## 6. On-disk and on-wire formats this phase introduces

Both get a version byte first and a golden test, and an unknown version is a typed error, never a
panic (`CLAUDE.md` invariants 2 and 9).

- **Row value** — `version:u8=2 ++ columns:varint ++ null_bitmap:ceil(columns/8) ++ non-null
  column values in column order`. The count is what makes `ALTER TABLE ADD COLUMN` rewrite nothing:
  a row narrower than the table reads back padded with NULL, and one wider is corruption
  ([ADR 0019](../adr/0019-a-row-says-how-many-columns-it-has.md), which also records why version 1
  — no count — is refused rather than kept readable). Per type: `INT8` 8-byte LE two's complement; `BOOL` one byte 0/1; `DOUBLE` 8-byte LE
  IEEE-754; `TIMESTAMPTZ` `i64` LE microseconds since 2000-01-01 UTC (PG's own epoch, so a value
  round-trips through PG's binary format unchanged); `TEXT` and `BYTEA` varint length ++ bytes.
  The bitmap is first so a projection can skip a NULL column without decoding it.
- **Primary key** — `'t' ++ tenant:u64 ++ table_id:u64 ++ 'r' ++ memcomparable(pk columns)` via
  `esker-keys`, which is already prefix-free, so a composite PK cannot alias.
- **Index key** — `'t' ++ tenant ++ table_id ++ 'i' ++ index_id ++ (null_marker:u8 ++
  memcomparable(index column))* [++ memcomparable(pk)]`. The trailing PK is present for a
  non-unique index (it is what makes the key unique) and absent for a unique one (its absence is
  what makes the uniqueness a key collision, which is exactly the conflict Percolator detects) —
  **except** for a unique entry with a NULL in it, which keeps the suffix, because PostgreSQL
  admits any number of NULLs in a `UNIQUE` column and two of them must not collide. The marker
  byte is `0x00` for a value and `0x01` for a NULL, so NULLs sort last, which is PostgreSQL's
  default for an ascending index; without it a NULL and an empty string would encode alike
  (built in unit 3).
- **Text ordering is byte ordering.** A key space compares bytes, so `ORDER BY` and an index scan
  over `TEXT` give what PostgreSQL gives under `COLLATE "C"` and not what its `en_US.utf8` default
  gives — `B` before `a`, not `a` before `B`. Declared, not stumbled into: a locale-aware
  collation means ICU or a platform C library and this project compiles no C. The ordering fixture
  is captured with `COLLATE "C"` for exactly this reason.
- **Catalog** — `'m' ++ "sql" ++ kind ++ tenant ++ id`, value a versioned record. A monotone
  `catalog_version:u64` at `'m' ++ "sql" ++ 'v'` is read once per transaction; a cached definition
  from an older version is discarded. As built (unit 4) the kinds are `'t'` a table — **with its
  indexes inside it**, so that a cache entry and a consistency unit are the same thing — `'n'` a
  name, and the two counters `'v'` and `'s'` (the per-tenant relation-id sequence). One name map
  serves tables and indexes together, because PostgreSQL keeps both in `pg_class` and really does
  answer `42P07` when an index takes a table's name.

## 7. Tests

Required kinds per `docs/DESIGN.md` §11, plus the two the contract adds.

1. **Syntax corpus** (C1) — 402 statements per statement class, each verified against a real
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

The corpus is **402 statements across 17 classes**. Of those, **307 parse and 95 come back as
`0A000 feature_not_supported` naming the construct. None is a syntax error.** That last number is
the one that matters, and it is asserted by
`no_statement_postgresql_accepts_is_ever_a_syntax_error`.

Raw parser coverage is 305 of 402 (75.9%); the other two of the 307 are `TABLE t` and `ABORT`,
which PostgreSQL *defines* as synonyms for `SELECT * FROM t` and `ROLLBACK`, so `parse.rs` rewrites
the leading keyword and they execute as the statements they are documented to equal. The remaining
95 are the register below.

The corpus grew by 47 statements and the register by 25 when the `ALTER TABLE` grammar was swept in
one pass (§11, the ALTER continuation). The coverage *percentage* went down as a result, which is
the right direction for it to move: it was measuring a corpus that had not looked at that grammar,
not a parser that could read it.

### Why none of this is a syntax error any more

The first version of this register recorded 72 statements that came back `42601 syntax_error`. That
was a defect in its own right, and a worse one than the missing features behind it: telling a user
that correct PostgreSQL is malformed is both untrue and unactionable, and it points them at a
keyword that is perfectly valid. A missing feature and a typo are different things and the client
is owed the difference.

So `parse.rs` carries a recognizer table — leading-keyword and construct patterns, one row per
feature in this register — that is consulted **only after a parse has already failed**. When it
names the construct, the answer is `0A000 feature_not_supported` naming it; when it does not, the
statement really is malformed and the answer stays `42601`. Being consulted only after failure is
what makes a loose pattern safe: it can only re-describe something that was going to be an error
anyway.

Two tests hold both directions. `no_statement_postgresql_accepts_is_ever_a_syntax_error` forbids
`42601` for anything in the corpus, and `malformed_sql_is_still_a_syntax_error` forbids `0A000` for
a typo — a recognizer that swallowed real syntax errors would pass the first test and be useless.
That second test found the one false positive this design admits: `SELECT 1 +` has exactly one
*word*, so an empty-target-list rule matching on word count claimed it. Recognising `SELECT;` from
the source text rather than the word list fixed it.

### The other direction: statements PostgreSQL rejects and `sqlparser` accepts

Not part of C1, C2 or C3 as written, but found while testing and worth recording rather than
discovering later. `sqlparser` is in places *more* permissive than PostgreSQL: `SELECT FROM WHERE`
and `DELETE FROM WHERE` both parse, reading `WHERE` as a table name. PostgreSQL rejects both with
`42601`.

The practical consequence is small — such a statement fails a moment later with `42P01 relation
"where" does not exist` instead of a syntax error, so it is a wrong *code* on input that was going
to fail regardless, never a wrong answer. Closing it properly needs PostgreSQL's grammar, which is
the thing ADR 0014 declined to reimplement. Recorded as a known divergence; revisit only if a real
client is confused by it.

#### A deliberate one, added in phase 8: `columnar_replicas`

`ALTER TABLE t SET (columnar_replicas = <n>)` is accepted here and refused by PostgreSQL 19 —
`22023 unrecognized parameter "columnar_replicas"`. Unlike the two above, this one is *chosen*
rather than inherited from `sqlparser`, so it is worth being explicit about what was checked
before choosing it.

There is no spelling that would not diverge. A custom storage parameter has to be either
unqualified, which is `unrecognized parameter`, or namespaced, which is `unrecognized parameter
namespace "esker"` — only `toast` is a namespace PostgreSQL knows. Both measured, both in
`crates/esker-sql/tests/corpus/pg19_storage_parameters.txt`. So the choice was never "compatible
or divergent", it was "which divergent spelling", and the storage-parameter shape is the one a
PostgreSQL user already knows how to type and the one ADR 0022 Decision 5 asked for.

The capture also recorded the asymmetry that reading the documentation would have missed:
`RESET` of a parameter that has never existed anywhere is **accepted**, while `SET` of the same
name errors. `RESET` does not validate names at all. So a client that sends
`ALTER TABLE t RESET (columnar_replicas)` gets success from both servers, which is the one half of
this surface where the two agree — for different reasons, and worth knowing before somebody reads
the agreement as compatibility.

### Keeping the oracle, for acceptance

The oracle is not a one-off. It is how this plan's remaining units get their evidence, and it is
how phase-6a acceptance should do **differential testing**: run a statement against Esker and
against real PostgreSQL 19 and compare the answers, rather than comparing Esker against what we
wrote down about PostgreSQL. Value text formats, NULL ordering, SQLSTATE codes and command tags are
all things a differential run checks for free and a hand-written assertion checks only where
somebody thought to look.

Reproducing it:

```sh
docker run -d --name esker-pg19 -e POSTGRES_HOST_AUTH_METHOD=trust \
    -e POSTGRES_USER=esker -p 55432:5432 postgres:19beta1
```

`POSTGRES_HOST_AUTH_METHOD=trust` matters: without it the server negotiates SCRAM, and the startup
goldens would then record an authentication exchange this node does not implement. Byte-level
captures were taken through a recording proxy sitting between `psql` and that container, which is
what `tests/golden/pgwire.hex` holds.

### What the shape of the gap means

Nearly all of it is administrative surface: replication, foreign data wrappers, `VACUUM`, role
management, two-phase commit. Esker will not execute any of it, and a stateless SQL node in front
of a distributed store is not where an operator runs `ALTER SYSTEM`.

The rows marked **on the query path** are the ones that matter, because they sit inside the kind of
statement phase 6a *does* execute — `GROUP BY DISTINCT` is a query, and `SELECT a FROM t FOR KEY
SHARE` is a read a real application writes. Each of those now names itself in a `0A000`, which is
the honest answer; they remain the rows to close first if they are to be *executed* rather than
merely refused well.

### The register

Thirty-seven features, ninety-five statements. G13 (`TABLE t`) and `ABORT` from G22 are closed:
both are documented synonyms and are now rewritten rather than refused.

G32 to G39 came from one sweep of the `ALTER TABLE` grammar while `ADD COLUMN` was being built
(§11, the ALTER continuation). The corpus had fifteen `ALTER TABLE` lines and the grammar has some
thirty actions, so **eighteen statements PostgreSQL 19 accepts were coming back `42601`** — contract
C1 broken, silently, for a year of the plan's life. The lesson is the one §7 already states and this
is the sharpest evidence for it: a corpus is only a gate for what somebody thought to put in it, and
the cheapest way to find what is missing is to sweep a whole statement's grammar at the server
rather than to add the lines a feature happens to need.

| # | Feature | Statements | Minimal repro | Priority |
|---|---|---|---|---|
| G01 | partition maintenance | 2 | `ALTER TABLE t ATTACH PARTITION p FOR VALUES FROM (1) TO (10);` | admin / DDL only |
| G02 | unlogged / logged tables | 2 | `CREATE UNLOGGED TABLE t (a int8);` | admin / DDL only |
| G03 | CREATE TABLE LIKE / OF | 2 | `CREATE TABLE t (LIKE u INCLUDING ALL);` | admin / DDL only |
| G04 | exclusion constraints | 2 | `CREATE TABLE t (a int8, EXCLUDE USING gist (a WITH =));` | admin / DDL only |
| G05 | index maintenance | 3 | `CREATE INDEX i ON ONLY t (a);` | admin / DDL only. **Narrowed in phase 6e**: `DROP INDEX CONCURRENTLY` left this row — the parser cannot read the keyword, so the source is rewritten and the fact carried alongside (`crate::parse::Parsed::concurrently`), which is what `rewrite_synonym` already does for `TABLE t` and `ABORT`. |
| G06 | views: recursive, materialized | 3 | `CREATE RECURSIVE VIEW v (n) AS SELECT 1;` | admin / DDL only |
| G07 | sequence options | 2 | `CREATE SEQUENCE s START WITH 1 INCREMENT BY 1;` | admin / DDL only |
| G08 | routine bodies | 5 | `CREATE FUNCTION f() RETURNS int8 BEGIN ATOMIC SELECT 1; END;` | admin / DDL only |
| G09 | INSERT OVERRIDING | 1 | `INSERT INTO t (a) OVERRIDING SYSTEM VALUE VALUES (1);` | **on the query path** |
| G10 | MERGE ... DO NOTHING | 1 | `MERGE INTO t USING u ON t.id = u.id WHEN MATCHED AND u.a > 0 THEN DO NOTHING;` | **on the query path** |
| G11 | GROUP BY DISTINCT | 1 | `SELECT a FROM t GROUP BY DISTINCT a;` | **on the query path** |
| G12 | row-level locking clauses | 2 | `SELECT a FROM t FOR NO KEY UPDATE OF t NOWAIT;` | **on the query path** |
| G14 | SELECT with no list | 1 | `SELECT;` | **on the query path** |
| G15 | JOIN USING alias | 1 | `SELECT * FROM t JOIN u USING (id) AS j;` | **on the query path** |
| G16 | ROWS FROM | 1 | `SELECT * FROM ROWS FROM (generate_series(1, 2), generate_series(3, 4)) WITH ORDINALITY;` | **on the query path** |
| G17 | recursive CTE SEARCH/CYCLE | 2 | `WITH RECURSIVE w (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM w WHERE n < 5) SEARCH DEPTH FIRST BY n SET o SELECT * FROM w;` | **on the query path** |
| G18 | window frame EXCLUDE | 2 | `SELECT sum(a) OVER (ORDER BY b GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM t;` | **on the query path** |
| G19 | BETWEEN SYMMETRIC | 1 | `SELECT a BETWEEN 1 AND 10, a NOT BETWEEN SYMMETRIC 10 AND 1 FROM t;` | **on the query path** |
| G20 | TRIM keyword forms | 1 | `SELECT TRIM(BOTH ' ' FROM b), TRIM(LEADING FROM b), TRIM(TRAILING 'x' FROM b) FROM t;` | **on the query path** |
| G21 | JSON_QUERY wrapper | 1 | `SELECT JSON_QUERY('{"a":1}', '$' WITH WRAPPER);` | **on the query path** |
| G22 | transaction modes | 2 | `BEGIN WORK ISOLATION LEVEL SERIALIZABLE READ WRITE DEFERRABLE;` | admin / DDL only |
| G23 | two-phase commit | 3 | `PREPARE TRANSACTION 'gid';` | admin / DDL only |
| G24 | role grants and privileges | 5 | `GRANT alice TO bob WITH ADMIN OPTION;` | admin / DDL only |
| G25 | VACUUM / CLUSTER / CHECKPOINT | 4 | `VACUUM (FULL, ANALYZE, VERBOSE) t;` | admin / DDL only |
| G26 | cursor MOVE | 1 | `MOVE BACKWARD 1 IN c;` | admin / DDL only |
| G27 | database and system admin | 8 | `CREATE DATABASE d WITH OWNER alice ENCODING 'UTF8';` | admin / DDL only |
| G28 | extended statistics | 2 | `CREATE STATISTICS st ON a, b FROM t;` | admin / DDL only |
| G29 | logical replication | 6 | `CREATE PUBLICATION pub FOR TABLE t;` | admin / DDL only |
| G30 | foreign data wrappers | 2 | `CREATE FOREIGN TABLE ft (a int8) SERVER srv;` | admin / DDL only |
| G31 | `UNIQUE NULLS NOT DISTINCT` as a **column option** | 1 | `CREATE TABLE t (a int8 UNIQUE NULLS NOT DISTINCT);` | **on the query path** — the table-constraint and index spellings of the same clause parse, so only this one is a gap |
| G32 | `ALTER TABLE ... ALTER COLUMN`, every action beyond the four `sqlparser` reads (`SET`/`DROP NOT NULL`, `SET DEFAULT`, `SET DATA TYPE`, `ADD GENERATED`) | 11 | `ALTER TABLE t ALTER COLUMN a SET STATISTICS 100;` | admin / DDL only |
| G33 | moving a table between schemas | 1 | `ALTER TABLE t SET SCHEMA s;` | admin / DDL only |
| G34 | tablespaces | 2 | `ALTER TABLE t SET TABLESPACE ts;` | admin / DDL only |
| G35 | table access methods | 1 | `ALTER TABLE t SET ACCESS METHOD heap;` | admin / DDL only |
| G36 | clustering, and the OID legacy | 3 | `ALTER TABLE t CLUSTER ON i;` | admin / DDL only |
| G37 | resetting storage parameters | 2 | `ALTER TABLE t RESET (fillfactor);` | admin / DDL only |
| G38 | table inheritance | 2 | `ALTER TABLE t INHERIT u;` | admin / DDL only |
| G39 | a table of a composite type | 2 | `ALTER TABLE t OF sometype;` | admin / DDL only |

**Decision, for every row above:** refuse honestly, carry the gap, do not fork. The corpus keeps each statement, and
`every_known_gap_is_still_a_gap` fails the build the day an upstream release starts parsing one, so
a `sqlparser` upgrade is checked against the register automatically rather than by someone
remembering to look. The pre-parse rewrite is already taken where PostgreSQL itself
documents an equivalence: `TABLE t` and `ABORT` are rewritten and gone from the register. That
mechanism is deliberately limited to documented synonyms — rewriting `GROUP BY DISTINCT` into
something else would be inventing semantics, and inventing semantics is how a compatibility layer
starts returning wrong answers instead of honest refusals. Nothing here justifies a fork of
the parser, and nothing here is a reason to reconsider ADR 0014: a hand-written parser would have
its own gap register, and it would be longer.

## 10. Progress

- [x] 1 — plan, ADR 0014, dependency, crate skeleton, SQLSTATE table, error type, parse guard
- [x] 1b — the syntax corpus: 402 statements, oracle-verified, 95 gaps registered in §9
- [x] 1c — the feature recognizer: all 402 answered by a parse or an honest `0A000`, none a syntax
  error; `TABLE`/`ABORT` rewritten as the documented synonyms they are
- [x] 2 — pgwire, complete: framing, message codec, startup + `NegotiateProtocolVersion`,
  `ErrorResponse` fields, goldens, decoder fuzz (2a); the session state machine and the simple
  query protocol (2b); the extended protocol's statement and portal lifecycle (2c); the `tokio`
  listener, and a real `psql` connecting to it (2d)
- [x] 3 — row and tuple encodings: the six types' text formats against a real server (3a), the
  row value, primary key and index key encodings with goldens, proptests and a captured ordering
  fixture (3b)
- [x] 4 — catalog: the `'m'`-space records with a golden, the per-transaction version check,
  and a cache that cannot serve a definition from a snapshot's future
- [x] 5 — backend trait and fake (the executor's half of the unique-index composition is unit 6)
- [x] 6 — planner and executor — the lowering out of the parser's AST, the DDL
  executor over the catalog, `Execute` implemented and a real `psql` driving `CREATE`/`DROP`
  against it (6a); `INSERT` with its indexes and both halves of the unique-index ruling (6b); the
  planner, the pull-based executor and `EXPLAIN` (6c); `UPDATE` and `DELETE` with their index
  maintenance (6d); bound parameters in both wire formats and inferred `ParameterDescription` (6e).
  All three obligations from §10a are discharged.
- [x] 7 — the `.slt` harness: fourteen files, 295 directives, run by **two** runners (ours and
  the `sqllogictest` crate) and replayed against the real server to check that they record
  PostgreSQL's answers and not ours
- [x] 8 — `ALTER TABLE ADD COLUMN` (the continuation): the catalog cache fix it uncovered, the row
  and catalog format change that makes it rewrite nothing (ADR 0019), the statement itself, and the
  `ALTER TABLE` grammar sweep that closed eighteen contract-C1 violations nobody had looked for
- [x] 9 — tables with no `PRIMARY KEY`: an internal row id at column 0, leased a batch at a time so
  that two writers to one keyless table do not conflict on its counter; the six system columns
  refused by name rather than reported missing
- [x] 10 — one inner `JOIN`, as a nested loop whose inner side is a point read or a unique-index
  lookup when the condition allows one and a materialised table when it does not; two-table name
  resolution with PostgreSQL's three answers for a reference that does not resolve; `EXPLAIN` names
  the inner access path, and the planner picks which side drives the loop
- [x] 11 — **the real backend**: `StoreBackend` over `esker-client`'s `TxnClient`, the whole `.slt`
  corpus replayed against three real stores, and seven end-to-end tests through Percolator. The
  session runs the executor on a blocking task, which the plan always said and nothing had needed
  until the store stopped being in this process
- [x] 12 — **a real `psql` against a real cluster**: the acceptance target. The same script as the
  fake-backed smoke, driven by a client nobody here wrote, over a socket, against a node holding no
  data at all. It needed one fix outside this crate — `esker-proto`'s blocking transport refused
  the one thread a synchronous client belongs on (`spawn_blocking`), because `tokio` sets its
  handle there too

## 10a. Handoff — where a fresh lane picks up

Every unit is complete. 231 tests, `cargo fmt --check`, `clippy -D warnings`, `cargo deny` and the
dependency budget all green; no dependency was added.

**A real `psql` 18.6 runs SQL against this node, and against a real three-store cluster.**
`tests/psql_smoke.rs` drives the same session twice — once over the in-process fake and once over
`StoreBackend`, where every statement is a Percolator transaction spanning two regions on two
separate databases. It drives a whole session over
a socket — `CREATE TABLE`, `INSERT`, `SELECT ... ORDER BY`, an `UPDATE`, a `23505` naming its
constraint, a bound parameter through the extended protocol with `\bind`, and a `BEGIN`/`ROLLBACK`
that leaves the rows it deleted — against the executor and the store. It skips when `psql` is
absent.

### The whole surface, diffed against PostgreSQL 19

Each unit's script was run against this node and against a real PostgreSQL 19beta1 and the outputs
compared. Every row, every value's text, every SQLSTATE, message, `DETAIL` and `HINT` agrees. What
does not, in full:

| Divergence | Why | Where it is written down |
|---|---|---|
| `text` sorts by bytes | A locale-aware collation needs ICU or a platform C library; this project compiles neither. Equivalent to PostgreSQL's `COLLATE "C"`. | §6, `crate::row`, `tests/slt/select.slt` |
| A table with no `PRIMARY KEY` has no `ctid`, `xmin`, `xmax`, `cmin`, `cmax` or `tableoid` | The table itself works — it gets an internal row id (§11 unit 9) — but PostgreSQL's six system columns do not exist here and are `0A000` naming themselves. `ctid` is the one that matters: PostgreSQL's is a *physical* address that moves when a row is rewritten, ours is a *logical* identity that never moves, and answering one with the other would behave differently the first time somebody updated a row. | §11 unit 9, `tests/slt/no_primary_key.slt` |
| A decimal literal in an `int8` column is `0A000` | `numeric` rounds half away from zero and `float8` rounds half to even; there is no `numeric` here to be sure with. | §11 unit 6b, `tests/slt/types.slt` |
| `ADD COLUMN ... NOT NULL` **with no `DEFAULT`** is `0A000` even on an empty table | PostgreSQL accepts it there, because there is no row to violate it. Accepting it would mean scanning the table to find out, and the `ALTER` is supposed to touch no row; the restriction is stated rather than conditional. **Narrowed in phase 6e**: `NOT NULL DEFAULT <constant>` is now instant, here as there — the constant is the column's missing value and every row already stored reads it (`docs/plans/phase-6e.md` §5 unit 1). Only the bare form, which has no value to pad with, stays refused. | §11 unit 8, `tests/slt/unsupported.slt`, `tests/schema_change.rs` |
| `ADD COLUMN ... DEFAULT random()` is `0A000` | PostgreSQL accepts a volatile default and **rewrites the table** for it — measured: `atthasmissing` comes back false. This node stores one constant and rewrites nothing, so a value that differs per row is refused by name rather than stored once and wrong everywhere. The rewrite is what `docs/plans/phase-6e.md` unit 4 builds. | `tests/schema_change.rs` |
| `DEFAULT (1+1)` is `0A000` | PostgreSQL constant-folds it to `2` before storing. There is no folder here, and naming that is more honest than half-reading the expression. | `tests/schema_change.rs` |
| `CREATE INDEX CONCURRENTLY` needs a driver to step it | PostgreSQL's own backend finishes the build before the statement returns from the *user's* point of view (it returns early, and the index becomes valid on its own). Here the statement returns with the job recorded at `absent`, and the states advance as something calls `esker_schema_step`. The difference is that PD publishes the interval and does not drive the steps — a step is a catalog transaction and PD is byte-opaque (`docs/plans/phase-6e.md` §10). | `docs/plans/phase-6e.md` unit 5, `tests/slt/index.slt` |
| `esker_schema_jobs()` and `esker_schema_step()` exist here and are `42883` there | PostgreSQL watches a concurrent build through `pg_stat_progress_create_index`, a view. This node has no system catalogs, so the progress is a function — the same shape the time machine's verbs took, and for the same reason (`docs/plans/phase-6d.md` §1). | `tests/slt/index.slt` |
| Six value inputs are `0A000` | Hexadecimal floats, and PostgreSQL's datetime grammar outside ISO 8601. | `tests/value_parity.rs`'s `DIVERGENCES` |
| No `LINE n: ... ^` in an error | The `P` field needs the parser's spans carried through the lowering. `TODO(post-v1)`; §1 already excludes it for syntax errors. | this table |
| No `CONTEXT:` on a parameter's error | The `W` field. Same shape of gap as the caret. | this table |
| `SET esker.read_as_of`, `esker_checkpoint()`, `esker_diff()` **execute** here and do nothing there | The time machine (ADR 0021, `docs/plans/phase-6d.md`). PostgreSQL has no time travel, so it accepts the GUC and stores it, and answers `42883` for the functions. Divergence in the safe direction: every statement is one a real server parses, and the three spellings another database uses for this — `AS OF SYSTEM TIME`, `FOR SYSTEM_TIME AS OF`, `CHECKPOINT <name>` — stay refused, with a `HINT` naming what to write instead. | `docs/plans/phase-6d.md` §1, `tests/slt/time_machine.slt` |
| `SHOW esker.read_as_of` answers before it has ever been `SET` | PostgreSQL knows a custom GUC only once a session has set one, and answers `42704` until then. This node always knows it, because it is a parameter the node *has*. Answering is the safe direction — a discoverable feature rather than an error — and the empty string is what a real server returns after a `RESET`. | `docs/plans/phase-6d.md` §2 |
| `SET esker.read_as_of` reads `'-1h'` and not `'-1 hour'` | There is no `interval` type in this crate to be right with, and a half-implemented grammar would accept PostgreSQL's words and mean something else. The short form is CockroachDB's, which is what `AS OF SYSTEM TIME` takes. Refused rather than guessed at. | `crate::time_machine`, `tests/slt/time_machine.slt` |
| An exported snapshot id is `esker-<16 hex>`, not `%08X-%08X-%d` | Ours carries a `start_ts` where PostgreSQL's carries a transaction id, so it is prefixed rather than disguised. The contract on both sides is that the token is opaque. A checkpoint also **outlives its session**, where PostgreSQL's exported snapshot dies with the exporting transaction — a superset, and what makes it a checkpoint rather than a handle. | `docs/adr/0021-time-machine.md`, `crate::time_machine` |

Everything else PostgreSQL executes and this node does not comes back `0A000` naming the construct,
which is contract C2 and a deliverable rather than an omission — `tests/slt/unsupported.slt` holds
thirty of them and `tests/lowering.rs` holds twenty-nine more at the clause level.

### What is left

- ~~**The real `TxnClient`.**~~ **Wired** (§11 unit 11). `backend::StoreBackend` is the impl and
  nothing above it changed. The corpus and seven end-to-end tests run against three real stores
  over real sockets; `MemoryBackend` stays as what the unit tests run on, which is what keeps them
  fast and what makes "the same answers either way" a thing the corpus can assert. The three
  mismatches found by reading the client ahead of the signal are all closed and all verified
  against real stores — they are written out below because two of them were bugs and the third
  changed what the wiring unit had to write.
- **Acceptance against the `sqllogictest` crate** (§7.7), which wants rows separated by whitespace
  rather than tabs; a `sed` away, and worth doing when the real backend is under it.
- The `TODO(post-v1)`s named above, and the one in §5 about the round trip a unique index costs per
  row.

### Three seam mismatches, found by reading the client before the signal — all closed

Every one was found by reading rather than by a failure, which is the point of having read the
client before the signal fired: none of the three would have failed to compile, and two of them
produce wrong answers rather than errors.

`crates/esker-client/src/txn.rs` was read against `backend::Txn` ahead of the wiring unit, and
again after the store half opened. Every signature lines up — `get` and `scan` take `&self`, `put` and `delete` return nothing, `commit`
yields `Option<u64>`, `rollback` takes `self` — which is what shaping the trait against the real
one bought. Three things did not, and none of them would have failed to compile. Two are closed;
the third is open and is the client lane's.

**1. `scan(.., 0)` meant "everything" here and "a page" there. Fixed.** [`backend::Txn::scan`]'s
contract is that a `limit` of 0 is no limit; `Transaction::scan` runs it through
`Router::bounded_limit(limit, DEFAULT_SCAN_LIMIT)`, which turns 0 into 1024 and then caps it at
`max_scan_limit`. Four call sites in `exec/ddl.rs` asked for a whole range that way, and the last
of them returns *wrong answers* rather than leaving rubbish behind:

| Call site | What silently truncating did |
|---|---|
| `drop_table`, the row range | a table over a page kept its rows after `DROP TABLE` |
| `drop_table`, each index range | and its index entries |
| `drop_index` | left entries behind |
| `backfill` | **built an index missing every row past the first page** — so `CREATE INDEX` succeeded and afterwards a query *using* the index returned fewer rows than the same query without it |

All four now go through `exec::for_each_page`. Reading the surrounding code for the fix turned up a
**fifth** instance of the same defect on the read path, which the first version of this note had
called safe: `exec::cursor` paged correctly but stopped on a **short** chunk, and a short chunk is
not evidence that a range is finished — the store may cap a scan below what was asked. It was safe
only because `max_scan_limit` defaults to 16,384 and `SCAN_CHUNK` is 1,024; an operator lowering
that option below 1,024 would have made every `SELECT` return a prefix of its rows and say nothing.
Both now stop on an **empty** read, which costs one round trip per walk and depends on no number
anybody can configure.

`MemoryBackend::with_scan_limit` is what makes any of this testable: the fake used to answer 0 as
"everything", so code that did not page looked correct against it. Set to two, it behaves like the
real client and `tests/ddl.rs`'s `paging` module has one test per call site — each verified to fail
without the fix.

**2. A write conflict had no way to arrive as one. Closed by the phase-5 lane, better than asked.**
`explain_conflict` (`exec/mod.rs`) turns a lost race on a unique index key into the `23505` the user
actually caused, and it keys off `SqlError::SerializationFailure`; at the time of reading,
`esker_client::Error` had no conflict variant and neither did `ProtoError`, so a conflict would have
reached this layer as an undifferentiated store error and `explain_conflict` would never have fired.

It now arrives as `esker_client::Error::TxnConflict { start_ts, commit_ts, key: Option<Bytes> }`,
and the contract is `docs/txn-spec.md` §6.1: `Prewrite` answers per key, so the refusal **names the
key that lost**, and `None` means the refusing method does not answer per key rather than that no
key lost. (It is a struct variant with named fields, not a positional one — worth knowing before
writing the match.)

That is better than a bare error code, and it makes the wiring unit *smaller* rather than larger:

* the rule is `TxnConflict { key: Some(k), .. }` → look `k` up in `Written::unique_keys`; a hit is
  `23505` naming that constraint, a miss is `40001` and still retryable;
* which means **`explain_conflict`'s second look goes away** in the common case. It exists today
  because the transaction that could have said which key lost was gone by the time the error
  arrived, so it opens a fresh transaction and probes every unique key it wrote. The key is now in
  the error. Keep the probe only for `key: None`, and take the round trip out of the path a
  duplicate insert takes.

**3. A scan stopped at a region boundary, and nothing said so. Fixed by the client lane.**
`Transaction::scan` walks region by region now, and `tests/real_backend.rs` reads all 1500 rows of
a table and drops every one of them against real stores. What follows is the note as it stood when
it was reported, because the reasoning is what the wiring unit was built on.


Found while re-reading the store half after it opened. `esker_client::Transaction::scan` sends one
`TxnKvReq::Scan` and merges its buffer into the answer; the region it goes to is the one holding
**`start`** (`wire.rs`'s routing key, which already carries a `TODO(phase-4)` about reverse scans
and more than one region). The store answers out of its own engine
(`esker-store/src/txnkv.rs::user_keys_in`), so a range spanning two regions comes back holding only
the first one's keys.

That defeats **any** paging rule this crate can implement on its own, including the one it now has.
`for_each_page` resumes from `successor(last)` and stops on an empty read; once the first region's
keys are exhausted, `successor(last)` still routes to that same region, which answers empty, and
the walk stops with later regions unread. So the fix above is correct for one region and correct
for the fake, and a table that has *split* would silently truncate again — a `DROP TABLE` leaving
rows, and an index built over the first region only.

The fix belongs in `esker-client`: `Transaction::scan` iterating regions in order, the way a
region-aware client is expected to. `esker-sql` must not learn where boundaries are — that is the
routing layer's knowledge and this crate is above it. Flagged before the wiring signal rather than
after, because the wiring unit will otherwise appear to work: nothing truncates until a table grows
past one region.

One correction to the first version of this note, since it was read as a claim about the client's
internals: it said a duplicate would surface as "an internal-sounding failure", meaning how the
undifferentiated error would read *to a user*. No `Error::Internal` was observed on a conflict path
and none was being claimed.

### Handoff to the ADR-implementation era

Three designs are decided and unbuilt (§12): online schema change
([0020](../adr/0020-online-schema-change.md)), the time machine
([0021](../adr/0021-time-machine.md)) and the columnar learner
([0022](../adr/0022-columnar-learner-replica.md)). What follows is what this crate already has for
them, what it does not, and the two or three places where a plausible-looking assumption is wrong.
Every pointer below was checked against the code at the time of writing rather than remembered.

**The `AS OF` seam is one constructor, and it really is one.** `TxnClient::begin`
(`crates/esker-client/src/txn.rs`) takes a `start_ts` from the oracle and hands it to a
`Transaction` literal; `begin_at(start_ts)` is that literal with the number passed in. Nothing else
in that crate looks at where it came from — locks, resolution, read-your-writes and commit are all
written against `self.start_ts` as a value. Above it, `Backend::begin` is the only place this crate
opens a transaction, so a historical read is a second method there and an `Option<u64>` reaching
`StoreBackend`. The three refusals ADR 0021 §1 names (read-only, not below the safepoint, not in
the future) belong at that seam and not lower: `esker-client` has no opinion about them and should
not grow one.

**`TableDef::schema_version` exists and is deliberately not load-bearing.** It is `1` from
`CREATE TABLE` and one more per `ALTER TABLE` *statement* — not per column added, because a
statement's columns become visible together. Nothing reads it to decode a row: a row carries its
own column count (ADR 0019), which is what makes it safe to bump the version without touching data.
It is there for ADR 0020 to hang per-column state from, and the field's own doc comment says so, so
that nobody later "fixes" an apparently unused field.

**What the catalog can express today, and what it cannot.** `ColumnDef` is `{ name, ty, not_null }`
and `IndexDef` is `{ id, name, unique, columns }`. Neither has a *state*, so the catalog can say
that a column or an index **exists** and nothing about whether it is delete-only, write-only or
public. ADR 0020's four states are therefore a field on each plus the schema version it entered, and
that is `CATALOG_FORMAT_VERSION` 2 → 3 with the table-record golden re-captured — a format change
with a golden, so it goes to the project owner first (`CLAUDE.md`). Budget it as part of the work
rather than discovering it.

Two things that *are* already there and are easy to miss:

* **The retention records the collector reads** — `'m' ++ "sql" ++ 'd'` for the cluster default and
  `'m' ++ "sql" ++ 'r' ++ tenant ++ table_id` for an override, with `set_table_retention`,
  `clear_table_retention`, `table_retention` and `default_retention` beside them, golden-tested down
  to the key bytes. There is no SQL surface yet; the record format was built first because the GC
  filter consumes it and a format cannot wait for the feature on top of it.
* **`allocate_row_ids` is the template for any counter a hot path bumps.** It is leased a batch at a
  time in a transaction of the session's own, because a counter bumped inside the statement's
  transaction is a key every writer conflicts on. ADR 0020's backfill cursor has exactly this shape
  and should be written the same way — and it inherits the same consequence, which is gaps, which is
  what a PostgreSQL sequence does anyway.

**Three assumptions that look safe and are not.**

1. **A transaction that has written the catalog must not fill the shared cache.** It reads its own
   uncommitted DDL, and catalog versions are *reused* after a rollback, so an abandoned entry is
   waiting for the next DDL that lands on the same number. `Catalog::view_uncached` is the rule and
   `Executor::catalog_written` is what selects it. An online schema change writes the catalog
   repeatedly, per state, and every one of those transactions is in this category.
2. **A whole range cannot be asked for in one call.** `Txn::scan`'s `limit` of 0 means "no limit" to
   this crate's trait and a *page* to the real client. Everything that walks a range goes through
   `exec::for_each_page`, which resumes from the last key and stops on an **empty** read, not a
   short one — a short page is not evidence of anything. A backfill written as one big scan will
   silently index a prefix.
3. **`DROP COLUMN` is not a catalog change.** The row format carries a column *count*, not column
   identity, so dropping the second of three columns leaves rows whose count is 3 and whose second
   value belongs to a column that no longer exists. It needs row format version 3 with a column id
   per value, which ADR 0019 names. `ADD COLUMN` was cheap for a reason that does not generalise.

**Where a conflict's meaning is decided.** `SqlError::SerializationFailure` carries the key that
lost (`docs/txn-spec.md` §6.1) and `Executor::explain_conflict` turns it into the `23505` a user
actually caused. Anything that writes on a user's behalf — a backfill, a `FLASHBACK`'s compensating
writes — inherits that translation only if it records what it wrote in `Written::unique_keys`.

### Containment is one module, and now a test

ADR 0014 originally said `sqlparser` is named in one **file**, and unit 6's lowering — the part that
must touch the AST, and so must live inside the boundary — had taken `parse.rs` to 1851 lines,
past `CLAUDE.md`'s ~800-line guideline with no way to satisfy both rules at once.

Ruled by the project owner: the ADR is amended to one **module**. The intent was always that no
`sqlparser` type leaks past the boundary, and a module boundary carries that exactly as well. It is
now `src/parse/mod.rs` (the depth guard, the lexical scan, the classifier, the recognizer — 807
lines of code) and `src/parse/lower.rs` (the AST into `crate::plan` — 705).

`tests/containment.rs` is the other half of the ruling, and the more important half: the rule was a
sentence in an ADR and a reviewer's attention, and its failure mode is that nothing goes wrong until
the day you try to replace the dependency. The test reads the crate's own source and fails if the
name appears outside `src/parse/`. It is textual rather than type-based on purpose — a `use` is only
one of the ways to name a crate — with two exceptions that are prose about the rule rather than a
way around it: a citation of the ADR's own filename, and the bare backticked name in a doc comment.
The second is narrow enough that a doc *link* to a type still fails, which is checked by breaking it
three ways and watching it catch all three.

### Three things worth knowing before touching any of it

- **Capture first.** It has now found **twenty-eight** defects across this phase and reading the
  specification has found none. §9 has the container recipe. Two of those twenty-eight were defects
  in the *capture itself* — a `::text` cast that is not the output function, and a `printf %b` that
  decoded `\101` — and both would have pinned a wrong answer into a golden file, so the method
  deserves the same suspicion as the code.
- **`sqlparser` types stop at `parse.rs`** (ADR 0014), and the lowering there is the last place one
  is named. Its rule is *reject, do not ignore*: an AST field nothing reads is a clause the user
  wrote and the server did not honour.
- **Two seams, still easily confused.** `pgwire::session::Execute` is how the protocol reaches the
  executor; `backend::Backend`/`Txn` is how the executor reaches storage.

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

**Unit 3.** The formats are in `src/value/` and `src/row.rs` rather than the single `row.rs` §5
sketched. The split is along a real seam: `value/` is the six types and the text a client reads
them as — the compatibility surface — and `row.rs` is the two encodings that put them in the key
space. Each is well under the file-size rule and each has its own reason to change.

The method was the same as unit 2's and it paid the same way. **Ten more facts came off a running
PostgreSQL 19beta1 that reading would not have produced**, and two of them were defects in the
*capture* rather than in the code, which is worth recording because both would have pinned a wrong
answer into a golden file:

- **A boolean on the wire is `t`, not `true`.** The first capture asked for `value::text` and got
  `true`, because PostgreSQL has a real cast function for boolean that its *output function* — the
  one that fills a `DataRow` — does not go through. Every other one of the six agrees between the
  two, so the artifact would have shown up only in the one place it mattered. The capture now takes
  the raw field.
- **`printf %b` decodes `\101` as an octal byte**, so the first `bytea` capture recorded inputs the
  server had never been given, and PostgreSQL's answers to *different* inputs. The generator now
  unescapes exactly the three sequences the corpus file documents. Both the escape-format rules
  below were wrong before this was found.
- **`float8` switches to exponent notation outside `10^-4 .. 10^14`**, and the boundary does not
  depend on how many significant digits the value has: `999999999999999` prints in full and `1e+15`
  does not, `0.0001` is plain and `1e-05` is not. It is not `%g`'s rule, which is what a reading
  would have given. The digits themselves are Rust's own shortest round-trip formatting, which is
  the same thing PostgreSQL's Ryu produces, so nothing here reimplements Ryu.
- **`int8` input takes `1_000`, `0x1f`, `0o17` and `0b101`** — PostgreSQL 16 gave the input function
  the non-decimal literals the lexer had.
- **`boolean` input takes any unambiguous prefix**: `tr` and `ye` are true, `of` is false, and `o`
  is an error because it could be `on` or `off`.
- **`bytea`'s hexadecimal errors are `22023`**, not the `22P02` its neighbours in the same input
  function use, and its escape-format error quotes nothing back. Octal escapes need exactly three
  digits: `\101` is a byte and `\1` is an error.
- **The datetime type has its own condition, `22007`**, not `22P02`; a field out of range is
  `22008`; a zone displacement past `±15:59` is `22009`. Three codes where one was expected.
- **`24:00:00` and `23:59:60` are both legal ways to write the next midnight**, and `24:00:01` is
  not — so the check belongs on the whole time and not on the hour.
- **Fractional seconds round rather than truncate**, and the rounding carries: `.9999999` becomes
  the next second.
- **An era suffix prints after the offset**: `0001-01-01 00:00:00+00 BC`.
- **The range ends are Julian day 0 and 294276-12-31**, both confirmed by accepting one value and
  refusing the next microsecond past it.

`tests/corpus/pg19_values.txt` is 184 of those answers and `tests/value_parity.rs` replays every one
in both directions — the same characters for a value, the same SQLSTATE *and* message for a refusal.
It passed on the first run after the two capture defects were fixed, which is evidence about the
capture and not about the code: the corpus is the only independent party.

Three decisions the plan did not have, each recorded because a later reader could reasonably reverse
it:

- **Text sorts by bytes.** §6 now says so. The database captured against sorts `a` before `B` under
  `en_US.utf8`; a byte-ordered key space sorts `B` first, which is PostgreSQL's `COLLATE "C"`. A
  locale-aware collation needs ICU or a platform C library and this project compiles neither, so the
  choice was between declaring byte ordering and pretending. The ordering fixture is captured with
  `COLLATE "C"` so the divergence is a line in a file rather than a surprise in a query.
- **A value PostgreSQL reads and we do not is `0A000`, naming the construct** — contract C2 applied
  one level down, to a value rather than a statement. It covers hexadecimal float input and the
  parts of PostgreSQL's datetime grammar outside ISO 8601 (`DateStyle`-dependent dates, named time
  zones, `now`/`epoch`). The alternative was answering `22007 invalid input syntax` about input that
  is in fact valid, which is the exact lie C1 forbids one level up. `DecodeDateTime` is a large
  parser with a time zone database behind it, and implementing *part* of it is how a server returns
  a confidently wrong instant. The six divergences are listed in `tests/value_parity.rs` and held
  from both sides.
- **A unique index entry containing a NULL keeps its primary key suffix.** The unique-index design
  in §5 turns a duplicate into a collision on one key by leaving the suffix off — but PostgreSQL
  admits any number of NULLs in a `UNIQUE` column, confirmed against the server, so those entries
  would have collided with each other and the second NULL row would have been reported as a
  duplicate. `row::unique_index_key_is_unique_by_value` is the predicate, and unit 6 consumes it.

**Unit 4.** The catalog is `src/catalog/` — the types, the cache and the DDL writes in `mod.rs`,
the keys and the record bytes in `record.rs`. Four things came out differently from §6's sketch,
three of them from asking the server.

- **A table's indexes live inside its record**, not under keys of their own. The sketch implied one
  record per relation. The argument for embedding is not brevity: a cache entry is a whole table,
  and if the index list were a separate key a cached table could be current while its index list
  was stale — the one kind of staleness that *corrupts* rather than merely returning old data,
  because a row would be written with no entry in an index that exists. Embedding makes the unit of
  caching and the unit of consistency the same object.
- **Tables and indexes share one namespace.** `CREATE INDEX dup ON t (a)` where a table `dup`
  exists answers `42P07 relation "dup" already exists`. This started as two name maps and is one.
- **Identifiers fold only ASCII `A`–`Z`.** A UTF-8 server leaves `Ébc` alone; `str::to_lowercase`
  would have quietly renamed every non-ASCII identifier. Truncation is at 63 **bytes** and is a
  `42622` *notice*, not an error — a 70-character name became a 63-character table.
- **A transaction older than the cache reads through it.** The version check as sketched discards a
  cache that is *behind*; it also has to refuse to serve one that is *ahead*, or a transaction
  would see a definition from its own future.
  `a_cache_warmed_by_a_newer_transaction_does_not_leak_into_an_older_one` is that test.

Concurrent DDL needs nothing new: every DDL statement writes `catalog_version`, so two of them
conflict and one is told to retry, and two `CREATE TABLE`s of one name conflict on the name key by
the same read-then-write composition `backend.rs` documents for a unique index.

**Unit 6a.** The lowering, and the DDL half of the executor. `crate::plan` holds statements in
types this crate owns, `parse.rs` produces them (it stays the only file naming a `sqlparser` type),
and `exec::Executor` is the `Execute` the session has been calling into a placeholder since unit 2.

The rule the lowering is built on is **reject, do not ignore**. A lowered statement holds far less
than the tree it came from, and an unread field is a clause the user wrote and the server did not
honour — `CREATE TEMPORARY TABLE t` executed as a permanent table is a failure nothing reports.
So every clause that changes what a statement means is named and refused with `0A000`, and
`tests/lowering.rs` is written from that side: 28 statements, each carrying a clause phase 6a does
not honour, each asserting its own name comes back.

Running the same DDL script against a real PostgreSQL 19 and against this node found **six**
differences, all now closed:

- **`sqlparser` cannot parse `UNIQUE NULLS NOT DISTINCT` as a column option**, though it parses the
  table-constraint and index spellings of the same clause. PostgreSQL 19 accepts all three, so this
  was a live contract C1 violation — a `42601` about valid SQL — found by the lowering tests rather
  than by the corpus, which had never contained the statement. It is now gap **G31** in §9, the
  recognizer names it, and the corpus holds both spellings.
- **`DROP TABLE` words a missing table differently from a query**: `table "x" does not exist`
  against a query's `relation "x" does not exist`. Two conditions, not one.
- **A name that exists and is the wrong kind is `42809`, not `42P01`** — `"t_b_key" is not a table`
  — and `IF EXISTS` does not excuse it. Telling a user their index does not exist would send them
  looking for the wrong bug.
- **`CREATE INDEX ON t (a)` twice is not an error.** PostgreSQL disambiguates a name it derived
  itself: three of them gave `t_a_idx`, `t_a_idx1`, `t_a_idx2`. A name the *user* chose still
  collides. This had been written down as a `TODO(post-v1)` on the assumption it was cosmetic; the
  capture showed it changes whether the statement succeeds.
- **A key clause naming a missing column says `column "b" named in key does not exist`**, three
  words longer than the ordinary message and pointing at the constraint rather than the column list.
- **PostgreSQL sends a `HINT`** with the `42809`s — "Use DROP INDEX to remove an index." — and
  `SqlError` had no way to carry one. It does now.

Two decisions and one remaining divergence:

- **A table must have a primary key**, because the row key *is* the primary key (`crate::row`) and
  a table without one has no key space to live in. PostgreSQL allows it, so the answer is contract
  C2's `0A000` naming it rather than a syntax error or a table that quietly cannot be written to.
  `TODO(post-v1)`: an implicit row id from a per-table sequence, which is how this is usually
  closed and which needs a sequence phase 6a does not have.
- **The primary key constraint's name is a relation name.** `<table>_pkey` is reserved in the same
  namespace as tables and indexes even though there is no index behind it, because PostgreSQL
  answers `42P07` to `CREATE INDEX t_pkey ON t (a)` and because it is the name a `23505` on the key
  will quote back. That needed a field in the catalog record, which is a format change to something
  with a golden test — asked and approved, with the version byte left at 1 since nothing has ever
  written those bytes to disk. A `CONSTRAINT my_pk PRIMARY KEY (a)` keeps `my_pk`; without the
  field the name would have had to be derived, and the user's own name silently dropped.
- **The `LINE n: ... ^` caret is the one thing still missing** from an otherwise byte-identical
  diff against the real server. It is the `P` field, and filling it needs the parser's spans
  carried through the lowering. `TODO(post-v1)`, and consistent with §1's existing exclusion of
  caret positions for syntax errors.

**Unit 6b.** `INSERT`, and with it the executor's half of the unique-index ruling — the one §5 and
§10a both name. `a_concurrent_duplicate_is_reported_as_a_duplicate` is `backend.rs`'s
`a_concurrent_duplicate_loses_at_commit` run against the real executor: two sessions, two
transactions, both reading the index key as absent, both writing it, one committing. The loser is
told `23505` naming the constraint, because what the *user* did was insert a duplicate and telling
them `40001` would be telling them to retry something that can never succeed.

Two things about that translation came out of writing it rather than out of the plan:

- **The recording has to span the block, not the statement.** The first version recorded unique
  keys per statement, which meant a client using `BEGIN` — the case the whole ruling is about —
  got the untranslated `40001`. `Written` now accumulates across the open transaction and
  `Execute::commit` runs the same translation the autocommit path does. The test caught it.
- **The message needs the values, and after a failed commit there is no transaction left to ask.**
  So the `DETAIL` is rendered at write time, while the row is still to hand, and carried alongside
  the key. On a `40001` the executor opens a fresh transaction and looks: the keys that are now
  present are the ones it collided with, and a key nobody took means the conflict really was an
  ordinary race and stays retryable.

Diffing the `INSERT` surface against a real PostgreSQL 19 closed four more differences:

- **`DETAIL` is not decoration.** `Key (id)=(1) already exists.` is the part of a `23505` a user
  reads to find out *what* collided, and `SqlError` had no way to carry one. PostgreSQL renders it
  with **no quoting at all** — a text value containing `, y)` really does come back as
  `Key (a, b)=(1, x, y))`, unbalanced parentheses included — and that is copied exactly, because a
  client parsing the field was written against that.
- **An integer literal too large for `bigint` is three words**, `bigint out of range`, where the
  same overflow reached through the type's input function keeps its longer message. Two paths, two
  messages.
- **`INSERT has more expressions than target columns` is a `42601`** and PostgreSQL says exactly
  that, with no `syntax error:` in front of it.
- **A `NOT NULL` violation names the relation**, and so does a column the table does not have —
  `column "nope" of relation "t" does not exist` — where the same conditions elsewhere use the
  shorter form.

Assignment casts are the value layer doing its second job. A quoted literal is PostgreSQL's
`unknown` type, so assigning one is `Datum::from_text`, which the value corpus already checks for
all six types in both directions; that one function replaces six rules. The rest were measured and
two of them are not what a reading would give: **`true` in a `text` column stores `true`**, the
cast's word and not the `t` the output function writes, and **`1.5` stores the digits as written**,
because PostgreSQL types a decimal literal `numeric` and `numeric`'s text is its own digits.

One conversion is refused on purpose. A decimal literal in an `int8` column is `0A000` naming it,
because `numeric` rounds half away from zero (`2.5` becomes `3`) and a `float8` rounds half to even
(`2.5` becomes `2`), and there is no `numeric` here to be sure with. A silently wrong number is
worse than a named gap.

**Unit 6c.** `SELECT`: a rule-based planner, a pull-based executor and `EXPLAIN`. Diffing the whole
surface against a real PostgreSQL 19 — every row, every value's text, every error — leaves exactly
two differences, and both are already written down: the `text` collation (§6) and the `LINE n: ^`
caret (unit 6a's note).

The planner has three rules and `EXPLAIN` prints which one fired: a `WHERE` that pins the whole
primary key is a point read, one that pins a unique index's whole key is a lookup, and one that
bounds a single-column primary key narrows the scanned range. Everything else is a scan with a
filter, which is always correct. Two things about the rules are load-bearing:

- **Only conjunctions count.** A constant under an `OR` is not required by the query, and reading
  only its range would silently lose the rows the other branch matches.
- **`narrowing_a_range_never_loses_a_row`** checks every combination of bound against the rows a
  plain scan returns, because a pushdown that is subtly too tight returns fewer rows and nothing
  reports it.

The executor is a pull pipeline, so `LIMIT 10` over a large table reads one chunk of keys and
stops. Two nodes cannot be lazy and both say so: `Sort` drains its input by definition (its input's
last row can be its output's first) and is bounded at a million rows with `53400` rather than an
unbounded allocation, and the scan reads in chunks because `Txn::scan` returns a `Vec` and asking
for a whole table would be a whole table in memory.

The tests found three real defects that reading the code would not have:

- **`Sort` was above `Project`**, so `ORDER BY` read positions out of the *projected* row and
  ordered by nothing in particular. It belongs below, which also makes `SELECT n FROM t ORDER BY
  id` work — ordinary SQL that a sort above the projection cannot express at all. An `ORDER BY`
  naming an output alias is substituted first, which is the other half of what PostgreSQL allows.
- **`CREATE INDEX` did not build the index.** An index that exists and is empty is worse than no
  index: the planner picks it and it answers every lookup with no rows. It now backfills from the
  table's rows in the same transaction, and a `UNIQUE` index built over rows that already violate
  it fails with the `23505` an `INSERT` would have raised.
- **Comparison is stricter than assignment**, and using one rule for both is a wrong answer rather
  than a missing feature. PostgreSQL stores `42` in a `text` column happily — an assignment cast —
  and answers `WHERE txt = 42` with `operator does not exist: text = integer`. With the assignment
  rule, that error became a silent `false`.

Three more parity details came off the server:

- **`numeric` has no signed zero.** `-0.0` in a `double precision` column stores `0`, because the
  literal is a `numeric` on the way in; `'-0'` — a quoted literal, read by `float8`'s own input
  function — keeps the sign. Two paths, two answers, and only a capture would ever have shown it.
- **A negative `LIMIT` and a negative `OFFSET` carry different codes**, `2201W` and `2201X`, so a
  client is told which clause it got wrong.
- **`LIMIT NULL` means no limit**, which is a rule and not an oversight.

`EXPLAIN` output is deliberately not PostgreSQL-shaped: there is no cost model here, so there are
no costs. What it does print is the access path and the filter, which is the part a user changes
their schema over.

**Unit 6d.** `UPDATE` and `DELETE`. Both reuse the planner — a pinned key is a point read here too
— and the hard part of neither is the row. It is everything that points at it: an index entry left
behind after the value moved is not a slow query, it is a wrong answer, because the planner will
follow it and return a row that no longer has that value.

Two things in the implementation are there for a reason worth recording:

- **The matching rows are read before any of them is written.** The scan and the writes share a
  transaction, and the buffer is merged into a scan, so a row whose primary key an `UPDATE` *moves*
  could be met again further along and moved a second time. That is the Halloween problem, and
  materialising first is the cheap way out.
  `an_update_that_moves_rows_forward_touches_each_one_once` is the test.
- **`SET` expressions are evaluated against the row as it was**, so `SET a = b, b = a` swaps them
  rather than assigning `a` twice, and `SET e = e` on a unique column is not a duplicate of itself.

Assignment targets are resolved before the first row is read, so `SET nope = 1` fails without
having rewritten anything. The same script against a real PostgreSQL 19 differs only in the
`LINE/^` caret.

**Unit 6e.** Bound parameters, which closes the third obligation from §10a: `ParameterDescription`
now reports what a statement *needs* rather than echoing back what the client declared, which told
a client that declared nothing exactly nothing.

A `Bind` carries values and format codes and no types at all, so the type comes from where the
parameter appears — the column it is inserted into, the column it is compared against, the column
it is assigned to. A type the client did declare wins, because it is the one that knows what bytes
it is sending. Two things were measured rather than assumed:

- **The fallback is `text`.** `PREPARE p AS SELECT $1` on a real PostgreSQL 19 reports `{text}`,
  not an error and not "unknown".
- **The binary formats**, captured with `COPY ... TO STDOUT (FORMAT binary)`, which goes through
  the same `typsend` functions the protocol does. All six are big-endian — the one place in this
  project that is — and one of them settled a bet from unit 3: a `timestamptz` on the wire really
  is microseconds from 2000-01-01 with `i64::MAX` for `infinity`, so a value goes out exactly as it
  is stored, with no arithmetic at all.

A real `psql` 18.6 now drives the extended protocol against this node with `\bind`, and the same
script against PostgreSQL 19 returns the same rows and the same errors. The only field still
missing is the `CONTEXT: unnamed portal parameter $1 = ...` a real server adds to a parameter's
own error — noted with the `LINE/^` caret as the remaining message-field gap.

**Unit 7.** The `.slt` harness, and one decision about it that turned out to matter more than the
harness itself.

Eight files, one per statement class, in `tests/slt/`. The format is the familiar one — `statement
ok`, `statement error <SQLSTATE>`, `query <types>` and its rows — with two additions that earn
their place: `statement ok` may name the command tag it expects, and `query` checks the letters
against the *types the server reported*, so a query that silently changed shape fails here rather
than wherever reads it next. Rows are tab-separated rather than the `sqllogictest` crate's
whitespace, which cannot express a value with a space in it; `types.slt` has one.

**The corpus was then replayed against a real PostgreSQL 19beta1** — every directive, compared with
what that server answered. A corpus that only records our own behaviour proves nothing, and this is
the same capture-first method the rest of the phase used, turned on the tests. Two rounds of it
found nothing in the executor and three defects in the *verification*, which is worth recording
because each would have made the check look like it passed while checking almost nothing: psql
prints NULL as an empty string, so every NULL looked like a mismatch; running each statement in its
own psql invocation ends the transaction between them, which is exactly what `transactions.slt`
needs not to happen; and replaying a file's statements against a database the last replay already
mutated applies every `UPDATE` twice — and a swap applied twice is a swap that never happened.

The result: **every directive agrees**, with two kinds of exception, both marked in the files.
`unsupported.slt` in its entirety is contract C2 — thirty statements PostgreSQL runs and this node
answers `0A000` for, which is the deliverable §3 describes rather than an omission. And three lines
carry a `DIVERGES` comment with the reason beside them: a table with no primary key, a decimal
literal in an integer column, and `text` ordering by bytes. All three are the divergences already
argued for in §6 and §11; what is new is that they are now the *complete* list, measured rather
than believed.

**Unit 7, second pass — the `sqllogictest` crate, and what it caught.**

`CLAUDE.md`'s dev allowlist reserves `sqllogictest` for phase 6; it is now a dev-dependency and
`tests/sqllogictest.rs` hands it the same `tests/slt/*.slt` our own runner reads. The point is the
one the `psql` smoke test makes: **the other end was written by someone else.** A harness and its
files are written together, and a harness that quietly means something slightly different by
`statement error` will agree with its own files forever.

Making that possible meant giving up a dialect. The corpus was ours — `statement ok INSERT 0 2`,
`statement error 23505: message` — and is now the crate's exactly: `statement ok`, `statement count
N`, `statement error (23505)`, which the crate resolves through a `error_sql_state` hook on the
driver. One thing the format has no word for rides in a comment: `# tag:` names the command tag a
statement must report, because `INSERT 0 3` and `UPDATE 3` are different answers to a client and
`statement count 3` is the only word the format has for both. The crate's parser skips it.

It disagreed on the first run, and about the *format* rather than the server, which is the more
useful kind of disagreement to find. The crate's default validator normalises a result by splitting
on whitespace and rejoining with single spaces, and that **cannot represent an empty string**: a row
of `9223372036854775807`, `''`, `f` collapses to two visible columns and a run of spaces, and no
expected line can be written that means "the second column is empty". `types.slt` stores an empty
`text` on purpose, because an empty string that is not a NULL is precisely what the row encoding is
built around. The runner is given a tab-joining validator — the crate's own extension point — and
the limitation is recorded as the dialect's rather than worked around by deleting the values that
expose it.

A second test keeps the two honest about each other: both runners must count the same number of
records in every file, so one of them cannot start skipping what the other checks.

**Coverage.** Two files added, 228 directives across ten. `nulls.slt` is three-valued logic on its
own — the thing most likely to be subtly wrong and hardest to notice a wrong answer in — and
`access_paths.slt` is the planner's three rules, each checked for the *rows* it returns as well as
the plan it produces, because a pushdown that is subtly too tight returns fewer rows and nothing
reports it. Both were replayed against a real PostgreSQL 19 and agree with it completely.

Writing `access_paths.slt` also found a real defect in a user-facing surface: `EXPLAIN` was printing
`Condition: (column 0 = Integer(3))` — a position instead of the name the user typed, and a Rust
`Debug` rendering of the literal. It now prints `Condition: (id = 3)`, with strings quoted, because
`WHERE e = c` and `WHERE e = 'c'` mean different things and a plan that cannot tell them apart is a
plan that cannot be checked against the query.

The whole-corpus replay now accounts for every disagreement exactly: **30** in `unsupported.slt`
(contract C2 by design), **5** `EXPLAIN` blocks (no cost model here, marked `DIVERGES` in the file),
and **3** marked divergences. Nothing else.

**Unit 7, third pass — the edge semantics the container had already answered.**

Four more files, taking the corpus to 295 directives across fourteen. Nothing new in the server;
these are cases the pg19 captures had settled during units 3 and 6 and that no test had written
down.

- **`ordering.slt`** — where NULL sits in a *sort*, which is a different rule from what it does to a
  *predicate* and only one of the two is three-valued. In a `WHERE`, NULL is neither true nor false
  and the row is dropped; in an `ORDER BY`, NULL is the largest value there is and the row is kept.
  All six types, both directions, both overrides, and `infinity` sorting above every finite instant
  and still below NULL — a type that confused a value with a missing one would put them adjacent.
- **`empty_vs_null.slt`** — the distinction the row encoding is built around. A predicate separates
  them one way (`= ''` finds one, `IS NULL` the other, `<> ''` finds neither) and a unique index the
  other (two empty strings collide, two NULLs do not). Both print as nothing in most clients, which
  is why collapsing them would be a wrong answer nothing reports.
- **`limits.slt`** — zero, absent, and past the end, which is where an off-by-one hides. Two of the
  cases are about ordering rather than arithmetic: the window is taken after the sort and after the
  filter, and the first would look right under a stable storage order.
- **`rowsort_check.slt`** — the one directive where the two runners could silently disagree.

`rowsort` is now supported by our harness as well as the crate's, which is what lets the corpus say
**the order is not part of the answer**. A `SELECT` with no `ORDER BY`, or one whose `ORDER BY`
leaves ties, has no order to promise — PostgreSQL does not guarantee one and neither does this node
— and writing such a result down as though it did pins an accident that the first change to the scan
or the sort would break. Ours sorts column vectors exactly as the crate does, and
`rowsort_check.slt` is there because if the two ever sorted differently, every `rowsort` result in
the corpus would be checked against the wrong thing by one of them.

The replay against PostgreSQL 19 adds exactly one disagreement across all four files, and it is the
`A`/`a` collation pair `select.slt` already argues. Everything else agrees.

Three limitations of the `.slt` format have now been found and all three are recorded where they
bite: the whitespace join cannot express an empty column (hence the tab-joining validator); a NULL
renders as the letters `NULL` and so sorts where an `N` would under `rowsort`; and a single-column
row holding `''` renders as a blank line, which is what ends an expected block, so a possibly-empty
column goes first and never last. None of the three is the server's.

**Unit 2a.** The goldens are recorded, not written. A proxy between `psql` 18.6 and the
PostgreSQL 19beta1 container logged both directions of five real sessions, and
`tests/golden/pgwire.hex` is 26 byte strings taken straight out of that log. Three details came
back different from what the specification alone would have suggested, and each is now pinned by a
test:

- **`NegotiateProtocolVersion` is followed by the ordinary startup sequence**, not sent instead of
  it. Asking PostgreSQL 19 for minor version 9 produced a real one to check against, and asking with
  an unknown `_pq_.` parameter produced the variant that lists options by name.
- **Committing an already-failed transaction reports the command tag `ROLLBACK`**, not `COMMIT`.
  Nothing would have caught that but a capture.
- **In protocol 3.2 the cancel key is 32 bytes, not 4** — visible as a 40-byte `BackendKeyData` in
  the 3.2 capture against 12 bytes in the 3.0 one. Since we negotiate down to 3.0 we send the short
  form, but it confirms that answering 3.2 as though it were 3.0 would corrupt the stream.

The encoder matched all 14 captured backend messages byte for byte on the first run, which is
evidence for the goldens being right rather than for the encoder being clever: the same reading of
the specification produced both, and the capture is the only independent party.

**Unit 5.** The trait is the one above, and the fake behind it is a real little MVCC store rather
than a map: snapshot reads, buffered writes, and the one conflict rule that matters — a commit fails
if any key it wrote gained a version after its snapshot, which is what `docs/DESIGN.md` §8's
prewrite check does. That is deliberate. A fake that accepted every commit would let an executor
test *assume* the unique-index race is handled; this one makes the race observable, and
`a_concurrent_duplicate_loses_at_commit` watches two transactions read the same index key as absent,
both write it, and exactly one survive.

A write conflict is `40001 serialization_failure` at this layer, not `23505`. The distinction is
contract C3: at the storage seam a lost race is a lost race, and it is the *executor* that knows the
key was a unique index entry and so knows to report a duplicate. Turning every conflict into `23505`
here would mislabel an ordinary row-level race as a constraint violation.

**Unit 2d.** The listener is the only place in the crate that knows what a socket is, and
`Connection` is generic over the stream rather than tied to `TcpStream`, so the whole handshake is
driven over an in-memory pipe in five tests that need nothing installed. The sixth is a real `psql`,
and it is the only test in the crate whose other end was written by someone else — which is the only
way to find out whether our reading of the protocol and theirs agree. It skips when `psql` is
absent, because that is a fact about the machine and not a defect in the server.

Two details in it are load-bearing. An error that ends the connection is reported `FATAL` even when
the condition is ordinary, because a client told `ERROR` waits for a `ReadyForQuery` that is never
coming. And the message-length cap is enforced at the read, before the body is reserved, so a client
claiming a gigabyte gets an error rather than the allocation.

**Unit 2c.** `psql` cannot drive the extended protocol finely enough to answer the question this
unit turns on — what a server does with messages sent *after* a failure and *before* `Sync` — so the
capture was taken with a raw protocol client instead. The answer settles the trap the brief named:

- **A failure answers immediately, and then the server goes completely silent.** The `Bind` and
  `Execute` that followed a failed `Parse` produced *no bytes at all* — not an error each. A server
  that answered them would put extra messages in the stream and desynchronise the client for the
  rest of the session, which is worse than the original error and far harder to diagnose.
- **`Sync` alone sends `ReadyForQuery`**, and it is what clears the skip state.
- **`Describe` on a statement sends two messages** (`ParameterDescription`, then the row shape) and
  on a portal **one**, since a portal's parameters are already bound. One message too many here
  desynchronises rather than confuses.
- **`Execute` with a row limit answers `PortalSuspended`, not `CommandComplete`**, and the portal
  stays open — a second `Execute` resumes where the first stopped.
- **The empty statement is legal all the way through**: it parses, binds, describes, and executes to
  `EmptyQueryResponse`. Drivers probe with it, so refusing it would break them.

Two things are deliberately not done. `ParameterDescription` reports the types the client
*declared* rather than inferred ones, because inference needs the planner to type the expressions a
parameter appears in — marked `TODO(unit-6)` at the call site. And the extended protocol's failure
state is kept separate from the transaction's: an error inside a block sets both, and `Sync` reports
`E` while still clearing the skip.

**Unit 2b.** Six more rules read off captures rather than out of the specification, each now a
test: `ReadyForQuery` is once per *message* and not once per statement; an error abandons the rest
of the query string; an error **outside** a block leaves the status `I` rather than `E`; a second
`BEGIN` is a *warning* that still completes with the tag `BEGIN`, and so is `COMMIT` or `ROLLBACK`
outside a block; committing a failed transaction reports the tag `ROLLBACK`; an empty string gets
`EmptyQueryResponse` and no `CommandComplete`.

The fourth of those found a real bug the moment it ran. A warning was being returned as an error,
so the session sent the notice and then *no* `CommandComplete` — a client would have been told
something was odd and never told the command had finished. Warnings and failures are different
control flow, not different severities on one path, and the capture is what made that obvious.

`Execute` is the seam the executor will implement in unit 6, and transaction control is separate
from `execute` on it: the session has to know about `BEGIN`/`COMMIT`/`ROLLBACK` because they move
the status a client sees, while a real implementation still needs to run them and to be able to
*fail* while doing so. A commit that fails is a Percolator conflict, and the session ends the
transaction regardless — leaving it in `T` would have the client waiting for a block that is gone.

Containment (ADR 0014) needed a small piece of design here. The session holds parsed statements and
hands them to the executor, and holding `sqlparser::ast::Statement` to do that would already have
broken the promise that replacing the dependency is a one-file job. So `parse::Parsed` wraps the AST
and exposes only its class and its rendering; the lowering to our own plan types lands with the
planner.

**Unit 1b.** The corpus was going to be written from the PostgreSQL documentation. It is instead
written against a **running PostgreSQL 19beta1**, because a container of the target release turned
out to be one `docker run` away and an oracle beats a recollection — it immediately rejected two
statements that had looked right. The result is the first hard number this contract has: of the 353
statements real PostgreSQL 19 accepts that the corpus held then, `sqlparser` parses **281**. The
other 72 are registered in §9, and the thirteen of them that sit on the query path are now the most
concrete piece of work this plan has, because each is a statement a user could write today and get a
syntax error for where the contract promises `0A000`. (Both numbers moved later, when the `ALTER
TABLE` sweep took the corpus to 402.)

## 12. Milestones decided here and built later

Designs that are settled, written down as ADRs, and deliberately not implemented in this phase.
Each is a phase of its own; what they have in common is that all three are cheap to *design* now
and expensive to retrofit if the formats they need are chosen without them in mind — which is the
reason they are here rather than in a notebook.

| # | Milestone | ADR | What has to exist first | Size, honestly |
|---|---|---|---|---|
| M1 | **Distributed online schema change** — `CREATE INDEX` without blocking, and the door to `DROP COLUMN` and type changes | [0020](../adr/0020-online-schema-change.md) | a schema lease published by PD; per-column and per-index states in the catalog | a phase. The state machine is small; the lease is the hard part, because it must stop a node that believes it is healthy |
| M2 | **The time machine** — historical reads, checkpoints, `DIFF`, `FLASHBACK` as compensating writes | [0021](../adr/0021-time-machine.md) | `TxnClient::begin_at`; the collector honouring the per-table retention records, which **exist now** | a unit each, on machinery that is already built. The storage layer is a time machine already; this is a surface and a bound |
| M3 | **A columnar learner replica** — HTAP: a second copy of opted-in tables in a columnar layout, fed by the same Raft log, with the planner choosing per query | [0022](../adr/0022-columnar-learner-replica.md) | a columnar file format (a phase of its own); the fragment protocol; `esker-raft` needs **nothing** — learners and `ReadIndex` are built | the largest of the three. A second engine is an `esker-engine`-scale build and then there are two to operate; disk is the *cheapest* part of it, at +3–11% |
