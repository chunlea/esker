# 0053 — A cast to a user-defined type is resolved once per statement

Status: accepted

## Context

[ADR 0050](0050-a-user-defined-type-is-a-value.md) decided how a user-defined type rides on a
**column**: the column's `ty` is what the value physically is — an `int2` holding the label's
position for an enum — and the type's identity rides beside it on the `ColumnDef`, "where the
catalog is already in scope".

`'happy'::mood` has no column. It is the same type and the same value, written in an expression,
and every part of ADR 0050's argument that leaned on the column being there has nothing to lean on.
Four statements in `tests/corpus/pg19_enum_value.txt` need it and none of them is exotic:

```sql
SELECT 'happy'::mood, pg_typeof('happy'::mood)   -- happy | mood
SELECT 'happy'::text::mood                       -- happy
SELECT 'sad'::mood < 'happy'::mood               -- t: 'sad' is declared first
SELECT 'angry'::mood                             -- 22P02 invalid input value for enum mood
```

The third is the one that decides the shape. `'sad' < 'happy'` as **text** is false and as an
**enum** is true, so a design that lowers the cast to its label and stops is not a smaller version
of the right answer — it is a wrong one. The value has to be the ordinal by the time two of them
are compared.

Today the whole family is `0A000 the type mood is not supported`, from `lower_type` refusing a name
it does not have. That is honest and it is a gap; what is *not* honest is the neighbouring
`'mood'::regtype`, which answers `42704 type "mood" does not exist` for a type somebody declared.

## Options

### 1. A `Datum` that carries the type

`Datum::Enum { oid, ordinal }`, so a value knows what it is wherever it goes. This is the option
ADR 0050 rejected for columns and the reasons have not changed: `Datum::column_type()` would have to
return a `ColumnType` it cannot name, every pair-wise `PartialEq` and `pg_cmp` arm grows a case, and
the row codec gains a type whose text depends on a catalog the codec must not have
(`esker-keys` is catalog-free, invariant 7).

### 2. A `plan::Expr` variant that survives into evaluation

`Expr::UserCast { name, operand, def }`. It says the thing exactly, and it costs a new arm in every
exhaustive match over `plan::Expr` — twenty sites across eleven files — plus a catalog read per row
unless the definition is attached before the plan is built, which is option 3 with extra steps.

### 3. A `CatalogFunc` that is resolved before the plan is built, and never survives it

`CatalogFunc::UserCast`, carrying the type's name and the operand, replaced by
`Executor::resolve_regclass`'s pass with the value it stands for. This is what
`CatalogFunc::RegClass` already is: a cast that needs the catalog, read **once per statement**
rather than once per row, whose evaluator arm is an `Internal` error because reaching it means the
resolution was skipped.

## Decision

**Option 3.** A cast to a user-defined type is a `CatalogFunc` that the statement-level resolution
pass replaces, for the reason `::regclass` is one: the catalog answer is the same for every row, and
reading it per row is the cost trap `08ff6a2` already paid for once.

What the pass replaces it *with* depends on where the cast sits, and that is not a special case —
it is what an enum is:

- **In a projection**, on its own, it becomes the **label** — `SELECT 'happy'::mood` is `happy`,
  which is that value's output function.
- **Anywhere else** — a comparison, an assignment, a `WHERE` — it becomes the **ordinal**, so
  `'sad'::mood < 'happy'::mood` is `1 < 3` and is `t`, and `current_mood = 'happy'::mood` compares
  the number the row holds.

A label that the type does not have is `22P02 invalid input value for enum mood: "angry"` at
resolution, which is where the catalog is; a name that is no type at all keeps the `0A000` it has
today, because the operand may still be a type this node has not built.

## Consequences

- `SELECT 'happy'::mood` reports its type as `text` where a real server says `mood`. The value is
  right and the declared type is the standing catalog trade this crate makes for `regtype` and
  `name` — declared in `tests/enum_value.rs`, not silent.
- Nothing changes about storage. The ordinal is what ADR 0050 stores and this ADR does not touch it.
- `'mood'::regtype` is **not** answered by this decision. A real server's `regtype` is an oid that
  *prints* as a name, so `SELECT 'mood'::regtype` and `WHERE enumtypid = 'mood'::regtype` want two
  different things from one value; `::regclass` took the oid side and
  `tests/regclass_name.rs::the_forward_cast_prints_its_oid_where_a_real_server_prints_the_name`
  pins what that cost. Doing it properly is a `regtype` **value** that carries both halves, which
  is a type-surface unit of its own.
- A cast to a **range** or **composite** type stays `0A000` naming the kind, as
  `exec::ddl::resolve_user_type` already answers for a column of one.
