# 0098 — `regproc` is an oid that prints as a function

Status: accepted · 2026-09-09

## Context

`pg_type.typinput` is a `regproc` (oid 24) on a real server and was `text` here. It is the last of
the four catalog type families the type-surface queue held — after `name`
([ADR 0084](0084-name-is-a-stored-type-and-its-tag-is-additive.md)), `"char"`
([ADR 0095](0095-char-is-one-byte-and-the-quotes-are-part-of-the-name.md)) and `oid`
([ADR 0097](0097-an-oid-is-four-bytes-and-a-derived-one-is-not.md)).

The census is one query, the same one those units used with a different oid: **40 columns** on
PG 19beta1, of which this node has exactly **one**. The other thirty-nine are in catalogs it does
not serve — `pg_aggregate`, `pg_operator`, `pg_ts_parser`, `pg_transform` — or are `pg_type`'s
other five function columns (`typoutput`, `typreceive`, `typsend`, `typmodin`, `typsubscript`),
which nothing reads. So this is the narrowest of the four in surface and the widest in behaviour.

## Decision

`ColumnType::RegProc` (24, `typlen` 4, `typcategory` `N`, `typinput` `regprocin`, `typarray` 1008)
and `ColumnType::RegProcArray` (1008), mirroring `RegType` at every site — the datum carries the
oid **and** the name, because `esker-keys` must not have a catalog (invariant 7). Additive on both
tag spaces: catalog record 104 and 105, columnar 105 and 106.

Four rules carry it, and each separates `regproc` from the `regtype` it looks like:

**1. An oid no function has prints as the number.** `42::regproc` is `int4in` and `24::regproc` is
`24`. There are far more oids without a function than `pg_proc` has rows, so the digits are the
common case rather than the corner one.

**2. `min`/`max` decay to `oid`.** That makes `regproc` the fifth member of the decay arm after
`varchar`, `name`, `cidr` and `"char"`, and **the first whose landing type is not `text`**.

> **Corrected 2026-09-09.** The sentence that stood here — "a `regtype` beside it does not decay at
> all" — is wrong. It was written from one probe over a `VALUES` row rather than over the family.
> Measured on a real column of each type, `pg_typeof(min(t))` is `oid` for `regtype`, `regproc` and
> `regclass` alike, and so is `max`'s: none of the three has a `min` of its own, so the aggregate
> PostgreSQL resolves is `min(oid)` and the argument is coerced to reach it. The **value** decays
> with the declared type — `min('int4'::regtype)` is `23`, not `integer` — which is the half a
> declared-type-only fix leaves wrong. An *array* of any of them does not decay, because an array
> has a `min` of its own. `tests/captures/pg19_reg_class.txt` is the measurement, and
> `tests/reg_class.rs` is the test; the rule now lives in `exec::aggregate` as one arm over all
> three. The lesson is [measure the whole list](0075-the-oracle-captures-live-in-the-repository.md):
> a rule read off one member of a family is a guess about the rest.

**3. A comparison reads an unadorned literal as an `oid`; an assignment resolves it as a name.**
`WHERE typinput = 'array_in'` is `22P02 invalid input syntax for type oid: "array_in"` on a real
server, because `=` over a `regproc` is `oideq` and the `unknown` literal goes to `oidin`. The
three forms that answer are `= 'array_in'::regproc`, `::text = 'array_in'` and `= 750`.

**4. `regproc` and `oid` are one representation.** `pg_cast` has 24→26 and 26→24, both implicit and
both method `b` — a reinterpretation. So `typinput::oid` takes the oid rather than printing the
name and reading it back.

**The oid table is measured, not derived.** `value::reg_proc` holds every input function name
`pg_catalog::typinput` can return with the oid a real server gives it — 46 rows, one query. Four
more (`citextin`, `hstore_in`, `lquery_in`, `ltree_in`) carry this node's own oids and say so: an
extension's functions are allocated at `CREATE EXTENSION`, so they differ between two databases
that both have the extension and there is no number to record.
`tests/reg_proc.rs::every_typinput_resolves_to_a_function` walks the catalog and fails on a name
the table does not know — it found `regprocin` itself the first time it ran, because `regproc` is
a type this node now has and its own `typinput` is itself.

## Consequences

* **`tests/array_delimiter.rs` was writing a statement a real server refuses.** Its three queries
  read `WHERE typinput = 'array_in'`, which passed here only because the column was `text` — a test
  passing on the mechanism it was not testing. They are `::regproc` now, which is the form a real
  server answers, and they reddened the moment the column was declared its real type.
* **`<column>::oid` used to be `0A000`.** `oid` reaches the lowering as a custom type name, so
  `::oid` always routed to the literal-folding path and a column had nothing to fold. It lowers to
  an ordinary cast when the operand is not a literal — a gap ADR 0097 left and this unit found.
* **A `regtype` cast to a number was a text round trip too**, and it is the oid now. Nothing had
  asked, because `regtype`'s printed form is usually a name that does not parse as a number either.
* The comparison rule is in `exec::query::retype` rather than in `Literal::assign`, because
  assignment is the direction that resolves a name — the two are different questions about the same
  literal and the seam is what tells them apart.
