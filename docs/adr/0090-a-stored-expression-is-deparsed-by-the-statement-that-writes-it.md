# 0090 — A stored expression is deparsed by the statement that writes it

## Context

PostgreSQL stores expressions as trees. `pg_attrdef.adbin` is a `pg_node_tree`, and every client
that wants to read a default, a generated column, an index key or a check constraint reads it
through `pg_get_expr(adbin, adrelid)`, which **prints the tree** rather than returning what anyone
typed. The printed form is not the written form, and the differences are not cosmetic:

```text
written                              printed
(c1 * 2 + 3)                         ((c1 * 2) + 3)      every operator node takes its own pair
- c1 * 2                             ((- c1) * 2)        unary minus is an operator
(c1 + c2)::bigint                    ((c1 + c2))::bigint two pairs, not one
upper('a')                           upper('a'::text)    an unknown literal shows its coercion
coalesce(c1, c2 + 1)                 COALESCE(c1, (c2 + 1))
c1 BETWEEN 1 AND 10                  ((c1 >= 1) AND (c1 <= 10))
c1 IN (1, 2)                         (c1 = ANY (ARRAY[1, 2]))
t LIKE 'a%'                          (t ~~ 'a%'::text)
DEFAULT - 1                          '-1'::integer
```

`ActiveRecord` reads these strings and matches them with regular expressions —
`virtual_column_test#test_schema_dumping` asserts `as: "\(column1 \+ 1\)"` to the character, and
`schema_dumper_test#test_schema_dump_expression_indices` names `lower((name)::text)` — so the
printed form is part of the wire contract and not an implementation detail.

This node stores **text**, not a tree ([ADR 0030](0030-the-row-codec-moves-down.md)'s six stored types
are values; an expression is a string in the catalog record). So something has to produce the
printed form. Three options were open:

1. **store the written text and print it at every reader.** The catalog is a layer below the
   executor and cannot call a parser or a planner; `pg_attribute`, `information_schema.columns`,
   `pg_get_indexdef` and the schema dumper would each need one, and a `pg_attribute` scan over a
   wide table would deparse the same string once per row per read.
2. **store the parsed tree.** Correct, and it is what PostgreSQL does, but it changes the catalog
   record format — a golden test, a `CATALOG_FORMAT_VERSION` bump and an upgrade path — for a
   string every reader turns straight back into characters.
3. **store the printed text, produced once by the statement that writes it.**

## Decision

**Option 3. The statement that writes an expression stores it in the form `pg_get_expr` would
print, and every reader returns those bytes unchanged.**

Concretely, `exec::ddl::deparse` is the printer — one function, total over `plan::Expr`, one arm
per shape — and it is called by:

* `index_expression`, for an index key and an index predicate;
* `normalise_generated`, for a generated column at `CREATE TABLE`;
* the `ADD COLUMN` writer, for a generated column added by `ALTER`;
* `normalise_defaults` and two `ALTER` sites, for a `DEFAULT`;
* `normalise_checks` and `add_check`, for a `CHECK` — the fifth reader;
* `normalise_index_predicates` and the `CREATE INDEX` writer, for a partial index's predicate —
  the sixth;
* `normalise_exclude_predicates` and `add_exclude`, for an exclusion constraint's predicate — the
  seventh, and the last one that was still reading a text no writer had shaped.

`pg_get_expr(adbin, adrelid)` is then the identity on the stored string, and its three-argument
`pretty` form is that string with its outermost pair removed (`catalog::unparenthesised`).

### A `DEFAULT` is deparsed only for the shapes PostgreSQL reprints

Not every default is normalised, and the exception is not laziness. PostgreSQL keeps
`CURRENT_TIMESTAMP` and `now()` apart in its own tree — one is a `SQLValueFunction`, the other a
`FuncExpr` — and prints each back as it was written. This node lowers both to one node, so
deparsing a default whose whole content is a zero-argument function would print one spelling for
both and **lose an agreement that exists today**. `reprinted_by_pg_get_expr` is therefore an
allow-list of shapes — operators, casts, calls with arguments, `CASE`, `COALESCE` — and the answer
for everything else is "keep what was written", which is also the right answer for `nextval`,
`gen_random_uuid()` and a bare constant.

The polarity of that list is deliberate: the cheap mistake (keeping the text) is the default, and
the expensive one (printing a spelling the tree cannot distinguish) needs a name in the list.

## Consequences

* **The printed form is a write-path property, so it is not idempotent and must not be re-applied.**
  `upper('a'::text)` deparsed again is `upper(('a'::text)::text)`. `normalise_defaults` is
  table-wide only where every text is one a user just wrote — at `CREATE TABLE` — and the two
  `ALTER` sites normalise the single column the statement writes.
* **A rule measured for one caller reaches only that caller.** This is the third time in this
  crate: `ExprShape` was measured for an index key and asked in only that place, and the six
  divergences `tests/generated_parens.rs` used to declare were all "this node has no deparser"
  written beside a working deparser. The callers are listed above, and the list grew twice after
  this ADR was written: a `CHECK` and a partial index's predicate were being re-parenthesised at
  *read* time by a helper that split the text on the top-level `AND`, and an exclusion
  constraint's predicate was not printed by anything. **Group A of the deparse census closed all
  three**, which is what made the sentence above — every reader returns those bytes unchanged —
  true rather than aspirational, and deleted the helper.

  A splitter is why: it cannot say which operands bind first. `(a > 0 OR b > 0) AND flag` is
  `(((a > 0) OR (b > 0)) AND flag)` on a real server and came back `(((a > 0)) OR ((b > 0)) AND
  flag)` here, the grouping lost and a pair doubled in one answer. The tree has the grouping, and
  only the writer holds the tree.
* **The pretty spelling is derived from the stored one, and only because the stored one is
  canonical.** `pg_get_constraintdef(oid, true)` is what `ActiveRecord` reads, and a real server
  renders it from the same tree as the plain form. `catalog::pretty` reconstructs it instead:
  strip the outer pair, split the chain with the same scanner the writer's callers use, and give
  an operand its pair back only where precedence needs it — an `OR` inside an `AND`. That
  derivation is safe *because* the writer emits a shape it produced, and would not be safe over
  text a user wrote.
* **Three shapes prove the tree is necessary.** `BETWEEN`, `IN` and `LIKE` print as the operators
  they desugar to. No rule about parentheses over the written text can produce `(t ~~ 'a%'::text)`
  from `t LIKE 'a%'`, which is the argument against a fourth option — normalising the string.
* **A literal's type is threaded from the operator, not from the expression.** Each operand is
  deparsed under the type the *operator* takes: `length(t || 'x')` prints
  `length((t || 'x'::text))` and not `length((t || 'x'::integer))`, and `t LIKE 'a%'` prints
  `'a%'::text` and not `'a%'::boolean`. Both of those were live bugs found by the corpus.
* **What is still not printed**: a literal's own cast when the cast is folded away
  (`1::bigint` in a generated column prints `1`, because this node has one integer literal type —
  `docs/plans/debts-v1.1.md` #12), and nothing at all for a shape a generated column or index may
  not contain.

## Evidence

`tests/corpus/pg19_deparse_parens.txt`, 86 statements against PostgreSQL 19beta1 in one
`BEGIN ... ROLLBACK`, 84 agreeing and 2 declared with provenance; and
`tests/corpus/pg19_generated_parens.txt`, whose six declared divergences this decision drove to
zero. Both captures live under `tests/captures/` per
[ADR 0075](0075-the-oracle-captures-live-in-the-repository.md).
