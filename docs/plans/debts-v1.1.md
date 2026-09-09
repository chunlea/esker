# Debts still open at v1.1

**One row carries over from v1 — #4 — nine are new, one of the nine closed between being
written and being committed** (#11, and it is left in place with what closed it), **and one was
ruled rather than paid** (#16: keep the behaviour, reason ADR 0083). So the count of debts a
reader has to act on is eight, and the table below has ten rows: a register that deletes a settled
row is a register that gets the same question asked again.

**Then #17 was paid and two rows were opened by paying it** (#19, #20 — both found by measuring
the shapes rather than by reading the code, which is the argument for measuring), so the count is
eleven to act on across thirteen rows (#21 and #22 arrived the same way, from the same
corpus; #22 was #12's consequence until b4's int4 ladder landed and it stayed). #17 moved to §2 with what closed it, which is this file's own
rule: close a row where the row lives, in the commit that closes it. The numbering continues
[`debts-v1.md`](debts-v1.md)'s rather than restarting, because the numbers are quoted across lanes
and a register that renumbers makes every quotation of it wrong. A closed row leaves a **gap**.

Two rules come from that file's own failures, and both are applied here rather than restated:

> **Close a row where the row lives, in the commit that closes it.** Every lag `debts-v1.md` had
> came from a closure recorded somewhere the register does not read — once in an ADR whose first
> line said it closed two rows of it.
>
> **Asking "is the site still there" is not asking "is the symptom still there."** Six of that
> file's eight rows turned out to be closed when somebody looked. So every row below was put to the
> tree or to a log before it was written, and each says which; the two that could only be taken
> from another lane's evidence say that too.

`docs/acceptance/v1.1.md` will carry the numbers when run 107 has them. This file carries what is
left.

---

## 1. Open

| # | Debt | Site | Size | Owner | Verified how |
|---|---|---|---|---|---|
| 4 | **Cross-node deadlock detection.** The wait-for graph is node-local, which covers every deadlock two sessions of one `esker-sql` process can make. A cycle *across* nodes needs a graph both can see. | `crates/esker-sql/src/backend/locks.rs:61` — the doc comment still says "Node-local … a cycle across nodes needs a graph both can see — PD's job, and a named follow-on" | large — needs a PD-held graph | PD / pdha | read at HEAD; the site moved from `:46` to `:61` since v1, which is the whole reason a register cites text and not only a line |
| 9 | **`esker-raft` pre-vote term climb.** `promotion`'s `under_load` test found a region that elected no leader for 200 s at load 12–13. The diagnosis is a term that climbs through pre-vote rounds instead of settling. | `crates/esker-raft` election path | unknown until the diagnosis lands | h1 (diagnosing) | `$S/HANDOVER-h1-part-cursor-and-promotion.md`, and the gate log it came out of: `$S/gate-g1-95747668.log` (that batch's diff was all `esker-sql`; the failure was `esker-store::promotion`, and the same tree passed on a re-run — which is what makes it a *load-sensitive* row rather than a broken build) |
| 10 | **The mpp differential's time-sensitive assertion.** `multi_region_differential` waits for a `count(*)` fragment and not for the shape it is about, so it fails under load rather than on a difference. | `crates/esker-sql/tests` (mpp differential) | small — wait on the artefact, not the clock | h1 | `$S/gate-h1-c31da348.log` around line 150 |
| 11 | ~~A cursor's continued scan routes to the wrong region.~~ **Closed before this file was committed**, and the row is kept because it shows what the register is for: the gate's `08006 … key is not in region 0` was not a routing defect but a **bound** — `Router::route` raises it itself when the placement driver has not yet learned of a split, and the wait for that was fixed at five asks (~310 ms), which is fifteen heartbeats on an idle box and less than one effective cycle on a loaded one. `75ab41a9` gives the lookup **the caller's own deadline** instead of a number invented in the router. | closed by `75ab41a9`, with the test at `ab130d48` | — | h1 | I wrote this row from `$S/gate-h1-ab130d48.log` as "h1 diagnosing, batch held"; both had landed by the time the file was committed, so the row was re-read against `git log` before it was written down. **A register written from a gate log is one landing behind by construction** |
| 12 | **`int4` is measured and not implemented.** `pg_typeof(1)` is `integer` on PostgreSQL 19 and `bigint` here: this node has one integer literal type and it is `int8`. The rung above it is implemented — a literal past `int8` is `numeric` — so what is left is the *bottom* rung. **A deparse looked like this row's consequence and was not**: `GENERATED ALWAYS AS (1::bigint)` prints `(1)::bigint` on a real server and `1` here, and it still did after b4's ladder landed, so it moved to #22 — the printer, not the type. Worth keeping as the shape of the mistake: the same symptom sat on two causes, and only the landing told them apart. | `crates/esker-sql/tests/integer_literal_type.rs`'s module docs carry the whole measured ladder | medium, and the blast radius is the point: it changes the `RowDescription` OID of every `SELECT 1` the suite sends | b4 | read at HEAD: the test file says so in the section titled "the `int4` rung is measured and **not** implemented, deliberately" |
| 13 | **No `void` type.** Every advisory function that returns `void` on a real server answers an **empty string** here. The value prints the same and `pg_advisory_unlock_all() IS NULL` is `f` on both — only the type name differs. | `crates/esker-sql/tests/advisory_lock.rs`'s declared divergence | small — one `ColumnType`, but it is a type-surface change | b4 | read at HEAD |
| 14 | **Four type families this node does not have**, each declared once rather than argued per corpus: `regtype` (a real server's is four bytes holding an oid that print as a name; ADR 0077), `"char"` (the one-byte type, which is what `relkind`, `contype`, `typcategory` and `typdelim` are), `regproc` (`typinput`), and `oid` where the catalog's own oid columns are `bigint` here. | the table on `parity::Divergences::types` in `crates/esker-sql/tests/parity_harness/mod.rs` | medium each; `oid` is the cheapest and `"char"` the widest | b4 (type surface) | written from the rule-4 scan at `74b41a8f`, which counted them: 20 `regtype`, 6 `"char"`, 6 `oid`, 3 `regproc` |
| 15 | **`name[]` has no array type**, so `current_schemas()` is `text` where a real server answers `name[]`. Seven corpus rows. | `value::array_oid`'s named gaps, [ADR 0084](../adr/0084-name-is-a-stored-type-and-its-tag-is-additive.md) | small | **b4, in flight** — do not touch; the seven entries come out in the batch that closes it | the rule-4 scan; the gap is named in `array_delimiter.rs`'s own list |
| 16 | ~~`DROP INDEX CONCURRENTLY` is asynchronous.~~ **裁决：保持，理由 ADR 0083** — ruled by the user on 2026-09-09 and therefore **not an open debt**; the row stays because a reader who meets the asynchronous drop needs to find the decision, and a deleted row sends them to read the code and rediscover the question. The original entry follows, unchanged: **`DROP INDEX CONCURRENTLY` is asynchronous.** `CREATE INDEX CONCURRENTLY` answers when the build is done ([ADR 0083](../adr/0083-a-concurrent-build-answers-when-it-is-built.md)); the removal direction still returns as soon as the job exists. Deliberate: its last step waits the **retention** window rather than the step interval, because what it must outlast is a reader, and a statement that held a client for an hour would be worse than one that returns. | `crates/esker-sql/src/exec/ddl.rs`'s concurrent drop | medium, and it needs a decision before it needs code | g1 | ADR 0083 records it as a decision, not an oversight; nothing in the suite executes the concurrent drop — `active_schema_test`'s two `remove_index … concurrently` cases assert generated SQL only |
| 19 | **No collation derivation, so this node builds a generated column a real server refuses.** `GENERATED ALWAYS AS (upper('a')) STORED` is `42P22 could not determine which collation to use for upper() function` on PostgreSQL 19beta1: a generated column's expression must have a determinable collation and a literal argument carries none. `COLLATE` here is recorded per column and never derived through an expression, so the column is built and the value it stores is the value a real server would have computed. C3 in the accepting direction. | `crates/esker-sql/src/plan` has no collation inference; `crates/esker-sql/tests/generated_parens.rs` declares it | medium, and it is a **type-system** unit rather than a DDL one — collation would have to flow through every expression node | unassigned | measured on the oracle while building the deparser corpus (`tests/captures/pg19_deparse_parens.txt:141`), and declared there with that provenance |
| 20 | **A capture cannot hold a `\|` or a newline in a value**, and both failures are silent. A corpus row is `statement TAB types TAB rows` split on `' ; '` and `'\|'` with no escape, so `length((t \|\| 'x'))` parses as three columns and "disagrees" while reading identically in the report; and `sesscap.py` drops any output line beginning with a space — its psql-error-context filter — which is every continuation line of a multi-line value, so a `CASE` came back as ` ; CASE ; END` with the middle silently gone. Both cost a round of this unit. | `crates/esker-sql/tests/parity_harness/mod.rs`'s `parse`; `sesscap.py`'s row filter in the harness repo | small for the harness (an escape, or a different separator); the tool half is r1's | unassigned | both reproduced while building `pg19_deparse_parens.txt`: the pipe by reading the raw `Answer` with `{:?}`, the newline by querying the oracle directly and comparing |
| 21 | **No `ALL` quantifier**, so `NOT IN` cannot be printed the way a real server prints it. `pg_get_expr` says `(c1 <> ALL (ARRAY[1, 2]))` for `GENERATED ALWAYS AS (c1 NOT IN (1, 2))`; there is an `Expr::AnyArray` in this node's expression language and no `AllArray`, because `= ANY` was needed by a query and `<> ALL` never was. The *printer* is already right — `exec::ddl::deparse`'s `InList` arm prints the quantified form — and the read-back guard refuses to store a string the evaluator cannot parse, so the written text is kept. Nothing in the Rails suite writes `<> ALL`; what it costs today is one row of a deparse corpus. | `crates/esker-sql/src/plan/expr.rs` has `AnyArray` and no `AllArray`; `crates/esker-sql/tests/generated_parens.rs` declares the consequence | small — one node, one resolve arm, one evaluate arm, and the printer already exists | unassigned | measured on the oracle (`tests/captures/pg19_deparse_parens.txt:129`) and reproduced through the guard: the printed form fails `reads_back`, which is why the divergence is a *kept text* rather than a wrong one |
| 22 | **A deparsed numeric literal never shows its type.** `exec::ddl::deparse_literal` prints a number bare whatever its `Datum` is; PostgreSQL's `get_const_expr` shows the type whenever it is not the one the literal form defaults to, so `GENERATED ALWAYS AS (1::bigint)` prints `(1)::bigint` there and `1` here. This used to be #12's consequence and stopped being one when b4's int4 ladder landed (ADR 0087): the node now knows `1` is an `int4` and `1::bigint` an `int8`, so the information the printer needs is there and the printer does not use it. | `crates/esker-sql/src/exec/ddl.rs`'s `deparse_literal`; `crates/esker-sql/tests/generated_parens.rs` declares the one row | small in code, and **the measurement is the work**: which constants show a cast, for each numeric type and in each position (a bare key, inside a comparison against a wider column, inside a call), which is a capture nobody has taken | unassigned | reproduced on the merged tree at main 9d642ef0 — b4's two ADRs are in it and the row still differs, which is what moved this off #12 |
## 2. Closed since v1, and recorded here because the register is where a reader looks

| # | Debt | Closed by |
|---|---|---|
| 17 | **`pg_get_expr` parenthesised only the outermost operator**, and a default's literal showed no coercion inside a call. | **This unit.** The rule was not missing, its caller was: `exec::ddl::deparse` already printed every operator node in its own pair and was reachable from the index key list alone. Routing `index_expression` through it unconditionally, normalising `ALTER TABLE ... ADD COLUMN ... GENERATED` the way `CREATE TABLE` already did, and giving a `DEFAULT` the same treatment at all three statements that store one closed the six declared divergences in `tests/generated_parens.rs` to zero. Three shapes needed the tree and could never have come from parenthesising text — `LIKE` prints as `~~`, `IN` as `= ANY (ARRAY[…])`, `BETWEEN` as two comparisons — and are measured in `tests/corpus/pg19_deparse_parens.txt`, 86 statements, 84 agreeing. |
| 18 | **`yes_or_no`'s length.** `information_schema.columns.is_nullable` is a domain over `character varying(3)` and this node declared `character varying` — a catalog column list carried a type and no length. | this file's own batch: the list carries a length now, and the measurement behind it is that **no `pg_catalog` column has a typmod at all** on a real server, and of `information_schema`'s five domains only `yes_or_no` and `time_stamp` (which no view here serves) carry one |

## 3. Where the evidence is

**`$S` and `$R` are outside this repository and `$S` does not outlive its session**, which is why
every citation above also names a *hash* or a file **name**: a gate log is
`gate-<lane>-<hash>.log` wherever it is kept, and the hash is in this repository's history for
ever. `$S` is the coordinator's scratchpad for the night of 2026-09-08/09 and `$R` is
`esker-rails-harness/results`; the index of both is `$S/v11-evidence.txt`, and the durable half of
each row is the sentence, not the path. The three that matter most for a reader
coming to this cold:

* **the landing chain**, thirteen batches with a `ci-tree` gate log each (`$S/gate-<lane>-<hash>.log`,
  carrying the `FMT`/`CLIPPY`/`DOC`/`DENY`/`TEST` return codes and the nextest summary);
* **the scoreboard**: run 104 10,048/10,134 = 99.15 %, run 105 10,067 = 99.34 %, run 106 10,071 =
  99.38 %, each with its routing table under `$R`;
* **the gates that went red and passed on a re-run of the same tree** — four of them, and every
  one was load-sensitive rather than a defect in the batch: they are rows 9, 10 and 11 above. Row
  11 turned out to be a *bound* that only looks load-sensitive — five asks is fifteen heartbeats
  idle and less than one loaded — which is the difference worth learning from those four: "passes
  on a re-run" is a symptom, and it has both an innocent cause and a guilty one.

## 4. ADR numbering, re-taken

Checked mechanically over `docs/adr/*.md` at this commit:

| Check | Result |
|---|---|
| files matching `NNNN-*.md` | **84** |
| distinct numbers | **84** |
| range | **0001 – 0084** |
| gaps in 1..84 | **none** |
| numbers used by two files | **none** |

Twenty-one ADRs since v1's sixty-three. Numbering has collided twice in this project's history —
ADR numbers and, separately, the catalog record version — and both were caught at merge rather
than at write time, so the standing rule holds: **claim the number out loud in the report the
moment it is taken.**

## 5. How to re-take this

```sh
# ADR contiguity and duplicates
ls docs/adr/[0-9]*.md | sed 's|.*/||' | cut -c1-4 | sort | uniq -d
ls docs/adr/[0-9]*.md | wc -l

# every declared divergence, by family — the rule-4 scan's own list
rg -n 'types: &\[' -A 40 crates/esker-sql/tests/*.rs | rg '^\s*"SELECT'

# what is still UNMEASURED, which is the number `provenance_budget` holds
rg -c 'UNMEASURED' crates/esker-sql/tests/*.rs | sort -t: -k2 -rn | head
```
