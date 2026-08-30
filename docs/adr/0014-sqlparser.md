# ADR 0014 — `sqlparser` is the one large dependency, without its default features

Status: accepted (phase 6a)
Date: 2026-08-30
Context: `CLAUDE.md` ("Dependency policy", the deferred `sqlparser` decision); `docs/adr/0003-dependencies.md`;
`docs/DESIGN.md` §13; `prompts/06-sql-serverless.md` §6a.2; `docs/plans/phase-6a.md` §1

## Context

Esker writes its own formats. `CLAUDE.md` names exactly one expected exception to that — a SQL
parser — and requires this ADR before the dependency is added, "stating what a replacement would
cost". Phase 6a is where the bill comes due, and it arrives alongside a compatibility directive that
changes the arithmetic: **the target is 100% PostgreSQL 19 syntax acceptance** (`docs/plans/phase-6a.md`
§1, contract C1). Not a subset that grows — every valid PG-19 statement parses from the first commit,
and anything the parser cannot handle is a tracked gap rather than a silent rejection.

That directive is what settles the decision, because it is not a statement about how much SQL we
want to execute. We execute a dozen statement forms. It is a statement about how much SQL we must
*recognise*, and PostgreSQL's grammar is not a dozen forms — it is a 25,000-line Bison grammar with
per-statement quirks that exist because a release in 2004 shipped them.

## Decision

Add **`sqlparser` 0.62.0** (newest published, pinned in `[workspace.dependencies]`) with
**`default-features = false, features = ["std"]`**, PostgreSQL dialect. It is the only crate on the
runtime graph that is not either written here or on the phase-0 allowlist.

Disabling the default features is not a tidiness preference; it is the difference between the
dependency being admissible and being banned outright. Measured on this machine, 2026-08-30:

| Feature set | Crates added | Verdict |
|---|---|---|
| `default-features = false, features = ["std"]` | 2 — `sqlparser`, `log` | admissible; runtime graph 22 → **24 of 40** |
| `default` ( = `std` + `recursive-protection`) | 20 — including **`cc` 1.4.4** and **`psm` 0.1.32** | banned by `deny.toml` and by `CLAUDE.md`'s pure-Rust rule |

`recursive-protection` pulls `recursive` → `stacker` → `psm`, and `psm` compiles hand-written
assembly through `cc`. `deny.toml` denies `cc` by name and `crates/esker-cli/tests/dep_budget.rs`
denies it by pattern, so the default feature set fails the gate twice over. It is worth recording
that the *reason* it fails is a good one: the crate is trying to grow the stack, which is a thing
`libc` and assembly do.

### The consequence we inherit, and how it is paid for

Turning that feature off means we own the problem it solves. `sqlparser` is a recursive-descent
parser; without protection, `SELECT ((((…1…))))` nested deeply enough overflows the stack, and a
stack overflow on user input is an abort — which violates `CLAUDE.md` invariant 9 ("never panic on
user input") in the least recoverable way available.

So `esker-sql/src/parse.rs` carries a **pre-parse depth guard**: a token-level nesting count,
refusing anything past a documented limit before the parser is ever entered. The error it returns is
SQLSTATE **`54001 statement_too_complex`** — which is precisely what PostgreSQL raises when
`max_stack_depth` is exceeded. The guard is therefore not a deviation from PostgreSQL that we
tolerate; it is PostgreSQL's own behaviour, arrived at from the other direction. It is tested with a
generated 10,000-deep expression.

## What a replacement would cost

The question `CLAUDE.md` requires answering. Three replacements exist, and the compatibility
directive prices all of them.

**1. Write the parser ourselves.** This is what we do everywhere else, and it is what we would do if
the target were "a useful subset of SQL". It is not. Against a 100%-PG-19-syntax contract the work
is not a parser, it is a re-implementation of `src/backend/parser/gram.y`: lexer with PG's dollar
quoting, `E''`/`U&''` strings, operator-precedence rules that PG itself documents as irregular, and
roughly 200 statement forms of which we execute twelve. Estimate: **8,000–15,000 lines and several
weeks**, against 3,000 lines for the entire rest of this crate — and it would be *permanently*
behind, since PG 19 adds grammar every year. Worse, the failure mode is silent: a subtly wrong
precedence rule returns a wrong answer rather than an error. This is the option the exception exists
to avoid, and the compatibility directive makes it not merely expensive but unachievable at quality.

**2. `pg_query.rs` — embed PostgreSQL's actual parser.** Honesty requires naming this, because it is
the *only* option that would make contract C1 exact rather than best-effort: it vendors PostgreSQL's
real `gram.y`, so what it accepts is by construction what PostgreSQL accepts. It is disqualified,
and not on a technicality — it compiles a large C library, which is the single thing this project's
dependency policy exists to forbid. Recorded here so a future reader knows the trade was seen and
made deliberately: **we accepted a best-effort C1 with a tracked gap register (`docs/plans/phase-6a.md`
§9) in exchange for staying pure Rust.** If that trade is ever reversed, this is the ADR to
supersede.

**3. Swap `sqlparser` for another Rust crate.** The cost is bounded and low, which is the real
argument for taking the dependency at all. `sqlparser`'s AST is used in exactly one module —
`parse.rs` lowers it into our own `plan::Logical` types immediately, and nothing downstream of that
module mentions a `sqlparser` type. Replacing it is rewriting one file. The rule is enforced by
the crate boundary: `sqlparser` may not be named outside `parse.rs`.

## Consequences

- The runtime graph goes 22 → 24 of the 40-crate budget. No budget change, so no ADR 0003 amendment.
- `log` joins the graph transitively. Nothing in Esker logs through it (`tracing` is the facade);
  it is `sqlparser`'s internal diagnostics and stays unwired.
- The pin is exact and lives in `[workspace.dependencies]`. Upgrades are deliberate: a new
  `sqlparser` release is a chance to close rows in the §9 gap register, and the syntax corpus is the
  test that says whether it did.
- **Containment is a rule, not a habit.** `sqlparser` types stop at `parse.rs`. A `use sqlparser::`
  anywhere else in the crate is a review failure — it is what turns a one-file replacement into a
  rewrite.
- We are responsible for the recursion depth the crate would otherwise have handled, and for
  noticing if a future `sqlparser` version makes `recursive-protection` non-optional.
