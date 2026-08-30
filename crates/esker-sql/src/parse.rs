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

use sqlparser::ast::{ObjectType, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};

use crate::error::{Result, SqlError};

/// How deep a statement may nest before it is refused with SQLSTATE `54001`.
///
/// Chosen to be the same order as PostgreSQL's own effective limit under the default 2 MB
/// `max_stack_depth`, so that the statements Esker refuses are very nearly the statements
/// PostgreSQL refuses — contract C1 tolerates a limit, but not a *low* one.
pub const MAX_NESTING_DEPTH: usize = 1_000;

/// Below this depth a statement is parsed on the caller's own stack.
///
/// Measured, not guessed. On the 2 MiB stack a `tokio` blocking thread gets by default,
/// `sqlparser` costs about 6 KiB of stack per nested parenthesis and about **40 KiB** per nested
/// subquery — `SELECT * FROM (SELECT * FROM (…))` overflows 2 MiB at depth 60. Sixteen levels of
/// the most expensive construct is therefore about 640 KiB, which leaves the rest of the stack to
/// the session and executor frames above it. Ordinary statements are far below this and never
/// leave the caller's thread.
const INLINE_PARSE_DEPTH: usize = 16;

/// The stack given to the thread that parses a statement deeper than [`INLINE_PARSE_DEPTH`].
///
/// [`MAX_NESTING_DEPTH`] levels of nested subquery at the measured 40 KiB each is about 40 MiB;
/// 64 MiB is that with room to spare, and it is reserved address space rather than committed
/// memory, so a thread that does not use it does not pay for it. Verified by
/// `the_deepest_admissible_statement_parses`, which parses at exactly the limit.
const DEEP_PARSE_STACK_BYTES: usize = 64 * 1024 * 1024;

/// The recursion limit handed to `sqlparser` itself.
///
/// Its own default is **50**, which rejects `SELECT ((((…1…))))` at 51 parentheses with a parser
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

/// Parses one statement string into statements, guarding the stack first.
///
/// Contract C1 (`docs/plans/phase-6a.md` §1): a [`SqlError::Syntax`] from here for a statement
/// PostgreSQL 19 accepts is a bug, and belongs in the plan's gap register rather than in a user's
/// error log.
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let depth = nesting_depth(sql);
    if depth > MAX_NESTING_DEPTH {
        return Err(SqlError::StatementTooComplex);
    }
    if depth <= INLINE_PARSE_DEPTH {
        return parse_inner(sql);
    }
    parse_on_a_deep_stack(sql)
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
    let mut words = rendered
        .split_whitespace()
        .take(2)
        .filter(|word| word.chars().all(|c| c.is_ascii_alphabetic()))
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>()
        .join(" ");
    if words.is_empty() {
        words.push_str("this statement");
    }
    words
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
    let bytes = sql.as_bytes();
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
    max
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
