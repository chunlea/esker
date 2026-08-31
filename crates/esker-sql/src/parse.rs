//! The parser boundary, and the stack this crate has to protect itself.
//!
//! Two jobs, and they are separate on purpose.
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

use sqlparser::ast::{
    ColumnOption, CreateTableOptions, DataType, ExactNumberInfo, Expr, Ident, IndexColumn,
    IndexType, NullsDistinctOption, ObjectName, ObjectType, Statement, TableConstraint,
    TimezoneInfo,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};

use crate::catalog::fold_identifier;
use crate::error::{Result, SqlError};
use crate::plan;
use crate::value::ColumnType;

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
}

impl Parsed {
    /// What kind of statement this is.
    #[must_use]
    pub fn class(&self) -> &StatementClass {
        &self.class
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
    Ok(parse(sql)?
        .into_iter()
        .map(|statement| {
            let class = classify(&statement);
            Parsed { statement, class }
        })
        .collect())
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

    // A statement PostgreSQL defines as a synonym for one the parser does know.
    let rewritten = rewrite_synonym(sql, &scanned);
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
        Err(SqlError::Syntax { message, position }) => {
            match recognize_unsupported(sql, &scanned.words) {
                Some(feature) => Err(SqlError::unsupported(feature)),
                None => Err(SqlError::Syntax { message, position }),
            }
        }
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
        Statement::Rollback { .. } => StatementClass::Rollback,
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
    let words = rendered
        .split_whitespace()
        .take_while(|word| {
            !word.is_empty() && word.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        })
        .take(2)
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
    u("SET CONSTRAINTS", &["SET", "CONSTRAINTS"], &[]),
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
    u("CREATE UNLOGGED TABLE", &["CREATE", "UNLOGGED"], &[]),
    u("an EXCLUDE constraint", &[], &["EXCLUDE", "USING"]),
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
    u(
        "DROP INDEX CONCURRENTLY",
        &["DROP", "INDEX"],
        &["CONCURRENTLY"],
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

/// Rewrites the leading keyword of a statement PostgreSQL defines as a synonym for another.
///
/// Both substitutions are spelled out in PostgreSQL's own documentation — `TABLE name` is defined
/// as `SELECT * FROM name`, and `ABORT` is a deprecated synonym for `ROLLBACK` — so this is a
/// rewrite PostgreSQL sanctions rather than an interpretation of ours. It buys two statements that
/// are really written: `TABLE t` is a query, and `ABORT` ends a transaction.
fn rewrite_synonym(sql: &str, scanned: &Scan<'_>) -> Option<String> {
    let range = scanned.first_word.clone()?;
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

// --- lowering: sqlparser's AST into types this crate owns ---------------------------------------
//
// Everything below turns the parser's tree into `crate::plan`, and it is the last place a
// `sqlparser` type is named. Its rule is that **an unread field is a refused statement**: a clause
// this crate cannot honour comes back as contract C2's `0A000` naming the clause, never as a
// statement quietly executed without it. `CREATE TEMPORARY TABLE` run as a permanent table is the
// failure this rule exists to prevent -- nothing reports it, and the next session finds a table it
// did not expect.

impl Parsed {
    /// Lowers this statement into the plan types the executor runs, or names the construct that
    /// stopped it (contract C2).
    pub fn lower(&self) -> Result<plan::Statement> {
        lower_statement(&self.statement)
    }
}

fn lower_statement(statement: &Statement) -> Result<plan::Statement> {
    match statement {
        Statement::CreateTable(create) => {
            Ok(plan::Statement::CreateTable(lower_create_table(create)?))
        }
        Statement::CreateIndex(create) => {
            Ok(plan::Statement::CreateIndex(lower_create_index(create)?))
        }
        Statement::Drop {
            object_type,
            if_exists,
            names,
            cascade,
            restrict,
            purge,
            temporary,
            ..
        } => {
            refuse_if(*cascade, "DROP ... CASCADE")?;
            refuse_if(*restrict, "DROP ... RESTRICT")?;
            refuse_if(*purge, "DROP ... PURGE")?;
            refuse_if(*temporary, "DROP TEMPORARY")?;
            let names = names.iter().map(object_name).collect::<Result<Vec<_>>>()?;
            Ok(match object_type {
                ObjectType::Table => plan::Statement::DropTable(plan::DropTable {
                    names,
                    if_exists: *if_exists,
                }),
                ObjectType::Index => plan::Statement::DropIndex(plan::DropIndex {
                    names,
                    if_exists: *if_exists,
                }),
                other => return Err(SqlError::unsupported(format!("DROP {other}"))),
            })
        }
        Statement::Explain {
            describe_alias,
            analyze,
            verbose,
            query_plan,
            estimate,
            statement,
            format,
            options,
        } => {
            refuse_if(*analyze, "EXPLAIN ANALYZE")?;
            refuse_if(*verbose, "EXPLAIN VERBOSE")?;
            refuse_if(*query_plan, "EXPLAIN QUERY PLAN")?;
            refuse_if(*estimate, "EXPLAIN ESTIMATE")?;
            refuse_if(format.is_some(), "EXPLAIN (FORMAT ...)")?;
            refuse_if(options.is_some(), "EXPLAIN with options")?;
            let _ = describe_alias;
            Ok(plan::Statement::Explain(Box::new(lower_statement(
                statement,
            )?)))
        }
        other => Err(SqlError::unsupported(feature_name(other))),
    }
}

fn lower_create_table(create: &sqlparser::ast::CreateTable) -> Result<plan::CreateTable> {
    refuse_if(create.or_replace, "CREATE OR REPLACE TABLE")?;
    refuse_if(create.temporary, "CREATE TEMPORARY TABLE")?;
    refuse_if(create.external, "CREATE EXTERNAL TABLE")?;
    refuse_if(create.global.is_some(), "CREATE GLOBAL/LOCAL TABLE")?;
    refuse_if(create.transient, "CREATE TRANSIENT TABLE")?;
    refuse_if(create.volatile, "CREATE VOLATILE TABLE")?;
    refuse_if(create.iceberg, "CREATE ICEBERG TABLE")?;
    refuse_if(create.query.is_some(), "CREATE TABLE ... AS")?;
    refuse_if(create.like.is_some(), "CREATE TABLE ... LIKE")?;
    refuse_if(create.clone.is_some(), "CREATE TABLE ... CLONE")?;
    refuse_if(create.inherits.is_some(), "CREATE TABLE ... INHERITS")?;
    refuse_if(
        create.partition_of.is_some(),
        "CREATE TABLE ... PARTITION OF",
    )?;
    refuse_if(
        create.partition_by.is_some(),
        "CREATE TABLE ... PARTITION BY",
    )?;
    refuse_if(create.on_commit.is_some(), "CREATE TABLE ... ON COMMIT")?;
    refuse_if(create.without_rowid, "CREATE TABLE ... WITHOUT ROWID")?;
    refuse_if(create.strict, "CREATE TABLE ... STRICT")?;
    refuse_if(create.comment.is_some(), "CREATE TABLE ... COMMENT")?;
    refuse_if(create.order_by.is_some(), "CREATE TABLE ... ORDER BY")?;
    refuse_if(create.cluster_by.is_some(), "CREATE TABLE ... CLUSTER BY")?;
    refuse_if(
        !matches!(create.table_options, CreateTableOptions::None),
        "CREATE TABLE ... WITH",
    )?;

    let name = object_name(&create.name)?;
    let mut columns = Vec::with_capacity(create.columns.len());
    let mut primary_key = Vec::new();
    let mut primary_key_name = None;
    let mut unique = Vec::new();

    for column in &create.columns {
        let column_name = ident(&column.name);
        let mut not_null = false;
        for option in &column.options {
            match &option.option {
                ColumnOption::NotNull => not_null = true,
                ColumnOption::Null => {}
                ColumnOption::Unique(constraint) => {
                    refuse_if(
                        constraint.nulls_distinct != NullsDistinctOption::None,
                        "UNIQUE NULLS [NOT] DISTINCT",
                    )?;
                    unique.push(plan::UniqueConstraint {
                        name: option.name.as_ref().map(ident),
                        columns: vec![column_name.clone()],
                    });
                }
                ColumnOption::PrimaryKey(_) => {
                    primary_key.push(column_name.clone());
                    primary_key_name = primary_key_name.or_else(|| option.name.as_ref().map(ident));
                }
                other => return Err(SqlError::unsupported(column_option_name(other))),
            }
        }
        columns.push(plan::Column {
            name: column_name,
            ty: lower_type(&column.data_type)?,
            not_null,
        });
    }

    for constraint in &create.constraints {
        match constraint {
            TableConstraint::PrimaryKey(key) => {
                refuse_if(key.index_name.is_some(), "PRIMARY KEY USING INDEX")?;
                primary_key.extend(index_columns(&key.columns)?);
                primary_key_name = primary_key_name.or_else(|| key.name.as_ref().map(ident));
            }
            TableConstraint::Unique(key) => {
                refuse_if(
                    key.nulls_distinct != NullsDistinctOption::None,
                    "UNIQUE NULLS [NOT] DISTINCT",
                )?;
                unique.push(plan::UniqueConstraint {
                    name: key.name.as_ref().map(ident),
                    columns: index_columns(&key.columns)?,
                });
            }
            TableConstraint::ForeignKey(_) => {
                return Err(SqlError::unsupported("FOREIGN KEY"));
            }
            TableConstraint::Check(_) => return Err(SqlError::unsupported("CHECK")),
            other => {
                return Err(SqlError::unsupported(format!(
                    "the table constraint {other}"
                )));
            }
        }
    }

    Ok(plan::CreateTable {
        name,
        if_not_exists: create.if_not_exists,
        columns,
        primary_key,
        primary_key_name,
        unique,
    })
}

fn lower_create_index(create: &sqlparser::ast::CreateIndex) -> Result<plan::CreateIndex> {
    refuse_if(create.concurrently, "CREATE INDEX CONCURRENTLY")?;
    refuse_if(!create.include.is_empty(), "CREATE INDEX ... INCLUDE")?;
    refuse_if(
        create.nulls_distinct.is_some(),
        "CREATE INDEX ... NULLS [NOT] DISTINCT",
    )?;
    refuse_if(!create.with.is_empty(), "CREATE INDEX ... WITH")?;
    refuse_if(create.predicate.is_some(), "a partial index")?;
    refuse_if(
        !create.index_options.is_empty(),
        "CREATE INDEX with options",
    )?;
    refuse_if(
        !create.alter_options.is_empty(),
        "CREATE INDEX with table options",
    )?;
    if let Some(using) = &create.using {
        // Every index here is a range of the ordered key space, which is what a btree is. Saying
        // `USING hash` and getting one would be a different index than the user asked for.
        refuse_if(
            !matches!(using, IndexType::BTree),
            format!("an index USING {using}"),
        )?;
    }
    Ok(plan::CreateIndex {
        name: create.name.as_ref().map(object_name).transpose()?,
        table: object_name(&create.table_name)?,
        columns: index_columns(&create.columns)?,
        unique: create.unique,
        if_not_exists: create.if_not_exists,
    })
}

/// The six types, under every spelling PostgreSQL accepts for them.
fn lower_type(data_type: &DataType) -> Result<ColumnType> {
    Ok(match data_type {
        DataType::Int8(None) | DataType::BigInt(None) => ColumnType::Int8,
        DataType::Text => ColumnType::Text,
        DataType::Bool | DataType::Boolean => ColumnType::Bool,
        DataType::Bytea => ColumnType::Bytea,
        DataType::Float8 | DataType::DoublePrecision | DataType::Double(ExactNumberInfo::None) => {
            ColumnType::Double
        }
        DataType::Timestamp(None, TimezoneInfo::Tz | TimezoneInfo::WithTimeZone) => {
            ColumnType::TimestampTz
        }
        other => return Err(SqlError::unsupported(format!("the type {other}"))),
    })
}

/// An index's columns, which must be plain names: an expression index is a different feature.
fn index_columns(columns: &[IndexColumn]) -> Result<Vec<String>> {
    columns
        .iter()
        .map(|column| {
            refuse_if(column.operator_class.is_some(), "an index operator class")?;
            refuse_if(
                column.column.options.asc == Some(false),
                "a DESC index column",
            )?;
            refuse_if(
                column.column.options.nulls_first.is_some(),
                "NULLS FIRST/LAST on an index",
            )?;
            refuse_if(column.column.with_fill.is_some(), "WITH FILL")?;
            match &column.column.expr {
                Expr::Identifier(name) => Ok(ident(name)),
                other => Err(SqlError::unsupported(format!(
                    "the index expression {other}"
                ))),
            }
        })
        .collect()
}

/// A name, folded and truncated the way PostgreSQL stores it. Schema qualification is refused
/// rather than ignored: `other.t` and `t` are different tables and answering about the second
/// would be a wrong answer, not a missing feature.
fn object_name(name: &ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [part] => part
            .as_ident()
            .map(ident)
            .ok_or_else(|| SqlError::unsupported(format!("the name {name}"))),
        _ => Err(SqlError::unsupported(format!("the qualified name {name}"))),
    }
}

/// An identifier, folded unless it was quoted -- which is the only thing `quote_style` is for.
fn ident(ident: &Ident) -> String {
    fold_identifier(&ident.value, ident.quote_style.is_some()).0
}

fn refuse_if(condition: bool, feature: impl Into<String>) -> Result<()> {
    if condition {
        return Err(SqlError::unsupported(feature));
    }
    Ok(())
}

fn column_option_name(option: &ColumnOption) -> String {
    match option {
        ColumnOption::Default(_) => "DEFAULT".into(),
        ColumnOption::ForeignKey(_) => "REFERENCES".into(),
        ColumnOption::Check(_) => "CHECK".into(),
        ColumnOption::Generated { .. } => "GENERATED".into(),
        ColumnOption::Identity(_) => "IDENTITY".into(),
        ColumnOption::Collation(name) => format!("COLLATE {name}"),
        ColumnOption::Comment(_) => "COMMENT".into(),
        other => format!("the column option {other}"),
    }
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
            ("SAVEPOINT s", "SAVEPOINT"),
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
