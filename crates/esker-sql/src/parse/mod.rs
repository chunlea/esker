//! The parser boundary, and the stack this crate has to protect itself.
//!
//! This module and its children are the only place in the crate where a `sqlparser` type may be
//! named (ADR 0014, as amended: containment is one *module*, not one file). `tests/containment.rs`
//! reads the source and fails the build if that stops being true, so the rule costs a test rather
//! than a reviewer's attention.
//!
//! Three jobs, and they are separate on purpose.
//!
//! The first is **containment**. `sqlparser` is the one large dependency
//! (`docs/adr/0014-sqlparser.md`), and the whole argument for taking it is that replacing it means
//! rewriting one file. That is only true while this is the one file: a `use sqlparser::` anywhere
//! else in the crate is a review failure.
//!
//! The second is **the stack**. The crate is built without `sqlparser`'s `recursive-protection`
//! feature, because that feature pulls `psm`, which compiles assembly, which `deny.toml` bans. So
//! the protection it would have provided is ours to write, and it has to be written *before* the
//! parser is entered, because a recursive-descent parser that has already overflowed the stack
//! cannot report anything — it aborts the process, which is `CLAUDE.md` invariant 9 broken in the
//! least recoverable way there is.
//!
//! [`nesting_depth`] is that guard: a scan that counts how deep the statement nests, and refuses
//! past [`MAX_NESTING_DEPTH`] with SQLSTATE `54001`. PostgreSQL raises the same condition when
//! `max_stack_depth` is exceeded, so the guard is a parity behaviour rather than a deviation.
//!
//! The scan has to understand PostgreSQL's lexical structure — quoting, dollar quoting, nested
//! block comments — for one reason that is easy to miss: a `(` inside a string literal is not
//! nesting, and counting it would make us reject a statement PostgreSQL accepts, which is contract
//! C1 broken (`docs/plans/phase-6a.md` §1). The guard may only ever err towards accepting.
//!
//! The third is **lowering**, in `src/parse/lower.rs`: the parser's tree into `crate::plan`, which
//! is where the AST stops travelling. It is a child module rather than more of this file because
//! it is a different job with a different reason to change, and because together they were 1851
//! lines. The reference is textual on purpose — the module is private, and it has to stay that way
//! or its signatures would put `sqlparser` types in this crate's public API, which is the one thing
//! ADR 0014 exists to prevent.

mod lower;

use sqlparser::ast::{ObjectType, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};

use crate::error::{Result, SqlError};
use crate::plan;

/// How deep a statement may nest before it is refused with SQLSTATE `54001`.
///
/// Chosen to be the same order as PostgreSQL's own effective limit under the default 2 MB
/// `max_stack_depth`, so that the statements Esker refuses are very nearly the statements
/// PostgreSQL refuses — contract C1 tolerates a limit, but not a *low* one.
pub const MAX_NESTING_DEPTH: usize = 1_000;

/// Stack consumed by the most expensive nesting level `sqlparser` has, with margin.
///
/// Measured on this project's own dependency rather than assumed, because the number decides
/// whether the guard works. A nested subquery -- `SELECT * FROM (SELECT * FROM (...))` -- is the
/// worst construct by a wide margin: about **34 KiB** per level in a release build and about
/// **205 KiB** in a debug one, against roughly 6 KiB per nested parenthesis. Debug frames are five
/// times the size, so a single constant would be either unsafe when tests run or absurdly
/// pessimistic when the server does; the profile is part of the measurement.
///
/// The values below round the measurement up by half again, so the two derived constants inherit
/// that margin instead of each needing their own.
const STACK_PER_NESTING_LEVEL: usize = if cfg!(debug_assertions) {
    320 * 1024
} else {
    64 * 1024
};

/// How much of the caller's own stack a parse may use before it is moved off it.
///
/// One mebibyte of the 2 MiB a `tokio` blocking thread gets. Parsing happens near the base of a
/// session's stack -- the session loop calls it almost directly -- so the parser's own consumption
/// is what this bounds.
const INLINE_STACK_BUDGET: usize = 1024 * 1024;

/// Below this depth a statement is parsed on the caller's own stack: 16 levels in release, 3 in
/// debug. Ordinary statements are far below either and never leave the caller's thread.
const INLINE_PARSE_DEPTH: usize = INLINE_STACK_BUDGET / STACK_PER_NESTING_LEVEL;

/// The stack given to the thread that parses a statement deeper than [`INLINE_PARSE_DEPTH`].
///
/// Enough for [`MAX_NESTING_DEPTH`] levels of the worst construct, plus slack for the frames that
/// are not per-level: 68 MiB in release, 324 MiB in debug. That is reserved address space and not
/// committed memory, so a thread that does not descend does not pay for it.
/// `the_deepest_admissible_statement_parses` is the test that keeps this honest -- it parses
/// nested subqueries at exactly [`MAX_NESTING_DEPTH`], which needed 256 MiB when measured directly.
const DEEP_PARSE_STACK_BYTES: usize = MAX_NESTING_DEPTH * STACK_PER_NESTING_LEVEL + 4 * 1024 * 1024;

/// The recursion limit handed to `sqlparser` itself.
///
/// Its own default is **50**, which rejects `SELECT ((((...1...))))` at 51 parentheses with a parser
/// error. PostgreSQL accepts that statement, so leaving the default in place would break contract
/// C1 on the first deeply-parenthesised query a client sent. It is raised above anything
/// [`MAX_NESTING_DEPTH`] admits so that our guard is the one that speaks; if it fires anyway, it is
/// reported as `54001` and not as a syntax error, because "too complex" is not "malformed".
const PARSER_RECURSION_LIMIT: usize = MAX_NESTING_DEPTH * 4;

/// What kind of statement this is, for the executor to dispatch on and for contract C2 to name.
///
/// The variants are the statements phase 6a executes; everything else is [`StatementClass::Other`]
/// carrying the feature name that goes into the `0A000` message. That split is the contract: the
/// set we execute grows, the set we mishandle is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementClass {
    /// `SELECT`, and anything else that is a query.
    Query,
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
    /// `CREATE TABLE`.
    CreateTable,
    /// `DROP TABLE`.
    DropTable,
    /// `CREATE INDEX`.
    CreateIndex,
    /// `DROP INDEX`.
    DropIndex,
    /// `BEGIN` / `START TRANSACTION`.
    Begin,
    /// `COMMIT`, and `END` used as its synonym.
    Commit,
    /// `ROLLBACK`.
    Rollback,
    /// `SET CONSTRAINTS { ALL | name [, …] } { DEFERRED | IMMEDIATE }`.
    ///
    /// **`sqlparser` 0.62.0 cannot read it** — it takes `SET` and then wants `=` or `TO` — which
    /// makes this a contract C1 gap and not a syntax question, exactly as `DROP INDEX
    /// CONCURRENTLY` is. So the source is rewritten to a statement the parser accepts and what it
    /// said is carried here, which is the mechanism this module already has for that.
    SetConstraints {
        /// The constraints named, or **empty for `ALL`** — which is not "no constraints" but
        /// "every deferrable one", and the two are told apart by the mode's own rules rather than
        /// by a flag (`crate::exec::deferred`).
        names: Vec<String>,
        /// `DEFERRED`, as against `IMMEDIATE`.
        deferred: bool,
    },
    /// `SAVEPOINT <name>`, carrying the name.
    Savepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] <name>`, carrying the name.
    ///
    /// **A separate class from [`StatementClass::Rollback`], and the reason is a wrong answer.**
    /// `sqlparser` puts the two in one variant with an `Option<Ident>`, so classifying on the
    /// variant alone made `ROLLBACK TO s` end the whole block — a statement PostgreSQL accepts,
    /// answered with a `ROLLBACK` tag, and the user's other work gone with no error to say so.
    RollbackTo(String),
    /// `RELEASE [SAVEPOINT] <name>`, carrying the name.
    Release(String),
    /// `EXPLAIN`.
    Explain,
    /// Parsed, not executed. The string is the feature name for the `0A000` message, phrased the
    /// way PostgreSQL phrases it — the construct, never the module that refused it.
    Other(String),
}

impl StatementClass {
    /// The name that goes into a `feature_not_supported` message, or `None` when the statement is
    /// one this crate executes.
    #[must_use]
    pub fn unsupported_feature(&self) -> Option<&str> {
        match self {
            StatementClass::Other(feature) => Some(feature),
            _ => None,
        }
    }
}

/// One parsed statement, with its class, and no `sqlparser` type visible from outside.
///
/// This is what keeps ADR 0014's containment rule true in practice. The rest of the crate needs to
/// *hold* parsed statements — the session passes them to the executor — and if it held
/// `sqlparser::ast::Statement` to do it, the promise that replacing the dependency means rewriting
/// one file would already be false. So the AST stays inside and everything outside works with the
/// class, the rendering, and (from unit 6) the lowered plan.
#[derive(Debug, Clone)]
pub struct Parsed {
    statement: Statement,
    class: StatementClass,
    /// `CONCURRENTLY` on a `DROP INDEX`, which the parser cannot carry.
    ///
    /// `sqlparser` 0.62.0's `Statement::Drop` has no field for it, so the statement does not parse
    /// at all — the keyword is where the parser stops. PostgreSQL 19 **accepts** it, which makes it
    /// a contract C1 gap rather than a syntax question, and the fix is the mechanism this module
    /// already has for a statement the parser cannot read: rewrite the source and remember what was
    /// taken out ([`strip_drop_index_concurrently`]).
    ///
    /// Carried here rather than inferred later because the alternative is worse than a field: a
    /// rewrite that dropped the word silently would give the **blocking** drop to somebody who
    /// asked for the concurrent one, which is precisely the class of failure `crate::plan`'s
    /// lowering exists to make impossible.
    concurrently: bool,
    /// The `EXCLUDE` constraints cut out of a `CREATE TABLE` so it would parse.
    ///
    /// `sqlparser` 0.62.0 cannot read one, so the statement is rewritten without them and the
    /// clause texts travel here — the same arrangement `concurrently` uses for a keyword the
    /// parser cannot carry ([`strip_exclude_constraints`]).
    exclude: Vec<String>,
}

impl Parsed {
    /// What kind of statement this is.
    #[must_use]
    pub fn class(&self) -> &StatementClass {
        &self.class
    }

    /// Whether this `BEGIN` asked for a **read-only** transaction.
    ///
    /// PostgreSQL's `BEGIN READ ONLY` refuses every write in the block with `25006`, and this node
    /// used to parse the words and ignore them — a clause the user wrote and the server did not
    /// honour, which is the defect class `crate::plan`'s lowering exists to prevent. It was
    /// invisible because `BEGIN` never reaches the lowering at all: transaction control belongs to
    /// the session, so the mode had to be read here.
    ///
    /// `READ WRITE` is the default and says nothing, so it is not carried.
    #[must_use]
    pub fn begins_read_only(&self) -> bool {
        use sqlparser::ast::{TransactionAccessMode, TransactionMode};

        matches!(&self.statement, Statement::StartTransaction { modes, .. }
        if modes.iter().any(|mode| {
            matches!(
                mode,
                TransactionMode::AccessMode(TransactionAccessMode::ReadOnly)
            )
        }))
    }

    /// Whether a `DROP INDEX` asked for `CONCURRENTLY`, which is stripped from the source
    /// before `sqlparser` sees it and remembered rather than inferred.
    #[must_use]
    pub fn is_concurrently(&self) -> bool {
        self.concurrently
    }

    /// The `EXCLUDE` constraint clauses this statement was rewritten without.
    #[must_use]
    pub fn exclude_constraints(&self) -> &[String] {
        &self.exclude
    }

    /// The statement rendered back to SQL, for `EXPLAIN` output and diagnostics.
    #[must_use]
    pub fn rendered(&self) -> String {
        self.statement.to_string()
    }
}

/// Parses a query string into statements, each classified.
///
/// A simple-query message may carry several statements in one string, which is why this returns a
/// list and why the session runs them in order and stops at the first failure.
pub fn parse_statements(sql: &str) -> Result<Vec<Parsed>> {
    // `parse` does the rewriting; this only has to *notice*, because the keyword it removes is a
    // fact about the statement that the parsed tree cannot carry.
    let scanned = scan(sql);
    let concurrently = strip_drop_index_concurrently(sql, &scanned).is_some();
    let constraints = set_constraints(sql, &scanned);
    // The clause texts, taken the same way `parse` takes them — this only has to *notice*, because
    // what was removed is a fact about the statement that the parsed tree cannot carry.
    let exclude = strip_exclude_constraints(sql, &scanned)
        .map(|(_, clauses)| clauses)
        .unwrap_or_default();
    Ok(parse(sql)?
        .into_iter()
        .map(|statement| {
            // The rewritten source parses as something harmless; what the user wrote is this.
            let class = constraints.clone().unwrap_or_else(|| classify(&statement));
            Parsed {
                statement,
                class,
                concurrently,
                exclude: exclude.clone(),
            }
        })
        .collect())
}

/// Every `EXCLUDE` table constraint cut out of a `CREATE TABLE`, and the statement without them.
///
/// **`sqlparser` 0.62.0 cannot read an `EXCLUDE` constraint at all** — its only `EXCLUDE` keywords
/// are `UNPIVOT`'s `EXCLUDE NULLS` and a window frame's — so statement 777 of
/// `postgresql_specific_schema.rb` is a *syntax error* from the parser rather than a clause the
/// lowering declines. That is contract C1's shortfall, and the fix is this module's standing one:
/// rewrite the source so it parses and carry what was taken out beside the tree
/// ([`strip_drop_index_concurrently`] does the same for one keyword).
///
/// What comes back is the clause text of each constraint, from `CONSTRAINT` or `EXCLUDE` through
/// its last balanced parenthesis and any `WHERE`/`DEFERRABLE` after it — enough for
/// [`crate::parse::lower`] to re-parse the pieces with the real parser.
///
/// **Parenthesis-balanced rather than comma-split**, because the clause is full of commas:
/// `EXCLUDE USING gist (daterange(start_date, end_date) WITH &&)` has three, and a split on the
/// first would cut the constraint in half.
fn strip_exclude_constraints(sql: &str, scanned: &Scan<'_>) -> Option<(String, Vec<String>)> {
    let [first, second, ..] = scanned.words.as_slice() else {
        return None;
    };
    // Only inside a `CREATE TABLE`: `EXCLUDE` is an ordinary word elsewhere, and a window frame's
    // `EXCLUDE TIES` must not be mistaken for one.
    if !first.eq_ignore_ascii_case("CREATE") {
        return None;
    }
    if !second.eq_ignore_ascii_case("TABLE") && !second.eq_ignore_ascii_case("UNLOGGED") {
        return None;
    }
    let upper = sql.to_ascii_uppercase();
    if !upper.contains("EXCLUDE") {
        return None;
    }
    let bytes = sql.as_bytes();
    let mut clauses = Vec::new();
    let mut kept = String::with_capacity(sql.len());
    let mut at = 0;
    while let Some(found) = find_exclude_clause(&upper, bytes, at) {
        let (start, end) = found;
        kept.push_str(sql.get(at..start)?);
        clauses.push(sql.get(start..end)?.trim().to_owned());
        // The comma that separated it from the next item goes too, or the column list would keep
        // an empty slot: `(a int, CONSTRAINT c EXCLUDE …, b int)` must become `(a int, b int)`.
        let mut after = end;
        while bytes.get(after).is_some_and(u8::is_ascii_whitespace) {
            after += 1;
        }
        if bytes.get(after) == Some(&b',') {
            after += 1;
        } else {
            // It was the last item, so the comma **before** it is the one to drop.
            let trimmed = kept.trim_end();
            if let Some(without) = trimmed.strip_suffix(',') {
                kept.truncate(without.len());
            }
        }
        at = after;
    }
    if clauses.is_empty() {
        return None;
    }
    kept.push_str(sql.get(at..)?);
    Some((kept, clauses))
}

/// The last `CONSTRAINT` **keyword** in `prefix`, ignoring the word inside an identifier.
///
/// A plain `rfind` is wrong here and the statement this unit exists for is what proves it:
/// `CONSTRAINT "test_exclusion_constraints_date_overlap"` contains the letters `constraint` inside
/// its own quoted name, later than the keyword, so backing up to the last match cut the name in
/// half and left an unterminated quote. Both boundaries are checked and quoted text is skipped.
fn last_constraint_keyword(prefix: &str) -> Option<usize> {
    const KEYWORD: &str = "CONSTRAINT";
    let bytes = prefix.as_bytes();
    let word = |byte: Option<&u8>| byte.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    let (mut found, mut quoted, mut literal, mut at) = (None, false, false, 0);
    while at < bytes.len() {
        match bytes[at] {
            b'"' if !literal => quoted = !quoted,
            b'\'' if !quoted => literal = !literal,
            _ if quoted || literal => {}
            _ if prefix.get(at..)?.starts_with(KEYWORD)
                && !word(at.checked_sub(1).and_then(|before| bytes.get(before)))
                && !word(bytes.get(at + KEYWORD.len())) =>
            {
                found = Some(at);
                at += KEYWORD.len() - 1;
            }
            _ => {}
        }
        at += 1;
    }
    found
}

/// The byte range of one `EXCLUDE` table constraint, starting at or after `from`.
///
/// The clause runs from its `CONSTRAINT` keyword — or from `EXCLUDE` when it has no name — to the
/// end of the last thing that belongs to it: the key list, then an optional `WHERE (…)` and an
/// optional `DEFERRABLE …`. Anything after that is the next table item or the closing paren.
fn find_exclude_clause(upper: &str, bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let keyword = upper.get(from..)?.find("EXCLUDE")? + from;
    // `EXCLUDE USING <am> (…)` or a bare `EXCLUDE (…)`, which is what a table constraint writes —
    // a window frame's `EXCLUDE CURRENT ROW` and `UNPIVOT`'s `EXCLUDE NULLS` write neither.
    //
    // **The bare form is not a shortcut for `USING gist`.** It defaults to btree and a real server
    // then refuses it; the point of parsing it is to reach that refusal rather than a syntax error
    // (`parse_exclude_constraint`).
    let after_keyword = keyword + "EXCLUDE".len();
    let after = upper.get(after_keyword..)?.trim_start();
    if !after.starts_with("USING") && !after.starts_with('(') {
        return None;
    }
    // Back up over a `CONSTRAINT <name>` that names it, so the name is cut out with the clause.
    let start = last_constraint_keyword(upper.get(..keyword)?)
        .filter(|&at| {
            // Only if nothing but the name lies between: a `CONSTRAINT` belonging to an earlier
            // item has a comma after it.
            !upper
                .get(at..keyword)
                .is_some_and(|between| between.contains(','))
        })
        .unwrap_or(keyword);
    // The key list, balanced.
    let mut at = upper.get(after_keyword..)?.find('(')? + after_keyword;
    let mut end = balanced_end(bytes, at)?;
    // `WHERE (…)`, which is one more balanced group.
    let rest = upper.get(end..)?;
    let trimmed = rest.trim_start();
    if trimmed.starts_with("WHERE") {
        let where_at = end + (rest.len() - trimmed.len());
        at = upper.get(where_at..)?.find('(')? + where_at;
        end = balanced_end(bytes, at)?;
    }
    // `[NOT] DEFERRABLE [INITIALLY IMMEDIATE|DEFERRED]`, which is words rather than parentheses.
    for tail in [
        "NOT DEFERRABLE",
        "DEFERRABLE INITIALLY IMMEDIATE",
        "DEFERRABLE INITIALLY DEFERRED",
        "DEFERRABLE",
    ] {
        let rest = upper.get(end..)?;
        let trimmed = rest.trim_start();
        if trimmed.starts_with(tail) {
            end += (rest.len() - trimmed.len()) + tail.len();
            break;
        }
    }
    Some((start, end))
}

/// The byte just past the parenthesis that closes the one at `open`.
fn balanced_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    let mut quoted = false;
    for (at, &byte) in bytes.iter().enumerate().skip(open) {
        // A parenthesis inside a string literal is a character, not a nesting level.
        if byte == b'\'' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// `DROP INDEX CONCURRENTLY x` with the keyword taken out, or `None` if that is not what this is.
///
/// The same shape as [`rewrite_synonym`] and for the same reason: a statement PostgreSQL 19 accepts
/// and `sqlparser` cannot read is a **gap in the parser**, not a malformed statement, and this
/// module's job is to keep that distinction from reaching a user. `CREATE INDEX CONCURRENTLY` needs
/// none of this — `sqlparser`'s `CreateIndex` has the flag — which is the whole reason only one
/// direction is rewritten here.
///
/// Deliberately narrow: only when the statement *begins* `DROP INDEX CONCURRENTLY`, so it cannot
/// touch a table called `concurrently` or the word inside a string.
fn strip_drop_index_concurrently(sql: &str, scanned: &Scan<'_>) -> Option<String> {
    let [first, second, third, ..] = scanned.words.as_slice() else {
        return None;
    };
    if !first.eq_ignore_ascii_case("DROP")
        || !second.eq_ignore_ascii_case("INDEX")
        || !third.eq_ignore_ascii_case("CONCURRENTLY")
    {
        return None;
    }
    // Case-insensitively, on the first occurrence, which the check above has pinned to the third
    // word of the statement.
    let at = sql.to_ascii_uppercase().find("CONCURRENTLY")?;
    let mut rewritten = String::with_capacity(sql.len());
    rewritten.push_str(sql.get(..at)?);
    rewritten.push_str(sql.get(at + "CONCURRENTLY".len()..)?);
    Some(rewritten)
}

/// One `EXCLUDE` clause, parsed from the text [`strip_exclude_constraints`] cut out.
///
/// The clause never reached `sqlparser`, so its pieces are handed back to it one at a time: the
/// key expression and the `WHERE` predicate are ordinary expressions and go through the real
/// parser, and only the keywords around them are read here. That keeps the hand-written part to
/// the shape of the clause and leaves every expression inside it to the parser that knows them.
pub(crate) fn parse_exclude_constraint(
    clause: &str,
    table: &str,
) -> Result<crate::catalog::ExcludeDef> {
    let upper = clause.to_ascii_uppercase();
    let bad = || SqlError::unsupported(format!("the EXCLUDE constraint {clause}"));

    // `CONSTRAINT "name"`, or a name PostgreSQL derives from the table and the key's columns.
    let named = upper.starts_with("CONSTRAINT");
    let exclude_at = upper.find("EXCLUDE").ok_or_else(bad)?;
    let given = named
        .then(|| clause.get("CONSTRAINT".len()..exclude_at))
        .flatten()
        .map(|name| {
            let name = name.trim();
            let unquoted = name.strip_prefix('"').and_then(|n| n.strip_suffix('"'));
            crate::catalog::fold_identifier(unquoted.unwrap_or(name), unquoted.is_some()).0
        });

    // `USING <method>`, **optional** — and its absence is not the same as `USING gist`. Written
    // without one the constraint gets a btree, which the check below then refuses; treating the
    // missing clause as a parse failure made that refusal a `0A000` naming the whole constraint
    // instead of the `42809` naming the operator, which is what a real server answers.
    let after = clause.get(exclude_at + "EXCLUDE".len()..).ok_or_else(bad)?;
    let after = after.trim_start();
    let method: String = after
        .get("USING".len()..)
        .filter(|_| after.to_ascii_uppercase().starts_with("USING"))
        .map(|rest| {
            rest.trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect()
        })
        .unwrap_or_default();

    // `(<expr> WITH <op>)`, whose parentheses the stripper already balanced.
    let open = clause.find('(').ok_or_else(bad)?;
    let close = matching_paren(clause.as_bytes(), open).ok_or_else(bad)?;
    let inside = clause.get(open + 1..close - 1).ok_or_else(bad)?;
    let with_at = inside
        .to_ascii_uppercase()
        .rfind(" WITH ")
        .ok_or_else(bad)?;
    let key = inside.get(..with_at).ok_or_else(bad)?.trim().to_owned();
    let operator = inside
        .get(with_at + " WITH ".len()..)
        .ok_or_else(bad)?
        .trim()
        .to_owned();
    // Through the real parser: a key this crate cannot read is a refusal here rather than a
    // surprise at the first insert.
    lower::parse_expr_text(&key)?;
    // **`USING gist` is load-bearing, not decoration.** Written without one, or with `USING btree`
    // spelled out, the constraint gets a btree — and `&&` is not in `range_ops`, which is what a
    // real server says, in those words, for both spellings. Measured: the two are indistinguishable
    // in the answer, which is how you can tell what the default was.
    if !method.eq_ignore_ascii_case("gist") {
        return Err(SqlError::ExclusionOperatorNotInFamily {
            operator: format!("{operator}(anyrange,anyrange)"),
            family: "range_ops".to_owned(),
        });
    }
    // `&&` is the operator this node enforces, through `crate::value::range`. Any other is a
    // refusal by name rather than a constraint that would admit a row a real server refuses.
    if operator != "&&" {
        return Err(SqlError::unsupported(format!(
            "an EXCLUDE constraint WITH {operator}"
        )));
    }

    // `WHERE (<predicate>)`, one more balanced group.
    let tail = clause.get(close..).ok_or_else(bad)?;
    let upper_tail = tail.to_ascii_uppercase();
    let predicate = match upper_tail.find("WHERE") {
        None => None,
        Some(where_at) => {
            let open = tail
                .get(where_at..)
                .ok_or_else(bad)?
                .find('(')
                .ok_or_else(bad)?
                + where_at;
            let close = matching_paren(tail.as_bytes(), open).ok_or_else(bad)?;
            let text = tail
                .get(open + 1..close - 1)
                .ok_or_else(bad)?
                .trim()
                .to_owned();
            lower::parse_expr_text(&text)?;
            Some(text)
        }
    };

    // `[NOT] DEFERRABLE [INITIALLY IMMEDIATE|DEFERRED]`. **`INITIALLY DEFERRED` is recorded, not
    // refused**: the check still runs at the statement, and holding it to `COMMIT` is a
    // transaction-layer unit that plugs in where this flag is read.
    let deferrable = upper_tail.contains("DEFERRABLE") && !upper_tail.contains("NOT DEFERRABLE");
    let deferred = deferrable && upper_tail.contains("INITIALLY DEFERRED");

    Ok(crate::catalog::ExcludeDef {
        name: given.unwrap_or_else(|| format!("{table}_excl")),
        key,
        operator,
        method,
        predicate,
        deferrable,
        deferred,
    })
}

/// The byte just past the parenthesis closing the one at `open`, for a clause already balanced.
fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    let mut quoted = false;
    for (at, &byte) in bytes.iter().enumerate().skip(open) {
        if byte == b'\'' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            continue;
        }
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// One column `DEFAULT`, parsed from text and folded against the column's type.
///
/// The seam exists because folding needs **both** halves and they are known in different places: a
/// plan is lowered without the catalog, so `ALTER COLUMN … SET DEFAULT 7` cannot know whether `7`
/// is going into an `int4` or a `text` until the executor has the column. `CREATE TABLE` has the
/// type in hand and folds where it lowers; this is the same function reached the other way round.
///
/// The text is re-parsed rather than carried as a tree, which costs one parse per `ALTER` and
/// keeps `plan::AlterTableAction` free of a `sqlparser` type — the same trade `CHECK` and a
/// generation expression already make.
pub(crate) fn fold_column_default(
    expr: &str,
    ty: crate::value::ColumnType,
) -> Result<(Option<crate::value::Datum>, Option<String>)> {
    let not_one = || SqlError::Internal("a stored default is not one expression".to_owned());
    let statements = parse(&format!("SELECT {expr}"))?;
    let [Statement::Query(query)] = statements.as_slice() else {
        return Err(not_one());
    };
    let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
        return Err(not_one());
    };
    match select.projection.as_slice() {
        [sqlparser::ast::SelectItem::UnnamedExpr(expr)] => lower::column_default(expr, ty),
        _ => Err(not_one()),
    }
}

/// One stored expression, parsed and lowered — a `CHECK`, an index predicate, or an index key.
///
/// It goes through the real parser rather than a second one: the text came from a statement this
/// parser accepted, so anything it will not read back is a bug here rather than in the catalog.
/// Wrapped in a `SELECT` because that is the smallest statement with an expression in it.
pub(crate) fn parse_stored_expr(expr: &str) -> Result<plan::Expr> {
    let not_one = || SqlError::Internal("a stored expression is not one expression".to_owned());
    let statements = parse_statements(&format!("SELECT {expr}"))?;
    let [parsed] = statements.as_slice() else {
        return Err(not_one());
    };
    let plan::Statement::Select(select) = parsed.lower()? else {
        return Err(not_one());
    };
    match select.projection.as_slice() {
        [plan::SelectItem::Expr { expr, .. }] => Ok(expr.clone()),
        _ => Err(not_one()),
    }
}

/// Parses one statement string into statements, guarding the stack first.
///
/// Contract C1 (`docs/plans/phase-6a.md` §1): a [`SqlError::Syntax`] from here for a statement
/// PostgreSQL 19 accepts is a bug, and belongs in the plan's gap register rather than in a user's
/// error log.
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let scanned = scan(sql);
    if scanned.max_depth > MAX_NESTING_DEPTH {
        return Err(SqlError::StatementTooComplex);
    }

    // A statement PostgreSQL defines as a synonym for one the parser does know, and one whose
    // *keyword* the parser cannot read. Both are source rewrites for the same reason: the statement
    // is valid PostgreSQL and the gap is the parser's, so the honest fix is to make it parse rather
    // than to report a syntax error about correct SQL (contract C1).
    let rewritten = rewrite_synonym(sql, &scanned)
        .or_else(|| strip_drop_index_concurrently(sql, &scanned))
        .or_else(|| strip_exclude_constraints(sql, &scanned).map(|(kept, _)| kept));
    let text = rewritten.as_deref().unwrap_or(sql);

    let parsed = if scanned.max_depth <= INLINE_PARSE_DEPTH {
        parse_inner(text)
    } else {
        parse_on_a_deep_stack(text)
    };

    match parsed {
        // Contract C2 over contract C1's shortfall. The parser could not read it; if PostgreSQL
        // could, then what happened is that Esker is missing a feature, and saying "syntax error"
        // about valid SQL is both untrue and unactionable. `recognize_unsupported` is what tells
        // the two apart, and a statement it does not recognise really is malformed.
        Err(SqlError::Syntax {
            message, position, ..
        }) => match recognize_unsupported(sql, &scanned.words) {
            Some(feature) => Err(SqlError::unsupported(feature)),
            // Not a feature gap and not valid PostgreSQL either — PostgreSQL 19 answers `42601`
            // for these too, so the code stays. What is added is a `HINT` naming the spelling that
            // works here, which turns a dead end into a redirect without inventing a syntax that
            // this node would accept and the oracle would reject.
            None => Err(SqlError::Syntax {
                message,
                position,
                hint: recognize_redirect(&scanned.words),
            }),
        },
        other => other,
    }
}

/// Parses on a thread sized for [`MAX_NESTING_DEPTH`].
///
/// A thread spawn costs tens of microseconds, which is nothing beside the distributed transaction
/// the statement is about to become, and only statements past [`INLINE_PARSE_DEPTH`] pay it.
fn parse_on_a_deep_stack(sql: &str) -> Result<Vec<Statement>> {
    let owned = sql.to_owned();
    let worker = std::thread::Builder::new()
        .name("esker-sql-parse".into())
        .stack_size(DEEP_PARSE_STACK_BYTES)
        .spawn(move || parse_inner(&owned))
        .map_err(|error| SqlError::Internal(format!("could not spawn a parser thread: {error}")))?;
    // A panic in the parser is a bug in the dependency, not something the client did. It is
    // reported as an internal error rather than allowed to unwind into the session, because
    // invariant 9 is about what a client can provoke and this must not become a dropped connection.
    worker
        .join()
        .map_err(|_| SqlError::Internal("the parser thread panicked".into()))?
}

fn parse_inner(sql: &str) -> Result<Vec<Statement>> {
    Parser::new(&PostgreSqlDialect {})
        .with_recursion_limit(PARSER_RECURSION_LIMIT)
        .try_with_sql(sql)
        .and_then(|mut parser| parser.parse_statements())
        .map_err(|error| match error {
            // Not malformed, just too deep: the same condition our own guard reports, and the one
            // PostgreSQL reports when `max_stack_depth` is exceeded.
            ParserError::RecursionLimitExceeded => SqlError::StatementTooComplex,
            other => SqlError::Syntax {
                message: other.to_string(),
                position: None,
                hint: None,
            },
        })
}

/// Classifies a parsed statement, naming the feature when it is one we do not execute.
#[must_use]
pub fn classify(statement: &Statement) -> StatementClass {
    match statement {
        Statement::Query(_) => StatementClass::Query,
        Statement::Insert(_) => StatementClass::Insert,
        Statement::Update(_) => StatementClass::Update,
        Statement::Delete(_) => StatementClass::Delete,
        Statement::CreateTable(_) => StatementClass::CreateTable,
        Statement::CreateIndex(_) => StatementClass::CreateIndex,
        Statement::Drop {
            object_type: ObjectType::Table,
            ..
        } => StatementClass::DropTable,
        Statement::Drop {
            object_type: ObjectType::Index,
            ..
        } => StatementClass::DropIndex,
        Statement::StartTransaction { .. } => StatementClass::Begin,
        Statement::Commit { .. } => StatementClass::Commit,
        // The `savepoint` field is what tells the two apart, and reading only the variant is how
        // `ROLLBACK TO s` used to end the whole transaction.
        Statement::Rollback {
            savepoint: Some(name),
            ..
        } => StatementClass::RollbackTo(
            crate::catalog::fold_identifier(&name.value, name.quote_style.is_some()).0,
        ),
        Statement::Rollback { .. } => StatementClass::Rollback,
        Statement::Savepoint { name } => StatementClass::Savepoint(
            crate::catalog::fold_identifier(&name.value, name.quote_style.is_some()).0,
        ),
        Statement::ReleaseSavepoint { name } => StatementClass::Release(
            crate::catalog::fold_identifier(&name.value, name.quote_style.is_some()).0,
        ),
        Statement::Explain { .. } | Statement::ExplainTable { .. } => StatementClass::Explain,
        other => StatementClass::Other(feature_name(other)),
    }
}

/// The feature name for a statement we do not execute.
///
/// Taken from the statement's own rendering rather than from its Rust variant, because the client
/// is owed the SQL construct it wrote — `CREATE VIEW`, `MERGE` — and not the shape of our AST. Two
/// leading keywords is what makes `CREATE VIEW` distinct from `CREATE TABLE` while leaving `MERGE`
/// alone.
fn feature_name(statement: &Statement) -> String {
    let rendered = statement.to_string();
    // `sqlparser` renders keywords in upper case and leaves identifiers in the case the user
    // wrote them, so "already upper case" selects the keywords without transforming anything.
    // Selecting rather than upper-casing is the point: `SAVEPOINT s` must not come back as
    // "SAVEPOINT S is not supported", which puts a name the user did not write into their log.
    let keywords: Vec<&str> = rendered
        .split_whitespace()
        .take_while(|word| {
            !word.is_empty() && word.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        })
        .collect();
    // Two keywords name almost every statement — `CREATE TABLE`, `DROP INDEX`, `SAVEPOINT`. The
    // exception is a **modifier** in front of the name: `CREATE OR REPLACE FUNCTION` spends three
    // words before reaching the noun, and stopping at two answered "CREATE OR is not supported",
    // which names no feature and leaves a user unable to tell which statement was declined.
    //
    // Widened here rather than everywhere, because the cap is what keeps an upper-case
    // *identifier* out of the message: `SAVEPOINT S` must not come back as the feature
    // "SAVEPOINT S". `OR REPLACE` cannot be an identifier in this position, so the four words
    // after it are keywords by construction.
    let take = if keywords.starts_with(&["CREATE", "OR", "REPLACE"]) {
        4
    } else {
        2
    };
    let words = keywords
        .into_iter()
        .take(take)
        .collect::<Vec<_>>()
        .join(" ");
    if words.is_empty() {
        "this statement".to_owned()
    } else {
        words
    }
}

/// A construct PostgreSQL 19 accepts that `sqlparser` cannot read, and the name to call it by.
///
/// This table is contract C2 applied to contract C1's shortfall. A statement PostgreSQL parses and
/// this crate's parser does not is a *missing feature*, not a malformed statement, and the client
/// is owed `0A000 feature_not_supported` naming the construct rather than `42601 syntax_error`
/// pointing at a keyword that is perfectly valid. `docs/plans/phase-6a.md` §9 is the register these
/// rows come from, and `tests/syntax_corpus.rs` is what proves the two still agree.
///
/// The table is consulted **only after a parse has already failed**, which is what makes a loose
/// pattern safe: it can only re-describe something that was going to be an error anyway, and
/// "BETWEEN SYMMETRIC is not supported" beats "syntax error at or near SYMMETRIC" even when the
/// statement was malformed for another reason too. What it must never do is swallow an ordinary
/// typo, which is why no row matches on a bare common keyword.
struct Unsupported {
    /// The construct, named as PostgreSQL names it.
    feature: &'static str,
    /// Words the statement must begin with; empty means no constraint.
    leading: &'static [&'static str],
    /// Words that must appear in this order somewhere; [`ANY`] matches any single word.
    contains: &'static [&'static str],
}

/// Matches any single word in an [`Unsupported::contains`] pattern.
const ANY: &str = "?";

/// Ordered most specific first: `CREATE USER MAPPING` must be tested before `CREATE USER`, or the
/// shorter row would claim the longer statement and name the feature wrongly.
/// What to write instead of `CHECKPOINT`, which the table below already refuses by name.
///
/// The redirect belongs on a `0A000` here rather than in [`recognize_redirect`], because
/// `CHECKPOINT` never reaches the parser: it is PostgreSQL's own statement, this node does not
/// force a WAL checkpoint, and the answer is contract C2's. What the answer was missing is that a
/// user who wrote it was almost certainly reaching for a *named* checkpoint, which exists here
/// under a spelling PostgreSQL parses (ADR 0021 Decision 3: the word must not be taken, because
/// PostgreSQL owns it for something else).
pub(crate) const CHECKPOINT_FEATURE: &str = "CHECKPOINT";

const UNSUPPORTED: &[Unsupported] = &[
    // --- Constructs inside statements this crate does execute. These are the rows a user of the
    // --- supported subset can actually reach, so they are tested first and named precisely.
    u("GROUP BY DISTINCT", &[], &["GROUP", "BY", "DISTINCT"]),
    u(
        "row-level locking with FOR KEY SHARE",
        &[],
        &["FOR", "KEY", "SHARE"],
    ),
    u(
        "row-level locking with FOR NO KEY UPDATE",
        &[],
        &["FOR", "NO", "KEY", "UPDATE"],
    ),
    // The *column-option* spelling only. The table-constraint and index spellings parse, so
    // they never reach the recognizer -- which is why matching the three bare words is safe.
    u(
        "UNIQUE NULLS NOT DISTINCT on a column",
        &[],
        &["NULLS", "NOT", "DISTINCT"],
    ),
    u("BETWEEN SYMMETRIC", &[], &["BETWEEN", "SYMMETRIC"]),
    u("TRIM(BOTH ...)", &[], &["TRIM", "BOTH"]),
    u("TRIM(LEADING ...)", &[], &["TRIM", "LEADING"]),
    u("TRIM(TRAILING ...)", &[], &["TRIM", "TRAILING"]),
    u(
        "a window frame EXCLUDE clause",
        &[],
        &["EXCLUDE", "CURRENT"],
    ),
    u("a window frame EXCLUDE clause", &[], &["EXCLUDE", "TIES"]),
    u("a window frame EXCLUDE clause", &[], &["EXCLUDE", "GROUP"]),
    u("a window frame EXCLUDE clause", &[], &["EXCLUDE", "NO"]),
    u("ROWS FROM", &[], &["ROWS", "FROM"]),
    u("a JSON wrapper clause", &[], &["WITH", "WRAPPER"]),
    u("a JSON wrapper clause", &[], &["WITHOUT", "WRAPPER"]),
    u("a JSON quotes clause", &[], &["OMIT", "QUOTES"]),
    u("a JSON quotes clause", &[], &["KEEP", "QUOTES"]),
    u("INSERT ... OVERRIDING", &[], &["OVERRIDING"]),
    u("MERGE ... THEN DO NOTHING", &["MERGE"], &["THEN", "DO"]),
    u(
        "an aliased JOIN ... USING clause",
        &["SELECT"],
        &["USING", ANY, "AS"],
    ),
    u(
        "the SEARCH clause of a recursive CTE",
        &[],
        &["SEARCH", "DEPTH"],
    ),
    u(
        "the SEARCH clause of a recursive CTE",
        &[],
        &["SEARCH", "BREADTH"],
    ),
    u(
        "the CYCLE clause of a recursive CTE",
        &["WITH", "RECURSIVE"],
        &["CYCLE"],
    ),
    // --- Transaction control ---
    u("PREPARE TRANSACTION", &["PREPARE", "TRANSACTION"], &[]),
    u("COMMIT PREPARED", &["COMMIT", "PREPARED"], &[]),
    u("ROLLBACK PREPARED", &["ROLLBACK", "PREPARED"], &[]),
    u("a DEFERRABLE transaction", &["BEGIN"], &["DEFERRABLE"]),
    u("a DEFERRABLE transaction", &["START"], &["DEFERRABLE"]),
    // --- Schema objects ---
    u(
        "ALTER TABLE ... ATTACH PARTITION",
        &[],
        &["ATTACH", "PARTITION"],
    ),
    u(
        "ALTER TABLE ... DETACH PARTITION",
        &[],
        &["DETACH", "PARTITION"],
    ),
    u("ALTER TABLE ... SET LOGGED", &[], &["SET", "LOGGED"]),
    u("ALTER TABLE ... SET UNLOGGED", &[], &["SET", "UNLOGGED"]),
    // `sqlparser` 0.62.0 reads four `ALTER TABLE` actions and PostgreSQL has some thirty
    // (G32-G39). None of the rest is executed here either, so each is named the way the lowering
    // names the ones that *do* parse -- a user gets one sentence for a construct whether the gap
    // is upstream or ours. `ALTER COLUMN` is one row on purpose: the parser reads four of its
    // dozen actions, none of which this crate runs, so the action makes no difference to the
    // answer. It comes first because `ALTER COLUMN a RESET (...)` would otherwise be claimed by
    // the table-level `RESET` row below and named for the wrong level.
    u(
        "ALTER TABLE ... ALTER COLUMN",
        &["ALTER", "TABLE"],
        &["ALTER", "COLUMN"],
    ),
    u(
        "ALTER TABLE ALL IN TABLESPACE",
        &["ALTER", "TABLE", "ALL"],
        &[],
    ),
    u(
        "ALTER TABLE ... SET SCHEMA",
        &["ALTER", "TABLE"],
        &["SET", "SCHEMA"],
    ),
    u(
        "ALTER TABLE ... SET TABLESPACE",
        &["ALTER", "TABLE"],
        &["SET", "TABLESPACE"],
    ),
    u(
        "ALTER TABLE ... SET ACCESS METHOD",
        &["ALTER", "TABLE"],
        &["SET", "ACCESS", "METHOD"],
    ),
    u(
        "ALTER TABLE ... SET WITHOUT CLUSTER",
        &["ALTER", "TABLE"],
        &["SET", "WITHOUT", "CLUSTER"],
    ),
    u(
        "ALTER TABLE ... SET WITHOUT OIDS",
        &["ALTER", "TABLE"],
        &["SET", "WITHOUT", "OIDS"],
    ),
    u("ALTER TABLE ... RESET", &["ALTER", "TABLE"], &["RESET"]),
    u(
        "ALTER TABLE ... CLUSTER ON",
        &["ALTER", "TABLE"],
        &["CLUSTER", "ON"],
    ),
    // Covers `NO INHERIT` too: it is the same feature, table inheritance, and naming the two
    // separately would say nothing a reader does not already see in their own statement.
    u("ALTER TABLE ... INHERIT", &["ALTER", "TABLE"], &["INHERIT"]),
    u(
        "ALTER TABLE ... OF a type",
        &["ALTER", "TABLE"],
        &[ANY, "OF"],
    ),
    u("CREATE UNLOGGED TABLE", &["CREATE", "UNLOGGED"], &[]),
    // **`CREATE TABLE` is not here**: an `EXCLUDE` constraint is cut out of the source and read on
    // its own (`strip_exclude_constraints`). What stays a refusal is `ALTER TABLE ... ADD
    // CONSTRAINT ... EXCLUDE`, which has no such path.
    u("an EXCLUDE constraint", &["ALTER", "TABLE"], &["EXCLUDE"]),
    u(
        "CREATE TABLE ... LIKE",
        &["CREATE", "TABLE"],
        &["INCLUDING"],
    ),
    u("a typed table", &["CREATE", "TABLE"], &[ANY, "OF"]),
    u(
        "CREATE INDEX ... ON ONLY",
        &["CREATE", "INDEX"],
        &["ON", "ONLY"],
    ),
    u("REINDEX", &["REINDEX"], &[]),
    u("CREATE RECURSIVE VIEW", &["CREATE", "RECURSIVE"], &[]),
    u("CREATE MATERIALIZED VIEW", &["CREATE", "MATERIALIZED"], &[]),
    u("REFRESH MATERIALIZED VIEW", &["REFRESH"], &[]),
    u("CREATE SEQUENCE", &["CREATE", "SEQUENCE"], &[]),
    u("ALTER SEQUENCE", &["ALTER", "SEQUENCE"], &[]),
    u("CREATE STATISTICS", &["CREATE", "STATISTICS"], &[]),
    u("DROP STATISTICS", &["DROP", "STATISTICS"], &[]),
    // --- Routines ---
    u("a SQL-standard routine body", &[], &["BEGIN", "ATOMIC"]),
    u("CREATE PROCEDURE", &["CREATE", "PROCEDURE"], &[]),
    u("CREATE AGGREGATE", &["CREATE", "AGGREGATE"], &[]),
    u("DO", &["DO"], &[]),
    // --- Access control ---
    u("CREATE USER MAPPING", &["CREATE", "USER", "MAPPING"], &[]),
    u("GRANT", &["GRANT"], &[]),
    u("REVOKE", &["REVOKE"], &[]),
    u("CREATE USER", &["CREATE", "USER"], &[]),
    u("ALTER DEFAULT PRIVILEGES", &["ALTER", "DEFAULT"], &[]),
    u("SECURITY LABEL", &["SECURITY", "LABEL"], &[]),
    u("REASSIGN OWNED", &["REASSIGN"], &[]),
    u("DROP OWNED", &["DROP", "OWNED"], &[]),
    // --- Cluster administration ---
    u("VACUUM", &["VACUUM"], &[]),
    u("CLUSTER", &["CLUSTER"], &[]),
    u("CHECKPOINT", &["CHECKPOINT"], &[]),
    u("MOVE", &["MOVE"], &[]),
    u("CREATE DATABASE", &["CREATE", "DATABASE"], &[]),
    u("DROP DATABASE", &["DROP", "DATABASE"], &[]),
    u("ALTER DATABASE", &["ALTER", "DATABASE"], &[]),
    u("ALTER SYSTEM", &["ALTER", "SYSTEM"], &[]),
    // --- Replication and foreign data ---
    u("CREATE PUBLICATION", &["CREATE", "PUBLICATION"], &[]),
    u("ALTER PUBLICATION", &["ALTER", "PUBLICATION"], &[]),
    u("DROP PUBLICATION", &["DROP", "PUBLICATION"], &[]),
    u("CREATE SUBSCRIPTION", &["CREATE", "SUBSCRIPTION"], &[]),
    u("DROP SUBSCRIPTION", &["DROP", "SUBSCRIPTION"], &[]),
    u("CREATE FOREIGN TABLE", &["CREATE", "FOREIGN"], &[]),
    u("IMPORT FOREIGN SCHEMA", &["IMPORT", "FOREIGN"], &[]),
];

/// Builds a row of [`UNSUPPORTED`]. A free function because a `const` table cannot call a method.
const fn u(
    feature: &'static str,
    leading: &'static [&'static str],
    contains: &'static [&'static str],
) -> Unsupported {
    Unsupported {
        feature,
        leading,
        contains,
    }
}

impl Unsupported {
    fn matches(&self, words: &[&str]) -> bool {
        if !starts_with_words(words, self.leading) {
            return false;
        }
        self.contains.is_empty() || contains_words(words, self.contains)
    }
}

fn word_matches(word: &str, pattern: &str) -> bool {
    pattern == ANY || word.eq_ignore_ascii_case(pattern)
}

fn starts_with_words(words: &[&str], pattern: &[&str]) -> bool {
    words.len() >= pattern.len()
        && words
            .iter()
            .zip(pattern)
            .all(|(word, expected)| word_matches(word, expected))
}

fn contains_words(words: &[&str], pattern: &[&str]) -> bool {
    if pattern.is_empty() || pattern.len() > words.len() {
        return false;
    }
    (0..=words.len() - pattern.len()).any(|start| starts_with_words(&words[start..], pattern))
}

/// Names the unsupported construct in a statement that would not parse, if one can be named.
///
/// `None` means the statement is simply malformed, and the client gets `42601` — the right answer
/// for a typo and the wrong one for a feature. Telling those two apart is the whole job here.
fn recognize_unsupported(sql: &str, words: &[&str]) -> Option<&'static str> {
    // `SELECT` with no target list at all -- PostgreSQL returns one row of no columns. Recognised
    // from the source rather than from the word list, because a word list holds only bare words:
    // `SELECT 1 +` also has exactly one of them, and calling that a missing feature instead of the
    // syntax error it is would be the recognizer lying in the other direction.
    if sql
        .trim()
        .trim_end_matches(';')
        .trim()
        .eq_ignore_ascii_case("SELECT")
    {
        return Some("SELECT with an empty target list");
    }
    UNSUPPORTED
        .iter()
        .find(|candidate| candidate.matches(words))
        .map(|candidate| candidate.feature)
}

/// A spelling a user reaching for the time machine is likely to try, and what to write instead.
///
/// Every one of these is `42601` on a real PostgreSQL 19 as well as here
/// (`docs/plans/phase-6d.md` §1), so the *code* is parity and nothing is being invented. What a
/// bare syntax error does not carry is the fact that this node **has** the feature under another
/// name, and a user who wrote `AS OF SYSTEM TIME` because `CockroachDB` spells it that way has no
/// way to discover that. The hint is the whole difference between a dead end and a redirect.
///
/// Consulted only after a parse has already failed, like the table above, so a row can only ever
/// improve an error that was going to be raised anyway.
fn recognize_redirect(words: &[&str]) -> Option<&'static str> {
    const REDIRECTS: &[(&[&str], &str)] = &[
        (
            &["AS", "OF", "SYSTEM", "TIME"],
            "Esker reads the past with SET esker.read_as_of = '<timestamp>' or an interval \
             such as '-1h'. See docs/adr/0021-time-machine.md.",
        ),
        (
            &["AS", "OF", "CHECKPOINT"],
            "Read at a checkpoint with SET TRANSACTION SNAPSHOT '<name>', inside a transaction \
             block. See docs/adr/0021-time-machine.md.",
        ),
        (
            &["FLASHBACK", "TABLE"],
            "Esker puts a table back with SELECT esker_flashback('<table>', '<snapshot>'), \
             which writes the difference forwards rather than unwriting history. See \
             docs/adr/0021-time-machine.md.",
        ),
        (
            &["FOR", "SYSTEM_TIME", "AS", "OF"],
            "Esker reads the past with SET esker.read_as_of = '<timestamp>' or an interval \
             such as '-1h'. See docs/adr/0021-time-machine.md.",
        ),
    ];

    REDIRECTS
        .iter()
        .find(|(pattern, _)| contains_words(words, pattern))
        .map(|(_, hint)| *hint)
}

/// Rewrites the leading keyword of a statement PostgreSQL defines as a synonym for another.
///
/// Both substitutions are spelled out in PostgreSQL's own documentation — `TABLE name` is defined
/// as `SELECT * FROM name`, and `ABORT` is a deprecated synonym for `ROLLBACK` — so this is a
/// rewrite PostgreSQL sanctions rather than an interpretation of ours. It buys two statements that
/// are really written: `TABLE t` is a query, and `ABORT` ends a transaction.
/// `SET CONSTRAINTS { ALL | name [, …] } { DEFERRED | IMMEDIATE }`, read off the words.
///
/// Read here rather than parsed, because `sqlparser` 0.62.0 stops at the `CONSTRAINTS`: it takes
/// `SET` and then wants `=` or `TO`. The grammar is small enough to read exactly — two keywords, a
/// comma-separated list or `ALL`, and one of two modes — and reading it here is what keeps a
/// statement PostgreSQL accepts from reaching a user as a syntax error.
fn set_constraints(sql: &str, scanned: &Scan<'_>) -> Option<StatementClass> {
    let [first, second, ..] = scanned.words.as_slice() else {
        return None;
    };
    if !first.eq_ignore_ascii_case("SET") || !second.eq_ignore_ascii_case("CONSTRAINTS") {
        return None;
    }
    let deferred = match scanned.words.last()? {
        mode if mode.eq_ignore_ascii_case("DEFERRED") => true,
        mode if mode.eq_ignore_ascii_case("IMMEDIATE") => false,
        // Anything else is not this statement, and the parser's own error is the right answer.
        _ => return None,
    };
    // **The names are read from the source, not from the words.** A `Scan`'s words are keywords —
    // a quoted identifier is deliberately not one — and `ActiveRecord` quotes every constraint
    // name it writes, so reading the words gave `SET CONSTRAINTS "x" DEFERRED` an *empty* list,
    // which is `ALL`. That deferred every deferrable constraint where the user named one, and
    // answered "done" where a non-deferrable name is `42809`.
    let body = sql.get(sql.to_ascii_uppercase().find("CONSTRAINTS")? + "CONSTRAINTS".len()..)?;
    let names = constraint_names(body);
    // `ALL` is the empty list: every deferrable constraint, which is not the same question as a
    // list of none.
    if names.len() == 1 && names[0].eq_ignore_ascii_case("ALL") {
        return Some(StatementClass::SetConstraints {
            names: Vec::new(),
            deferred,
        });
    }
    Some(StatementClass::SetConstraints { names, deferred })
}

/// The comma-separated names between `CONSTRAINTS` and the mode, quoted or bare.
///
/// A quoted name keeps its case and its spaces and loses its quotes, which is what a quoted
/// identifier means; a bare one is folded to lower case, which is what an unquoted one means.
fn constraint_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut name = String::new();
            while let Some(c) = chars.next() {
                if c == '"' {
                    // `""` inside a quoted identifier is one quote.
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        name.push('"');
                        continue;
                    }
                    break;
                }
                name.push(c);
            }
            names.push(name);
        } else if c.is_alphanumeric() || c == '_' {
            let mut name = String::from(c);
            while chars
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                name.push(chars.next().unwrap_or_default());
            }
            names.push(name.to_ascii_lowercase());
        }
    }
    // The trailing `DEFERRED` or `IMMEDIATE` is a word like any other here, and is not a name.
    names.pop();
    names
}

fn rewrite_synonym(sql: &str, scanned: &Scan<'_>) -> Option<String> {
    let range = scanned.first_word.clone()?;
    // A `SET CONSTRAINTS` is rewritten **whole**, because nothing of it survives: what it said is
    // carried on the class instead ([`set_constraints`]), and the statement the parser gets is a
    // placeholder that never runs — the executor dispatches on the class first.
    if set_constraints(sql, scanned).is_some() {
        return Some("SELECT 1".to_owned());
    }
    let replacement = match scanned.words.first()? {
        first if first.eq_ignore_ascii_case("TABLE") => "SELECT * FROM",
        first if first.eq_ignore_ascii_case("ABORT") => "ROLLBACK",
        _ => return None,
    };
    let mut rewritten = String::with_capacity(sql.len() + replacement.len());
    rewritten.push_str(sql.get(..range.start)?);
    rewritten.push_str(replacement);
    rewritten.push_str(sql.get(range.end..)?);
    Some(rewritten)
}

/// The deepest nesting anywhere in the statement.
///
/// Counts three things, and skips everything that only looks like them:
///
/// * brackets — `(`, `[`, and `CASE`, which pairs with `END` exactly as a bracket does;
/// * runs of prefix operators — `NOT NOT NOT x` descends three levels without a bracket in sight,
///   and so does `- - - 1`, so the length of the current run is added to the bracket depth;
/// * nothing at all inside a string, a quoted identifier, a dollar-quoted body or a comment.
///
/// An unterminated string or comment ends the scan rather than failing it: the statement is
/// certainly a syntax error, and it is the parser's job to say so with the right message.
#[must_use]
pub fn nesting_depth(sql: &str) -> usize {
    scan(sql).max_depth
}

/// What one pass over a statement's lexical structure yields.
///
/// Both consumers need the same thing understood -- that a `(` inside a string is not nesting and
/// a `GROUP BY` inside a comment is not a clause -- so the quoting rules are implemented once here
/// and the depth guard and the feature recognizer are two readings of the same pass.
struct Scan<'a> {
    /// The deepest nesting anywhere in the statement.
    max_depth: usize,
    /// Every bare word, in order, as it appears in the source. Punctuation, literals, quoted
    /// identifiers and comments are not words: a recognizer matches keywords, and keywords are
    /// exactly what survives this filter.
    words: Vec<&'a str>,
    /// Byte range of the first word. The synonym rewrites are all leading-keyword substitutions,
    /// and this is what lets one be made without re-finding the keyword in text that may open with
    /// whitespace or a comment.
    first_word: Option<core::ops::Range<usize>>,
}

#[allow(clippy::too_many_lines)]
fn scan(sql: &str) -> Scan<'_> {
    let bytes = sql.as_bytes();
    let mut words: Vec<&str> = Vec::new();
    let mut first_word: Option<core::ops::Range<usize>> = None;
    let mut index = 0;
    let mut depth: usize = 0;
    let mut max: usize = 0;
    let mut run: usize = 0;

    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            // Whitespace never changes anything, and must not break a prefix run: `NOT NOT x`.
            b if b.is_ascii_whitespace() => index += 1,
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = line_comment_end(bytes, index);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = block_comment_end(bytes, index);
            }
            b'\'' => {
                index = single_quote_end(bytes, index);
                run = 0;
            }
            b'"' => {
                index = double_quote_end(bytes, index);
                run = 0;
            }
            b'$' => {
                // `None` is a `$` that opens nothing -- a positional parameter such as `$1`, or a
                // `$` inside an identifier -- and is just an ordinary character.
                index = dollar_quote(bytes, index).unwrap_or(index + 1);
                run = 0;
            }
            b'(' | b'[' => {
                depth += 1;
                max = max.max(depth + run);
                index += 1;
                run = 0;
            }
            b')' | b']' => {
                depth = depth.saturating_sub(1);
                index += 1;
                run = 0;
            }
            // A prefix operator descends one level per occurrence in a run. A binary `-` also
            // lands here, but the operand that follows resets the run, so `1 - 2 - 3` stays at 1.
            b'-' | b'+' | b'~' | b'!' | b'@' => {
                run += 1;
                max = max.max(depth + run);
                index += 1;
            }
            b if is_ident_start(b) => {
                let end = ident_end(bytes, index);
                if words.is_empty() {
                    first_word = Some(index..end);
                }
                words.push(&sql[index..end]);
                match keyword(&bytes[index..end]) {
                    Keyword::Case => {
                        depth += 1;
                        max = max.max(depth + run);
                        run = 0;
                    }
                    Keyword::End => {
                        depth = depth.saturating_sub(1);
                        run = 0;
                    }
                    Keyword::Not => {
                        run += 1;
                        max = max.max(depth + run);
                    }
                    Keyword::Other => run = 0,
                }
                index = end;
            }
            _ => {
                index += 1;
                run = 0;
            }
        }
    }
    Scan {
        max_depth: max,
        words,
        first_word,
    }
}

/// The three keywords that move the depth. Everything else resets the prefix run.
enum Keyword {
    Case,
    End,
    Not,
    Other,
}

fn keyword(word: &[u8]) -> Keyword {
    match word.len() {
        3 if word.eq_ignore_ascii_case(b"NOT") => Keyword::Not,
        3 if word.eq_ignore_ascii_case(b"END") => Keyword::End,
        4 if word.eq_ignore_ascii_case(b"CASE") => Keyword::Case,
        _ => Keyword::Other,
    }
}

/// Identifiers may start with a letter, an underscore, or any non-ASCII byte — in UTF-8 every byte
/// of a multi-byte character is `>= 0x80`, so this cannot mistake part of one for a delimiter.
fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn is_ident_char(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit() || byte == b'$'
}

fn ident_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    // `$` continues an identifier but must not swallow a following dollar-quote opener, so the
    // first character is taken unconditionally and the rest by the stricter test.
    index += 1;
    while index < bytes.len() && is_ident_char(bytes[index]) && bytes[index] != b'$' {
        index += 1;
    }
    index
}

fn line_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 2;
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

/// PostgreSQL nests block comments, so `/* /* */ */` is one comment and not a comment plus junk.
fn block_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 2;
    let mut depth = 1usize;
    while index < bytes.len() {
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            depth += 1;
            index += 2;
        } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return index;
            }
        } else {
            index += 1;
        }
    }
    bytes.len()
}

/// A single-quoted literal. `''` is always a literal quote; `\'` is one too, but only in an
/// `E'…'` string, which is why the byte before the opening quote is inspected.
fn single_quote_end(bytes: &[u8], start: usize) -> usize {
    let escapes = start > 0
        && (bytes[start - 1] == b'E' || bytes[start - 1] == b'e')
        && (start < 2 || !is_ident_char(bytes[start - 2]));
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if escapes => index += 2,
            b'\'' if bytes.get(index + 1) == Some(&b'\'') => index += 2,
            b'\'' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// A double-quoted identifier, where `""` is a literal quote and a backslash is never special.
fn double_quote_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'"' if bytes.get(index + 1) == Some(&b'"') => index += 2,
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// A dollar-quoted string, if this `$` opens one.
///
/// Returns the index just past the closing tag, or `None` when the `$` is something else — a
/// positional parameter such as `$1`, or a `$` inside an identifier. Everything between the tags is
/// literal text, including other dollar signs, quotes and comment markers, which is exactly why
/// this has to be understood rather than skipped.
fn dollar_quote(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    while index < bytes.len() && is_ident_char(bytes[index]) && bytes[index] != b'$' {
        index += 1;
    }
    if bytes.get(index) != Some(&b'$') {
        return None;
    }
    let tag = &bytes[start..=index];
    let mut cursor = index + 1;
    while cursor + tag.len() <= bytes.len() {
        if &bytes[cursor..cursor + tag.len()] == tag {
            return Some(cursor + tag.len());
        }
        cursor += 1;
    }
    // Unterminated: consume the rest, and let the parser report the syntax error.
    Some(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::{
        INLINE_PARSE_DEPTH, MAX_NESTING_DEPTH, StatementClass, classify, nesting_depth, parse,
    };
    use crate::error::SqlError;
    use proptest::strategy::Strategy as _;

    /// `SELECT * FROM (SELECT * FROM (... (SELECT 1) ...))`, the construct that costs the most
    /// stack per level and therefore the one every stack test uses.
    fn nested_subqueries(depth: usize) -> String {
        format!(
            "SELECT * FROM {}(SELECT 1{}",
            "(SELECT * FROM ".repeat(depth),
            ")".repeat(depth + 1)
        )
    }

    // --- the guard counts what nests ---

    #[test]
    fn brackets_nest_and_unnest() {
        assert_eq!(nesting_depth("SELECT 1"), 0);
        assert_eq!(nesting_depth("SELECT (1)"), 1);
        assert_eq!(nesting_depth("SELECT (((1)))"), 3);
        // Siblings are not nesting: three closed pairs never exceed one level.
        assert_eq!(nesting_depth("SELECT (1), (2), (3)"), 1);
        assert_eq!(nesting_depth("SELECT a[1]"), 1);
    }

    /// `CASE` pairs with `END` exactly as a bracket does, and nests without a parenthesis in sight.
    #[test]
    fn case_expressions_nest() {
        assert_eq!(nesting_depth("SELECT CASE WHEN a THEN 1 END"), 1);
        assert_eq!(
            nesting_depth("SELECT CASE WHEN a THEN CASE WHEN b THEN 1 END END"),
            2
        );
        // `END` as a synonym for COMMIT must not drive the count below zero.
        assert_eq!(nesting_depth("END"), 0);
    }

    /// A run of prefix operators descends one level each, with no bracket to mark it.
    #[test]
    fn prefix_operator_runs_count_as_depth() {
        assert_eq!(nesting_depth("SELECT NOT TRUE"), 1);
        assert_eq!(nesting_depth("SELECT NOT NOT NOT TRUE"), 3);
        assert_eq!(nesting_depth("SELECT - - - 1"), 3);
        // A binary operator is not a run: the operand between them resets it.
        assert_eq!(nesting_depth("SELECT 1 - 2 - 3 - 4"), 1);
        // Runs add to the bracket depth rather than replacing it.
        assert_eq!(nesting_depth("SELECT (NOT NOT TRUE)"), 3);
    }

    // --- and skips everything that only looks like it ---
    //
    // Each of these would, if miscounted, make the guard refuse a statement PostgreSQL accepts.
    // That is contract C1 broken, so the guard may only ever err towards accepting.

    #[test]
    fn brackets_inside_string_literals_are_text() {
        assert_eq!(nesting_depth("SELECT '((((('"), 0);
        assert_eq!(nesting_depth("SELECT ('(((((')"), 1);
        // A doubled quote is a quote, not the end of the string.
        assert_eq!(nesting_depth("SELECT 'it''s ((('"), 0);
        // Backslash escapes only exist in an E-string.
        assert_eq!(nesting_depth(r"SELECT E'\' ((('"), 0);
        // ... and in an ordinary string the backslash is literal, so this one ends at the quote
        // and the parenthesis that follows is real.
        assert_eq!(nesting_depth(r"SELECT '\', ("), 1);
    }

    #[test]
    fn brackets_inside_quoted_identifiers_are_part_of_the_name() {
        assert_eq!(nesting_depth(r#"SELECT "a((((b" FROM t"#), 0);
        assert_eq!(nesting_depth(r#"SELECT "a""(b" FROM t"#), 0);
    }

    #[test]
    fn dollar_quoted_bodies_are_opaque() {
        assert_eq!(nesting_depth("SELECT $$ ( ( ( $$"), 0);
        assert_eq!(nesting_depth("SELECT $tag$ ( ( ( $tag$"), 0);
        // A body may contain something that looks like a different tag.
        assert_eq!(nesting_depth("SELECT $a$ $b$ ((( $a$"), 0);
        // ... and quotes and comment markers, which are all just text in there.
        assert_eq!(nesting_depth("SELECT $$ 'unclosed /* ( $$"), 0);
    }

    /// `$1` is a parameter, not the start of a dollar-quoted string. Reading it as one would
    /// swallow the rest of the statement and hide every bracket in it.
    #[test]
    fn positional_parameters_are_not_dollar_quotes() {
        assert_eq!(
            nesting_depth("SELECT * FROM t WHERE a = $1 AND b = ((2))"),
            2
        );
    }

    #[test]
    fn comments_are_skipped_and_block_comments_nest() {
        assert_eq!(nesting_depth("SELECT 1 -- ((((\n"), 0);
        assert_eq!(nesting_depth("SELECT 1 /* (((( */"), 0);
        // PostgreSQL nests block comments, so the inner `*/` does not end the outer comment.
        assert_eq!(nesting_depth("SELECT 1 /* /* (((( */ */ "), 0);
        assert_eq!(nesting_depth("SELECT /* c */ (1)"), 1);
    }

    /// An unterminated string is certainly a syntax error, but it is the parser's job to say so
    /// with the right message. The scan must end rather than hang or panic.
    #[test]
    fn unterminated_quoting_ends_the_scan_without_panicking() {
        assert_eq!(nesting_depth("SELECT 'unclosed ((("), 0);
        assert_eq!(nesting_depth(r#"SELECT "unclosed ((("#), 0);
        assert_eq!(nesting_depth("SELECT $$ unclosed ((("), 0);
        assert_eq!(nesting_depth("SELECT 1 /* unclosed ((("), 0);
    }

    /// Multi-byte characters must not be mistaken for delimiters. In UTF-8 every byte of one is
    /// `>= 0x80`, so this is a property of the encoding rather than of the scan -- but it is the
    /// kind of property that a later rewrite in terms of `char` could quietly lose.
    #[test]
    fn multibyte_identifiers_do_not_confuse_the_scan() {
        assert_eq!(nesting_depth("SELECT \"тест\" FROM t"), 0);
        assert_eq!(nesting_depth("SELECT (\"日本語\")"), 1);
        assert_eq!(nesting_depth("SELECT 'ünïcödé (((' "), 0);
    }

    // --- the guard's decisions ---

    /// Contract C1's first regression test. `sqlparser`'s default recursion limit is 50, so this
    /// statement -- which PostgreSQL parses without complaint -- came back "recursion limit
    /// exceeded" until the limit was raised. It is here because nothing about the dependency
    /// advertised that behaviour; only trying it did.
    #[test]
    fn a_statement_deeper_than_the_dependencys_default_limit_still_parses() {
        let sql = format!("SELECT {}1{}", "(".repeat(51), ")".repeat(51));
        assert!(parse(&sql).is_ok(), "51 nested parentheses must parse");
    }

    /// The limit is the limit: at it, we parse; past it, we refuse with the condition PostgreSQL
    /// uses for the same thing. This also verifies `DEEP_PARSE_STACK_BYTES`, since the worst
    /// construct at this depth is exactly what the deep stack is sized for.
    #[test]
    fn the_deepest_admissible_statement_parses() {
        let sql = nested_subqueries(MAX_NESTING_DEPTH - 1);
        assert!(nesting_depth(&sql) <= MAX_NESTING_DEPTH);
        assert!(parse(&sql).is_ok(), "a statement at the limit must parse");
    }

    #[test]
    fn a_statement_past_the_limit_is_refused_as_too_complex() {
        let sql = format!(
            "SELECT {}1{}",
            "(".repeat(MAX_NESTING_DEPTH + 1),
            ")".repeat(MAX_NESTING_DEPTH + 1)
        );
        assert_eq!(parse(&sql), Err(SqlError::StatementTooComplex));
        // 54001 is what PostgreSQL raises when `max_stack_depth` is exceeded.
        assert_eq!(
            SqlError::StatementTooComplex.sqlstate(),
            crate::sqlstate::STATEMENT_TOO_COMPLEX
        );
    }

    /// The inline path is the one with no safety net, so it is the one worth proving. This parses
    /// the most expensive construct at exactly the inline threshold on a stack the size of a
    /// `tokio` blocking thread's -- if `INLINE_PARSE_DEPTH` is ever raised past what
    /// `STACK_PER_NESTING_LEVEL` can pay for, this test overflows and takes the process with it,
    /// which is a great deal better than a client doing it.
    #[test]
    fn a_statement_at_the_inline_threshold_parses_on_a_small_stack() {
        let sql = nested_subqueries(INLINE_PARSE_DEPTH);
        assert!(nesting_depth(&sql) >= INLINE_PARSE_DEPTH);
        let worker = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || parse(&sql).is_ok())
            .unwrap();
        assert!(worker.join().unwrap());
    }

    // --- contract C2 over contract C1's shortfall ---

    /// A statement PostgreSQL accepts and this parser cannot read is a missing feature, and the
    /// client is owed its name. `42601` here would be telling a user that correct SQL is broken.
    #[test]
    fn valid_postgresql_this_parser_cannot_read_names_the_feature() {
        let cases = [
            ("VACUUM ANALYZE t", "VACUUM"),
            ("CREATE PUBLICATION p FOR ALL TABLES", "CREATE PUBLICATION"),
            ("SELECT a FROM t GROUP BY DISTINCT a", "GROUP BY DISTINCT"),
            (
                "SELECT a FROM t FOR KEY SHARE",
                "row-level locking with FOR KEY SHARE",
            ),
            (
                "SELECT a BETWEEN SYMMETRIC 1 AND 2 FROM t",
                "BETWEEN SYMMETRIC",
            ),
            ("ALTER SYSTEM SET work_mem = '64MB'", "ALTER SYSTEM"),
            ("SELECT", "SELECT with an empty target list"),
        ];
        for (sql, feature) in cases {
            let error = parse(sql).expect_err("this parser cannot read it");
            assert_eq!(
                error.sqlstate(),
                crate::sqlstate::FEATURE_NOT_SUPPORTED,
                "{sql} came back as {} instead of 0A000",
                error.sqlstate()
            );
            assert_eq!(error.to_string(), format!("{feature} is not supported"));
        }
    }

    /// `CREATE USER MAPPING` must not be claimed by the shorter `CREATE USER` row. Ordering the
    /// table most-specific-first is the only thing preventing it, so the ordering gets a test.
    #[test]
    fn a_longer_pattern_wins_over_the_shorter_one_it_contains() {
        let error = parse("CREATE USER MAPPING FOR alice SERVER srv OPTIONS (user 'x')")
            .expect_err("not readable");
        assert_eq!(error.to_string(), "CREATE USER MAPPING is not supported");
    }

    /// The recognizer runs only after a parse has failed, so a statement that parses is never
    /// touched by it however much it looks like one of the patterns. `DELETE ... USING u AS x`
    /// matches the aliased-JOIN-USING pattern and must still execute.
    #[test]
    fn a_statement_that_parses_is_never_reinterpreted() {
        for sql in [
            "DELETE FROM t USING u AS x WHERE t.id = x.id",
            "INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING",
            "SELECT a FROM t GROUP BY a",
        ] {
            assert!(parse(sql).is_ok(), "{sql} must still parse");
        }
    }

    /// And a typo is still a typo. A recognizer that answered `0A000` for malformed SQL would be
    /// lying in the other direction, and would hide real mistakes behind a feature name.
    #[test]
    fn a_typo_is_still_a_syntax_error() {
        for sql in ["SELCT 1", "SELECT 1 +", "CREATE TABLE", "UPDATE SET"] {
            let error = parse(sql).expect_err("not valid SQL");
            assert_eq!(
                error.sqlstate(),
                crate::sqlstate::SYNTAX_ERROR,
                "{sql} came back as {}",
                error.sqlstate()
            );
        }
    }

    /// PostgreSQL defines both of these as synonyms, so they are rewritten and executed rather
    /// than refused -- the only rewrites allowed, because anything beyond a documented equivalence
    /// would be inventing semantics.
    #[test]
    fn documented_synonyms_become_the_statements_they_equal() {
        assert_eq!(
            parse("TABLE t").unwrap()[0].to_string(),
            "SELECT * FROM t",
            "TABLE name is defined as SELECT * FROM name"
        );
        assert_eq!(parse("ABORT").unwrap()[0].to_string(), "ROLLBACK");
        assert_eq!(
            classify(&parse("ABORT").unwrap()[0]),
            StatementClass::Rollback
        );
        assert_eq!(
            classify(&parse("TABLE t").unwrap()[0]),
            StatementClass::Query
        );
        // Only the leading keyword is replaced; the rest of the statement survives.
        assert_eq!(
            parse("TABLE t ORDER BY a LIMIT 1").unwrap()[0].to_string(),
            "SELECT * FROM t ORDER BY a LIMIT 1"
        );
        // A `TABLE` that is not the leading keyword is not a synonym.
        assert!(parse("CREATE TABLE t (a int8)").is_ok());
    }

    // --- invariant 9: nothing a client can send may panic ---

    proptest::proptest! {
        /// The guard runs on bytes that arrived over a socket, before anything has validated them.
        /// Every branch in it indexes into a slice, so "never panics" is a claim about arithmetic
        /// as much as about logic (`CLAUDE.md` invariant 9).
        #[test]
        fn the_depth_scan_never_panics(input: String) {
            let depth = nesting_depth(&input);
            proptest::prop_assert!(depth <= input.len());
        }

        /// And neither does the parse behind it -- a malformed statement is an error value, never
        /// an unwind and never an abort.
        #[test]
        fn parsing_arbitrary_text_returns_a_value(input: String) {
            let _ = parse(&input);
        }

        /// Text that is mostly delimiters is where a scanner's edge cases live: unterminated
        /// quotes, a `$` at the very end, a comment opener with no body.
        #[test]
        fn the_depth_scan_survives_delimiter_soup(
            input in proptest::collection::vec(
                proptest::sample::select(vec![
                    "(", ")", "[", "]", "'", "\"", "$", "$$", "$a$", "--", "/*", "*/",
                    "\\", "E'", "NOT", "CASE", "END", " ", "\n", "x",
                ]),
                0..64,
            ).prop_map(|parts| parts.concat())
        ) {
            let _ = nesting_depth(&input);
            let _ = parse(&input);
        }
    }

    // --- classification ---

    #[test]
    fn the_statements_this_crate_executes_are_recognised() {
        let cases = [
            ("SELECT 1", StatementClass::Query),
            ("INSERT INTO t VALUES (1)", StatementClass::Insert),
            ("UPDATE t SET a = 1", StatementClass::Update),
            ("DELETE FROM t", StatementClass::Delete),
            ("CREATE TABLE t (a INT8)", StatementClass::CreateTable),
            ("DROP TABLE t", StatementClass::DropTable),
            ("CREATE INDEX i ON t (a)", StatementClass::CreateIndex),
            ("DROP INDEX i", StatementClass::DropIndex),
            ("BEGIN", StatementClass::Begin),
            ("START TRANSACTION", StatementClass::Begin),
            ("COMMIT", StatementClass::Commit),
            ("ROLLBACK", StatementClass::Rollback),
            ("EXPLAIN SELECT 1", StatementClass::Explain),
        ];
        for (sql, expected) in cases {
            let statements = parse(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
            assert_eq!(classify(&statements[0]), expected, "{sql}");
            assert!(classify(&statements[0]).unsupported_feature().is_none());
        }
    }

    /// Contract C2: everything else parses, and names itself. The name is what the client reads,
    /// so it has to be the SQL construct and not the shape of our AST.
    #[test]
    fn everything_else_parses_and_names_the_feature_it_is() {
        let cases = [
            ("CREATE VIEW v AS SELECT 1", "CREATE VIEW"),
            ("DROP VIEW v", "DROP VIEW"),
            ("GRANT SELECT ON t TO alice", "GRANT SELECT"),
            ("ALTER TABLE t ADD COLUMN b INT8", "ALTER TABLE"),
        ];
        for (sql, feature) in cases {
            let statements = parse(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
            let class = classify(&statements[0]);
            assert_eq!(
                class.unsupported_feature(),
                Some(feature),
                "{sql} named itself wrongly"
            );
        }
    }
}
