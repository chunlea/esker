# ADR 0096 — A collation is derived from a column, or from nothing

**Status**: accepted · **Date**: 2026-09-10 · Debt `debts-v1.1.md` #19 is this row.

**The number was claimed on 2026-09-08 and the file was never written** — `docs/acceptance/v1.1.md`
records it as *"not on `main`: the number is claimed and the file is not written"*, which is the
one row of that table that names an absence. This is the file, and it is written after the family
was measured rather than before, which is why it can be `accepted` on the day it appears.

## Context

`COLLATE` is recorded per column (`catalog::ColumnDef::collation`) and **never derived through an
expression**. `C` and `POSIX` are the only collations this node has
([ADR 0076](0076-c-and-posix-are-the-collations-this-node-has.md)) and both order by byte, so
nothing here is about a wrong *answer*: it is about which statements exist. A real server refuses
several that this node builds, and answers one that this node answers.

The family was measured on 19beta1 one placement at a time —
`tests/corpus/pg19_collation_family.txt`, 118 statements, and a second pass on 2026-09-10 that
re-asked the three questions this ADR turns on.

### The corpus header was wrong about two of its three contexts, and the corpus rows were right

The header of that file says a generated column, an index key **and** a `CHECK` are
collation-requiring contexts, and `debts-v1.1.md` #19 was sized from that sentence — *"so
`upper('a')` is refused in three places and answered in two"*. **Its own rows say otherwise**, and
re-measuring says otherwise:

```text
                                            19beta1
SELECT upper('a')                           A
DEFAULT upper('a')                          accepted
CREATE INDEX ON g ((upper('a')))            accepted, index built
CREATE INDEX ON g (n) WHERE upper('a')='A'  accepted
CHECK (upper('a') = 'A')                    accepted, constraint built
GENERATED ALWAYS AS (upper('a')) STORED     42P22
```

**One context asks, not three.** The header's explanation — *"a generated column, an index key and
a check constraint are values that get **compared**, and a comparison is what needs the
collation"* — is refuted by its own table: an index key and a `CHECK` are compared and do not ask.
That sentence was marked in the header as a reading rather than a measurement, and it is wrong; it
is kept here struck rather than deleted because a register that hides its wrong readings teaches
nothing.

### And there are two mechanisms, not one

The second pass asked the same statements against a table **with a row in it**, which the first
pass did not, and that is what separates them:

```text
                                     empty table      one row
SELECT u < v          (C vs POSIX)   accepted         42P22 string comparison
GENERATED AS (u < v)  (C vs POSIX)   accepted         42P22 string comparison
GENERATED AS (upper('a'))            42P22            42P22 upper()/lower() function
```

* **A conflict between two *implicit* collations is an evaluation-time error.** It fires when a row
  reaches the operator, in a query and in a generated column alike, and never on an empty table. It
  is not a DDL check.
* **A generated column's expression must have a collation derivable from a column**, and that *is*
  a DDL check: it fires with or without rows, and only for a generated column.

Reading the first as the second is how "three contexts" happened: on an empty table a generated
column and an index expression look alike, and the difference only appears once something is
evaluated.

### What the rules are, measured

**Which operations use a collation** — and the message names the operation, in PostgreSQL's own
words. **Six names**, from a census of 43 shapes asked in one session
(`tests/captures/pg19_collation_operations.txt`); the placement-by-placement corpus had reached
three, and the three it had not are the ones a reader would not guess:

```text
lower('a')                                           -> "lower() function"
upper('a')  upper('a'::text)  upper(1::text)         -> "upper() function"
initcap('a')                                         -> "initcap() function"
('a' < 'b')  ('a' = 'b')  ('a' <> 'b')  ('a' >= 'b')
('a' IN ('b'))  ('a' BETWEEN 'a' AND 'b')
(CASE WHEN 'a' = 'b' THEN … END)  (ARRAY['a'] = ARRAY['b'])
replace  split_part  strpos  string_to_array
greatest  least  nullif  array_position               -> "string comparison"
('ab' LIKE 'a%')                                     -> "LIKE"
('ab' ILIKE 'a%')                                    -> "ILIKE"
('a' ~ 'b')   ('a' ~* 'b')                           -> "regular expression"

no collation: 'a'  ('a')::text  (1 + 2)  (1 = 2)  (1 < 2)  length  octet_length  md5  abs
              ascii  reverse  substr  substring  btrim  ltrim  rtrim
              ('a' || 'b')  COALESCE('a','b')  CASE WHEN true THEN 'a' ELSE 'b' END
```

**`replace` compares and `substr` does not**, which is the pair that says this is not a rule about
names; `greatest`, `least` and `nullif` compare, which nobody would put on a list of string
functions; `COALESCE` picks rather than compares and does not, where a `CASE` asks through the
comparison in its `WHEN`; and `||` needs no collation at all. **The type decides, not the
operator**: `(1 = 2)` is accepted and `('a' = 'b')` is not. None of it is derivable; all of it is
rows in the census.

**And two refusals in that census are not this family**: `concat('a','b')` and `to_tsvector('a')`
are `42P17 generation expression is not immutable`. Recorded because a census that reported them
as "refused" would have put two immutability rows in a collation list.

**Only a column settles it, and any column will do.** An explicit `COLLATE` on anything that is not
a column does not help, wherever it is written; a column does, even through a cast and even when
the column is not collatable:

```text
refused   upper('a' COLLATE "C")        upper('a'::text COLLATE "C")
refused   upper('a') COLLATE "C"        (('a' COLLATE "C") < 'b')
accepted  upper(t)   upper(u)   upper(t COLLATE "C")   (t < 'b')   (t || 'x')
accepted  upper(n::text)                -- n is an *integer* column
accepted  upper(COALESCE(t, 'a'))       CASE WHEN t = 'a' THEN 'x' ELSE 'y' END
```

**Two explicit collations that disagree are a different error, and a parse-time one**:

```text
SELECT (u COLLATE "C") < (t COLLATE "POSIX")                  42P21 collation mismatch
GENERATED AS (((t COLLATE "POSIX") < (u COLLATE "C"))) STORED 42P21 collation mismatch
```

`42P21 collation mismatch between explicit collations "X" and "Y"`, the names in the order the
expression writes them, no `HINT`. It fires on an empty table, in a query and in DDL alike.

**And the printed form keeps the clause.** `upper(t COLLATE "C")` prints back as
`upper((t COLLATE "C"))` on a real server; this node's parser reads the clause and drops it, so the
stored expression prints `upper(t)`. A derivation that did not carry the clause through would make
the catalog disagree with the rule it was enforcing.

## Decision

**Derive a collation and a *derivation strength* over the expression tree, on demand, at the three
places that ask — and do not put a collation on every node.**

`debts-v1.1.md` #19 sized this as *"a collation has to flow through every expression node, which is
the same shape as the type surface"*. The measurement says it does not have to: three call sites
ask, each has the whole expression in hand, and a recursive function answering
`(Option<Collation>, Derivation)` is enough for all three. `Derivation` is the SQL standard's own
three-valued thing — **none**, **implicit**, **explicit** — and it is what makes the four measured
rules one rule:

| an operand is | collation | derivation | a column under it |
|---|---|---|---|
| a literal, or anything built only from literals | the type's default | **none** | no |
| a column reference | the column's | **implicit** | **yes** |
| `x COLLATE "C"` | `C` | **explicit** | whatever `x` had |
| a cast, `COALESCE`, `CASE`, `\|\|`, `substr`, … | the merge of its inputs | the merge | either input's |

and the merge of two operands is: **explicit wins**; two disagreeing explicits are `42P21`; two
disagreeing implicits are a *conflict* that survives to evaluation; otherwise the stronger one.

**The fourth column is not the third, and the build is what proved it.** An explicit clause names
an ordering and does **not** make one derivable: `upper(t COLLATE "C")` builds and
`upper('a' COLLATE "C")` is `42P22`, and both are `explicit`. So rule 2 below asks *"is a column
under this operation"*, not *"is the derivation stronger than none"* — a check on the strength
accepts four statements a real server refuses, which is exactly the direction this row exists to
close. Measured one placement at a time; the four came off `tests/collation_family.rs`'s declared
list the moment the predicate was corrected.

Then:

1. **`42P21`, at parse time, everywhere.** Two explicit collations that disagree under one
   collation-using operation. This is the one rule that has nothing to do with context.
2. **`42P22`, at DDL time, for a generated column only.** Its expression is walked; a
   collation-using operation with **no column under it** is refused, naming the operation.
   An index expression, an index predicate, a `CHECK` and a `DEFAULT` are **not** walked, because
   a real server does not walk them.

   **Six operation names, not three, and `string comparison` is much wider than `<`.** The
   placement-by-placement corpus reached three; asking **43 shapes in one session**
   (`tests/captures/pg19_collation_operations.txt`) found `lower() function`, `upper() function`,
   `initcap() function`, `string comparison`, `LIKE`/`ILIKE` and `regular expression` — and put
   `greatest`, `least`, `nullif`, `strpos`, `split_part`, `string_to_array`, `array_position` and
   `replace` in the comparison bucket while leaving `substr`, `btrim`, `ltrim`, `rtrim`, `reverse`,
   `length`, `||` and `COALESCE` out of it. **The type decides, not the operator**: `(1 = 2)` is
   accepted and `('a' = 'b')` is not, so a comparison asks only over a *collatable* type. None of
   that is derivable from a name, which is why it is a list and why the list was measured whole
   rather than extended one function at a time.
3. **`42P22`, at evaluation time, everywhere.** A collation-using operation whose operands carry
   two disagreeing implicit collations. This is where the conflict from the merge lands.
4. **The deparser keeps `COLLATE`.** `plan::Expr` gains the clause so a stored expression prints
   `upper((t COLLATE "C"))`, which is both a parity fix and the thing rules 1–3 read.

**Why not "flow it through every node"** (the shape #19 named): a collation on every `Expr` is the
`ColumnType` change ADR 0103 shape B refused for the same reason — a closed shape matched
exhaustively across the crate, where every site has to answer a question that only three sites ask.
The recursion is re-run per statement and its cost is one walk of an expression that has already
been walked several times.

**Why not declare the whole family** (the shape this node has today): rule 1 and rule 3 are
statements a real server **refuses** and this node **answers**, which is the direction ADR 0031
calls worst — a wrong answer where the right one is a refusal. `C` and `POSIX` both order by byte
so no *value* here is wrong today, and the day a third collation arrives every one of these becomes
a wrong value rather than an extra statement.

## Scope — what is in and what is not

**In:**

* the three collation-using families, by the three names PostgreSQL prints:
  `upper()/lower() function`, `string comparison` (`<`, `<=`, `>`, `>=`, `=`, `<>`, `replace`),
  `LIKE`;
* `42P21` with both collation names, in the expression's own order, and no `HINT`;
* `42P22` with `HINT: Use the COLLATE clause to set the collation explicitly.`;
* the generated-column DDL check, and **only** that context;
* `COLLATE` surviving into a stored expression's printed form, parenthesised as a real server
  prints it.

**Not in, and each for a stated reason:**

* **A third collation.** ADR 0076 stands: `C` and `POSIX` are what this node has, and a collation
  is still a name and a byte order rather than a locale.
* **`ORDER BY … COLLATE`, index collations (`pg_index.indcollation`), and `COLLATE` in a `CREATE
  INDEX` key.** They are a different surface and nothing in the captured suites sends them; this
  ADR is about *derivation*, not about a new place to write the clause.
* **Non-deterministic collations** (`nondeterministic = true`), which change what equality means.
  A real server has them and this node has no locale provider to build one on.
* **The `md5` row** in the same corpus: it is `debts-v1.1.md` #26 and a missing function, not a
  collation.

## Consequences

* Sixteen declared divergences in `tests/collation_family.rs` come off, and the corpus header and
  `debts-v1.1.md` #19 are corrected on the "three contexts" reading in the same change as the
  first family.
* `plan::Expr` grows one node (`Collate`), which is the only structural change and is what the
  deparse fix needs anyway.
* An expression this node used to accept becomes a refusal. That is the point, and it is the
  direction that costs nothing: no stored value moves, because no expression that is refused was
  ever stored.
* The evaluation-time rule (3) is the one with a runtime cost, and it is paid only by a
  collation-using operator whose operands are two columns with different collations — which
  requires a table that declares one, and `C` and `POSIX` are the only two that can differ.
