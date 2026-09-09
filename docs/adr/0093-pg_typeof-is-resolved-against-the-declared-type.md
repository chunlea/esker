# 0093 — `pg_typeof` is resolved against the declared type, at plan time

Status: accepted · 2026-09-09

**Supersedes one consequence of [ADR 0077](0077-regtype-is-an-oid-that-prints-as-a-name.md)** — the
bullet reading "**`pg_typeof('integer'::regtype)` becomes `regtype`** where it has been `text`. That
is a declared divergence being deleted, not a new one: several corpora list it, and the ratchet will
name each as it starts agreeing." That happened for the one argument the bullet named and for no
other, because `pg_typeof` was still reading the datum. This file makes it the general case: the
function answers a `regtype` for *every* argument, and it answers the argument's declared type. The
rest of ADR 0077 — the value model, the ordering by oid, `oidvector`, the user-type half — stands
unchanged.

## Context

`pg_typeof` read the **datum**. A datum cannot say which of the types that share its representation
it is, and this crate has many such families:

```text
  text, varchar, bpchar, name, json, jsonb, xml, citext   one Datum::Text
  cidr and inet                                            one Datum::Inet
  void                                                     an empty Datum::Text
  an enum                                                  a Datum::Int2 (ADR 0050)
  a floatrange                                             a range representation
```

So the function answered `text` for a `name`, `cidr` for an aggregate declared `inet`, `smallint`
for an enum. It was **wrong in the one direction that matters**: the `RowDescription` beside it was
already right, so one expression had two answers — and every unit that touched the type surface
declared a slice of the same fact rather than closing it. The four most recent ones each left a
list behind: `name[]` nine rows, the decimal literal five, `cidr` five, `void` two.

The code had noticed. `exec::query::resolve` already folded `pg_typeof` at plan time for an **enum
column** — because a client told `smallint` for an enum is the worst class ADR 0031 ranks — and
then for `hstore[]`, `json` and `jsonb`, each with its own paragraph saying "the static type knows
and the value does not". Three exceptions with one reason is a rule that has not been written yet.

Beside that sat a second fact: `pg_typeof`'s own return type. It is a `regtype` on a real server and
was `text` here, and the comment saying why — "this node has no `regtype`" — had been false since
ADR 0077 built one, fifteen ADRs earlier.

## Decision

**1. `pg_typeof` is answered in `resolve`, from `expr_type` of its argument, for every argument.**
The three exceptions are deleted; a user-defined type still names itself, through the same
`Scope::user_type_at` that fills `OutputColumn::user_type`, so the function and the
`RowDescription` read one source.

**2. The argument stays an argument.** Folding the whole call to a constant — which is what the
three exceptions did — loses the rows a set-returning argument produces:
`pg_typeof(unnest(ARRAY['a','b']))` is **two** rows on a real server, measured, each carrying the
same type name. So the resolved type rides along as a *second* argument and the evaluator answers
that, leaving the first to be evaluated exactly as before.

**3. `pg_typeof` answers a `regtype`**, which is its `prorettype`. That is ADR 0077's model
finally reaching the function that motivated recording it.

## Consequences

* **179 declared divergences deleted across 57 test files** — every corpus that carried
  "`pg_typeof` answers a `regtype` there and `text` here", and every one that carried the datum
  seam. The ratchet named each of them; none was chosen.
* **It found two live wrong answers by making the two paths agree out loud**, which is the argument
  for the whole shape:
  * `lower(range)` and `upper(range)` answered `text` where a real server answers the **subtype** —
    `lower(ts_range)` is a `timestamp`, measured. The evaluator had told the two overloads apart by
    the operand since the range unit; only the type side had not.
  * `daterange(a, b)` was declared `text` where it builds a `Datum::Range` over `date`. Its own doc
    comment called that "the standing trade and a declared divergence"; it was a type nobody had
    asked for.
* A guard written as "its subtype is not `text`" made `lower('MiXeD')` a `timestamp`, because
  `range_subtype` answers `timestamp` for everything it does not know. The list of eight range types
  is written out instead — a total match would have forced that in the first place, and this is the
  second time in this queue that a helper's fallback has been read as an answer.
* **What is left is `unknown`.** `pg_typeof(NULL)` and `pg_typeof('x')` are `unknown` on a real
  server and `text` here, because this crate resolves a bare literal to `text` before anything asks.
  That is `tests/unknown_literal.rs`'s standing divergence seen through one more function, and it is
  the only `pg_typeof` row still declared.
