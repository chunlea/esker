//! Reading a body into a [`Block`]: the constructs the census found, PostgreSQL's own sentence for
//! a body PostgreSQL refuses, and a refusal naming every construct in between.
//!
//! **Three answers, and which one a body gets is measured rather than guessed** (ADR 0113,
//! decision 7):
//!
//! * a construct of the subset reads into a [`Statement`];
//! * a body PostgreSQL itself refuses answers PostgreSQL's code and sentence —
//!   `42601 missing "THEN" at end of SQL expression`, `42601 "x" is not a known variable` — each
//!   one captured against PostgreSQL 19;
//! * a construct PostgreSQL runs and the subset does not is `0A000 PL/pgSQL <construct> is not
//!   supported`. A body is read whole before any of it runs, as PostgreSQL compiles one whole
//!   before running it, so a refusal never leaves half a block behind.
//!
//! An SQL fragment — a condition, an expression, a statement — is kept as its source text and read
//! by the SQL layer when it runs. What this module decides about one is only where it ends.

use super::lex::{self, Kind, Token};
use crate::error::{Result, SqlError};

/// The variables PostgreSQL declares in every trigger function and the subset does not carry.
const TRIGGER_VARIABLES: &[&str] = &[
    "tg_name",
    "tg_when",
    "tg_level",
    "tg_op",
    "tg_relid",
    "tg_relname",
    "tg_table_name",
    "tg_table_schema",
    "tg_nargs",
    "tg_argv",
];

/// Words that continue a type name past its first word: `double precision`,
/// `character varying`, `timestamp with time zone`, `integer array`.
const TYPE_NAME_CONTINUES: &[&str] = &[
    "PRECISION",
    "VARYING",
    "CHARACTER",
    "CHAR",
    "WITH",
    "WITHOUT",
    "TIME",
    "ZONE",
    "YEAR",
    "MONTH",
    "DAY",
    "HOUR",
    "MINUTE",
    "SECOND",
    "TO",
    "ARRAY",
];

/// Where a body is read, which decides what is already declared and what `RETURN` may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    /// An anonymous `DO` block, which returns nothing.
    Do,
    /// A trigger function: `NEW` and `OLD` are declared records and `RETURN` hands back a row.
    Trigger,
}

/// A body: its declarations, then its statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The variables `DECLARE` introduces, in order.
    pub declarations: Vec<Declaration>,
    /// What runs between `BEGIN` and `END`.
    pub statements: Vec<Statement>,
}

/// One declared variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// Its name, folded.
    pub name: String,
    /// What it holds.
    pub ty: VariableType,
}

/// What a variable holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableType {
    /// `record`: no shape until a row is assigned to it.
    Record,
    /// An SQL type, as written. It is resolved when the block runs, which is where a name that is no
    /// type answers PostgreSQL's `42704 type "…" does not exist` — before any statement runs.
    Sql(String),
}

/// One statement of the subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// `NULL;`
    Null,
    /// `<target> := <expression>;`, or with `=`.
    Assign {
        /// What is assigned to.
        target: Target,
        /// The value, as SQL text.
        expression: String,
    },
    /// `IF <condition> THEN … END IF;`
    If {
        /// The condition, as SQL text.
        condition: String,
        /// What runs when it is true.
        then: Vec<Statement>,
    },
    /// `RAISE [NOTICE | WARNING | EXCEPTION] '<message>';` — no level means `EXCEPTION`.
    Raise {
        /// The level.
        level: RaiseLevel,
        /// The message, with its quotes read and `%%` read as `%`.
        message: String,
    },
    /// `RAISE;`, which re-raises the exception being handled. There is none anywhere the subset can
    /// hold one, so it is PostgreSQL's own `0Z002` when it runs.
    Reraise,
    /// `SELECT … INTO <target> …;`
    SelectInto {
        /// The query with its `INTO <target>` cut out.
        query: String,
        /// Where the first column of the first row goes.
        target: Target,
    },
    /// `FOR <record> IN <query> LOOP … END LOOP;`
    ForQuery {
        /// The record variable each row is assigned to.
        record: String,
        /// The query, as SQL text.
        query: String,
        /// What runs once per row.
        body: Vec<Statement>,
    },
    /// `EXECUTE <expression>;` — no `INTO`, no `USING`.
    Execute {
        /// The expression whose value is the command, as SQL text.
        command: String,
    },
    /// `RETURN [<expression>];`
    Return {
        /// The value, as SQL text; `None` for a bare `RETURN;` in a `DO` block.
        expression: Option<String>,
    },
    /// Any other SQL statement, run as it is written.
    Sql {
        /// The statement, as SQL text.
        text: String,
    },
}

/// What an assignment or an `INTO` writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A scalar variable, by its folded name.
    Variable(String),
    /// A field of a record variable, such as `NEW.id`.
    Field {
        /// The record's folded name.
        record: String,
        /// The field's folded name.
        field: String,
    },
}

/// The level a `RAISE` is written at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseLevel {
    /// `NOTICE`.
    Notice,
    /// `WARNING`.
    Warning,
    /// `EXCEPTION`, or no level at all: `P0001` with the message.
    Exception,
}

/// Reads a body.
///
/// Every construct in [`Statement`] reads; a body PostgreSQL refuses answers PostgreSQL's code and
/// sentence; every other construct PostgreSQL has is `0A000` naming it
/// (`docs/plans/plpgsql-subset.md` §4 and §11).
pub fn parse(body: &str, context: Context) -> Result<Block> {
    let tokens = lex::tokenize(body)?;
    let variables = match context {
        Context::Do => Vec::new(),
        Context::Trigger => vec![("new".to_owned(), true), ("old".to_owned(), true)],
    };
    let mut reader = Reader {
        source: body,
        tokens,
        at: 0,
        context,
        variables,
    };
    let block = reader.block()?;
    reader.end_of_body()?;
    Ok(block)
}

/// `0A000` naming a construct PostgreSQL runs and the subset does not.
fn unsupported(construct: impl std::fmt::Display) -> SqlError {
    SqlError::unsupported(format!("PL/pgSQL {construct}"))
}

/// `42601` with one of PostgreSQL's own sentences.
fn syntax(message: impl Into<String>) -> SqlError {
    SqlError::PlpgsqlSyntax(message.into())
}

/// PostgreSQL's sentence for a target that names no variable.
fn not_known(name: &str) -> SqlError {
    syntax(format!("\"{name}\" is not a known variable"))
}

/// The bracket depth after `token`.
fn bracket(token: Token, source: &str, depth: usize) -> usize {
    if token.is_punct(source, '(') || token.is_punct(source, '[') {
        depth + 1
    } else if token.is_punct(source, ')') || token.is_punct(source, ']') {
        depth.saturating_sub(1)
    } else {
        depth
    }
}

/// A body being read, one token at a time.
struct Reader<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    at: usize,
    context: Context,
    /// Every variable in scope, folded, and whether it is a record.
    variables: Vec<(String, bool)>,
}

impl Reader<'_> {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.at).copied()
    }

    fn token(&self, at: usize) -> Option<Token> {
        self.tokens.get(at).copied()
    }

    fn peek_word(&self, word: &str) -> bool {
        self.peek()
            .is_some_and(|token| token.is_word(self.source, word))
    }

    fn peek_punct(&self, punct: char) -> bool {
        self.peek()
            .is_some_and(|token| token.is_punct(self.source, punct))
    }

    /// PostgreSQL's `syntax error at or near "<token>"`, or `syntax error at end of input` past the
    /// last token.
    fn syntax_error(&self, token: Option<Token>) -> SqlError {
        match token {
            Some(token) => SqlError::SyntaxAtOrNear(token.text(self.source).to_owned()),
            None => syntax("syntax error at end of input"),
        }
    }

    fn expect_word(&mut self, word: &str) -> Result<()> {
        if self.peek_word(word) {
            self.at += 1;
            return Ok(());
        }
        Err(self.syntax_error(self.peek()))
    }

    fn expect_semicolon(&mut self) -> Result<()> {
        if self.peek_punct(';') {
            self.at += 1;
            return Ok(());
        }
        Err(self.syntax_error(self.peek()))
    }

    /// The source text the tokens `first..last` were read from.
    fn text(&self, first: usize, last: usize) -> String {
        match (
            self.token(first),
            last.checked_sub(1).and_then(|end| self.token(end)),
        ) {
            (Some(from), Some(to)) if first < last => self
                .source
                .get(from.start..to.end)
                .unwrap_or_default()
                .to_owned(),
            _ => String::new(),
        }
    }

    /// Whether `name` is a variable in scope, and if it is, whether it is a record.
    fn variable(&self, name: &str) -> Option<bool> {
        self.variables
            .iter()
            .rev()
            .find(|(declared, _)| declared == name)
            .map(|&(_, record)| record)
    }

    fn refuse_label(&self) -> Result<()> {
        if self
            .peek()
            .is_some_and(|token| token.is_operator(self.source, "<<"))
        {
            return Err(unsupported("block labels"));
        }
        Ok(())
    }

    /// `[DECLARE …] BEGIN … END`.
    fn block(&mut self) -> Result<Block> {
        self.refuse_label()?;
        let mut declarations: Vec<Declaration> = Vec::new();
        if self.peek_word("DECLARE") {
            self.at += 1;
            loop {
                let Some(token) = self.peek() else {
                    return Err(self.syntax_error(None));
                };
                if token.is_word(self.source, "BEGIN") {
                    break;
                }
                // `DECLARE` may be written again before `BEGIN`, and means nothing the second time.
                if token.is_word(self.source, "DECLARE") {
                    self.at += 1;
                    continue;
                }
                self.refuse_label()?;
                let declaration = self.declaration(&declarations)?;
                self.variables.push((
                    declaration.name.clone(),
                    declaration.ty == VariableType::Record,
                ));
                declarations.push(declaration);
            }
        }
        self.expect_word("BEGIN")?;
        let statements = self.statements()?;
        if self.peek_word("EXCEPTION") {
            return Err(unsupported("EXCEPTION"));
        }
        self.expect_word("END")?;
        Ok(Block {
            declarations,
            statements,
        })
    }

    /// After the block's `END`: nothing but an optional `;`. A name there is an end label, which a
    /// block with no label is refused for in PostgreSQL's words.
    fn end_of_body(&mut self) -> Result<()> {
        if let Some(token) = self.peek()
            && let Some(label) = token.identifier(self.source)
        {
            return Err(syntax(format!(
                "end label \"{label}\" specified for unlabeled block"
            )));
        }
        if self.peek_punct(';') {
            self.at += 1;
        }
        match self.peek() {
            None => Ok(()),
            Some(token) => Err(self.syntax_error(Some(token))),
        }
    }

    /// `<name> <type>;` — the one declaration form of the subset.
    fn declaration(&mut self, earlier: &[Declaration]) -> Result<Declaration> {
        let Some(name_token) = self.peek() else {
            return Err(self.syntax_error(None));
        };
        let Some(name) = name_token.identifier(self.source) else {
            return Err(self.syntax_error(Some(name_token)));
        };
        if earlier.iter().any(|declaration| declaration.name == name) {
            return Err(syntax(format!(
                "duplicate declaration at or near \"{}\"",
                name_token.text(self.source)
            )));
        }
        self.at += 1;
        for (word, construct) in [
            ("CONSTANT", "CONSTANT"),
            ("ALIAS", "ALIAS"),
            ("CURSOR", "cursor declarations"),
            ("SCROLL", "cursor declarations"),
            ("NO", "cursor declarations"),
        ] {
            if self.peek_word(word) {
                return Err(unsupported(construct));
            }
        }
        let first = self.at;
        let mut depth = 0;
        loop {
            let Some(token) = self.peek() else {
                return Err(syntax("incomplete data type declaration at end of input"));
            };
            if depth == 0 {
                if token.is_punct(self.source, ';') {
                    break;
                }
                if token.is_operator(self.source, ":=")
                    || token.is_operator(self.source, "=")
                    || token.is_word(self.source, "DEFAULT")
                {
                    return Err(unsupported("a default value in a declaration"));
                }
                if token.is_word(self.source, "NOT") {
                    return Err(unsupported("NOT NULL in a declaration"));
                }
                if token.is_word(self.source, "COLLATE") {
                    return Err(unsupported("COLLATE in a declaration"));
                }
            }
            depth = bracket(token, self.source, depth);
            self.at += 1;
        }
        let last = self.at;
        let ty = self.declared_type(first, last)?;
        self.at += 1;
        Ok(Declaration { name, ty })
    }

    /// The type a declaration names, from its tokens `first..last`.
    fn declared_type(&self, first: usize, last: usize) -> Result<VariableType> {
        let tokens = self.tokens.get(first..last).unwrap_or_default();
        if tokens.is_empty() {
            return Err(self.syntax_error(self.token(last)));
        }
        for pair in tokens.windows(2) {
            if let [percent, word] = pair
                && percent.is_operator(self.source, "%")
            {
                if word.is_word(self.source, "TYPE") {
                    return Err(unsupported("%TYPE"));
                }
                if word.is_word(self.source, "ROWTYPE") {
                    return Err(unsupported("%ROWTYPE"));
                }
            }
        }
        if let [only] = tokens
            && only.is_word(self.source, "record")
        {
            return Ok(VariableType::Record);
        }
        self.check_type_name(tokens)?;
        Ok(VariableType::Sql(self.text(first, last)))
    }

    /// Checks that `tokens` spell a type name, and answers PostgreSQL's
    /// `syntax error at or near "<token>"` at the first token that cannot continue one.
    ///
    /// That is how `DECLARE n integer BEGIN NULL; END` is `syntax error at or near "BEGIN"`: the
    /// declaration runs to the `;`, and `BEGIN` cannot follow `integer`. Whether the name is a type
    /// this node has is not decided here.
    fn check_type_name(&self, tokens: &[Token]) -> Result<()> {
        let mut at = match tokens.first() {
            Some(first) if matches!(first.kind, Kind::Word | Kind::QuotedIdent) => 1,
            other => return Err(self.syntax_error(other.copied())),
        };
        while let Some(&token) = tokens.get(at) {
            if token.is_punct(self.source, '.')
                && tokens
                    .get(at + 1)
                    .is_some_and(|next| matches!(next.kind, Kind::Word | Kind::QuotedIdent))
            {
                at += 2;
            } else if token.is_punct(self.source, '(') || token.is_punct(self.source, '[') {
                let mut depth = 0;
                while let Some(&inner) = tokens.get(at) {
                    depth = bracket(inner, self.source, depth);
                    at += 1;
                    if depth == 0 {
                        break;
                    }
                }
            } else if TYPE_NAME_CONTINUES
                .iter()
                .any(|word| token.is_word(self.source, word))
            {
                at += 1;
            } else {
                return Err(self.syntax_error(Some(token)));
            }
        }
        Ok(())
    }

    /// Statements, up to a word that ends a list: `END`, `EXCEPTION`, `ELSE` or `ELSIF`. The caller
    /// decides whether the word it stopped at is one it takes.
    fn statements(&mut self) -> Result<Vec<Statement>> {
        let mut statements = Vec::new();
        loop {
            let Some(token) = self.peek() else {
                return Err(self.syntax_error(None));
            };
            if ["END", "EXCEPTION", "ELSE", "ELSIF", "ELSEIF"]
                .iter()
                .any(|word| token.is_word(self.source, word))
            {
                return Ok(statements);
            }
            statements.push(self.statement(token)?);
        }
    }

    fn statement(&mut self, token: Token) -> Result<Statement> {
        self.refuse_label()?;
        if token.kind == Kind::Word {
            let keyword = token.text(self.source).to_ascii_uppercase();
            match keyword.as_str() {
                "NULL" => {
                    self.at += 1;
                    self.expect_semicolon()?;
                    return Ok(Statement::Null);
                }
                "IF" => return self.if_statement(),
                "RAISE" => return self.raise(),
                "FOR" => return self.for_query(),
                "EXECUTE" => return self.execute(),
                "RETURN" => return self.return_statement(),
                "BEGIN" | "DECLARE" => return Err(unsupported("nested blocks")),
                "GET" => return Err(unsupported("GET DIAGNOSTICS")),
                "PERFORM" | "WHILE" | "LOOP" | "EXIT" | "CONTINUE" | "CASE" | "FOREACH"
                | "ASSERT" | "CALL" | "COMMIT" | "ROLLBACK" | "OPEN" | "FETCH" | "MOVE"
                | "CLOSE" => return Err(unsupported(keyword)),
                _ => {}
            }
        }
        if let Some(assignment) = self.assignment(token)? {
            return Ok(assignment);
        }
        if token.is_punct(self.source, ';') {
            return Err(self.syntax_error(Some(token)));
        }
        self.sql_statement(token)
    }

    /// `<name> := <expression>;` or `<record>.<field> = <expression>;`, or `None` when the tokens
    /// at `first` are not an assignment.
    fn assignment(&mut self, first: Token) -> Result<Option<Statement>> {
        let Some(name) = first.identifier(self.source) else {
            return Ok(None);
        };
        let mut next = self.at + 1;
        let mut field = None;
        if self
            .token(next)
            .is_some_and(|token| token.is_punct(self.source, '.'))
            && let Some(field_name) = self
                .token(next + 1)
                .and_then(|token| token.identifier(self.source))
        {
            field = Some(field_name);
            next += 2;
        }
        match self.token(next) {
            Some(token)
                if token.is_operator(self.source, ":=") || token.is_operator(self.source, "=") => {}
            Some(token) if token.is_punct(self.source, '[') => {
                return Err(unsupported("assignment to an array element"));
            }
            _ => return Ok(None),
        }
        let target = self.target(name, field)?;
        self.at = next + 1;
        let first_value = self.at;
        let expression = self.expression(None)?;
        self.refuse_special_variables(first_value, self.at - 1)?;
        Ok(Some(Statement::Assign { target, expression }))
    }

    /// The variable or record field `name` and `field` name, in PostgreSQL's words when they name
    /// neither.
    fn target(&self, name: String, field: Option<String>) -> Result<Target> {
        match (field, self.variable(&name)) {
            (None, Some(false)) => Ok(Target::Variable(name)),
            (None, Some(true)) => Err(unsupported("assignment to a whole record")),
            (Some(field), Some(true)) => Ok(Target::Field {
                record: name,
                field,
            }),
            (None, None) => Err(not_known(&name)),
            (Some(field), _) => Err(not_known(&format!("{name}.{field}"))),
        }
    }

    /// An SQL expression up to `terminator` at bracket depth zero — the word, or `;` for `None` —
    /// with the terminator consumed. The three ways it can go wrong answer PostgreSQL's three
    /// sentences, each measured: the end of the body is `syntax error at end of input`, a `;` before
    /// the word is `missing "<word>" at end of SQL expression`, and nothing at all is
    /// `missing expression at or near "<terminator>"`.
    fn expression(&mut self, terminator: Option<&'static str>) -> Result<String> {
        let first = self.at;
        let mut depth = 0;
        loop {
            let Some(token) = self.peek() else {
                return Err(self.syntax_error(None));
            };
            if depth == 0 {
                let ends = match terminator {
                    Some(word) => token.is_word(self.source, word),
                    None => token.is_punct(self.source, ';'),
                };
                if ends {
                    if self.at == first {
                        return Err(syntax(format!(
                            "missing expression at or near \"{}\"",
                            token.text(self.source)
                        )));
                    }
                    let text = self.text(first, self.at);
                    self.at += 1;
                    return Ok(text);
                }
                if let Some(word) = terminator
                    && token.is_punct(self.source, ';')
                {
                    return Err(syntax(format!(
                        "missing \"{word}\" at end of SQL expression"
                    )));
                }
            }
            depth = bracket(token, self.source, depth);
            self.at += 1;
        }
    }

    /// Refuses, by name, a special variable PostgreSQL declares and the subset does not: `FOUND`
    /// wherever it has not been declared over, and the `TG_` variables of a trigger function.
    ///
    /// Without this a fragment naming one reaches the SQL layer as a column and answers
    /// `42703 column "tg_op" does not exist` — a sentence about a name PostgreSQL knows.
    fn refuse_special_variables(&self, first: usize, last: usize) -> Result<()> {
        for at in first..last {
            let Some(token) = self.token(at) else {
                break;
            };
            if token.kind != Kind::Word {
                continue;
            }
            // `t.found` is a column, not the variable.
            if at > 0
                && self
                    .token(at - 1)
                    .is_some_and(|before| before.is_punct(self.source, '.'))
            {
                continue;
            }
            let name = token.text(self.source).to_ascii_lowercase();
            if name == "found" && self.variable(&name).is_none() {
                return Err(unsupported("FOUND"));
            }
            if self.context == Context::Trigger
                && TRIGGER_VARIABLES.contains(&name.as_str())
                && self.variable(&name).is_none()
            {
                return Err(unsupported(name.to_ascii_uppercase()));
            }
        }
        Ok(())
    }

    /// `IF <condition> THEN … END IF;`
    fn if_statement(&mut self) -> Result<Statement> {
        self.at += 1;
        let first = self.at;
        let condition = self.expression(Some("THEN"))?;
        self.refuse_special_variables(first, self.at - 1)?;
        let then = self.statements()?;
        if self.peek_word("ELSIF") || self.peek_word("ELSEIF") {
            return Err(unsupported("ELSIF"));
        }
        if self.peek_word("ELSE") {
            return Err(unsupported("ELSE"));
        }
        self.expect_word("END")?;
        self.expect_word("IF")?;
        self.expect_semicolon()?;
        Ok(Statement::If { condition, then })
    }

    /// `RAISE [level] '<message>';`
    ///
    /// **The parameter count is checked before any refusal**, because PostgreSQL checks it when it
    /// reads the body: `RAISE WARNING 'a', 'b'` is PostgreSQL's `42601 too many parameters specified
    /// for RAISE`, while `RAISE WARNING 'a %', 'b'` is a statement PostgreSQL runs and the subset
    /// refuses by name.
    fn raise(&mut self) -> Result<Statement> {
        self.at += 1;
        let Some(token) = self.peek() else {
            return Err(self.syntax_error(None));
        };
        if token.is_punct(self.source, ';') {
            self.at += 1;
            return Ok(Statement::Reraise);
        }
        let level = if token.kind == Kind::Word {
            let word = token.text(self.source).to_ascii_uppercase();
            let level = match word.as_str() {
                "NOTICE" => RaiseLevel::Notice,
                "WARNING" => RaiseLevel::Warning,
                "EXCEPTION" => RaiseLevel::Exception,
                "DEBUG" | "LOG" | "INFO" => return Err(unsupported(format!("RAISE {word}"))),
                "USING" => return Err(unsupported("RAISE ... USING")),
                _ => return Err(unsupported("RAISE of a condition name")),
            };
            self.at += 1;
            level
        } else {
            RaiseLevel::Exception
        };
        let Some(message_token) = self.peek() else {
            return Err(self.syntax_error(None));
        };
        let Some(raw) = message_token.literal(self.source) else {
            if message_token.is_word(self.source, "USING") {
                return Err(unsupported("RAISE ... USING"));
            }
            if message_token.kind == Kind::Word {
                return Err(unsupported("RAISE of a condition name"));
            }
            return Err(self.syntax_error(Some(message_token)));
        };
        self.at += 1;
        let (arguments, using) = self.raise_tail()?;
        let placeholders = placeholders(&raw);
        if arguments > placeholders {
            return Err(syntax("too many parameters specified for RAISE"));
        }
        if arguments < placeholders {
            return Err(syntax("too few parameters specified for RAISE"));
        }
        if arguments > 0 {
            return Err(unsupported("RAISE with format arguments"));
        }
        if using {
            return Err(unsupported("RAISE ... USING"));
        }
        Ok(Statement::Raise {
            level,
            message: raw.replace("%%", "%"),
        })
    }

    /// What follows a `RAISE`'s message, through its `;`: how many arguments, and whether there was
    /// a `USING`.
    fn raise_tail(&mut self) -> Result<(usize, bool)> {
        let mut arguments = 0;
        let mut using = false;
        let mut depth = 0;
        loop {
            let Some(token) = self.peek() else {
                return Err(self.syntax_error(None));
            };
            self.at += 1;
            if depth == 0 {
                if token.is_punct(self.source, ';') {
                    return Ok((arguments, using));
                }
                if token.is_punct(self.source, ',') && !using {
                    arguments += 1;
                    continue;
                }
                if token.is_word(self.source, "USING") {
                    using = true;
                    continue;
                }
                if arguments == 0 && !using {
                    return Err(self.syntax_error(Some(token)));
                }
            }
            depth = bracket(token, self.source, depth);
        }
    }

    /// `FOR <record> IN <query> LOOP … END LOOP;`
    fn for_query(&mut self) -> Result<Statement> {
        self.at += 1;
        let Some(variable) = self.peek() else {
            return Err(self.syntax_error(None));
        };
        let Some(name) = variable.identifier(self.source) else {
            return Err(self.syntax_error(Some(variable)));
        };
        self.at += 1;
        if self.peek_punct(',') {
            return Err(unsupported("FOR over a query into a list of variables"));
        }
        self.expect_word("IN")?;
        if self.peek_word("REVERSE") {
            return Err(unsupported("integer FOR loops"));
        }
        if self.peek_word("EXECUTE") {
            return Err(unsupported("FOR ... IN EXECUTE"));
        }
        let first = self.at;
        let query = self.expression(Some("LOOP"))?;
        let last = self.at - 1;
        if self
            .tokens
            .get(first..last)
            .unwrap_or_default()
            .iter()
            .any(|token| token.is_operator(self.source, ".."))
        {
            return Err(unsupported("integer FOR loops"));
        }
        self.refuse_special_variables(first, last)?;
        match self.variable(&name) {
            Some(true) => {}
            Some(false) => return Err(unsupported("FOR over a query into a scalar variable")),
            None => {
                return Err(syntax(
                    "loop variable of loop over rows must be a record variable or list of scalar \
                     variables",
                ));
            }
        }
        let body = self.statements()?;
        self.expect_word("END")?;
        self.expect_word("LOOP")?;
        self.expect_semicolon()?;
        Ok(Statement::ForQuery {
            record: name,
            query,
            body,
        })
    }

    /// `EXECUTE <expression>;`
    fn execute(&mut self) -> Result<Statement> {
        self.at += 1;
        let first = self.at;
        let mut depth = 0;
        loop {
            let Some(token) = self.peek() else {
                return Err(self.syntax_error(None));
            };
            if depth == 0 {
                if token.is_punct(self.source, ';') {
                    break;
                }
                if token.is_word(self.source, "INTO") {
                    // PostgreSQL reads the target first: `INTO STRICT;` is a syntax error at the `;`.
                    let mut target = self.at + 1;
                    if self
                        .token(target)
                        .is_some_and(|next| next.is_word(self.source, "STRICT"))
                    {
                        target += 1;
                    }
                    return Err(match self.token(target) {
                        Some(next) if next.identifier(self.source).is_some() => {
                            unsupported("EXECUTE ... INTO")
                        }
                        other => self.syntax_error(other),
                    });
                }
                if token.is_word(self.source, "USING") {
                    return Err(unsupported("EXECUTE ... USING"));
                }
            }
            depth = bracket(token, self.source, depth);
            self.at += 1;
        }
        if self.at == first {
            return Err(syntax("missing expression at or near \";\""));
        }
        let last = self.at;
        self.refuse_special_variables(first, last)?;
        self.at += 1;
        Ok(Statement::Execute {
            command: self.text(first, last),
        })
    }

    /// `RETURN [<expression>];`
    fn return_statement(&mut self) -> Result<Statement> {
        self.at += 1;
        if self.peek_word("NEXT") {
            return Err(unsupported("RETURN NEXT"));
        }
        if self.peek_word("QUERY") {
            return Err(unsupported("RETURN QUERY"));
        }
        if self.peek_punct(';') {
            // A trigger function returns a row, so a bare `RETURN` has nothing to hand back.
            if self.context == Context::Trigger {
                return Err(syntax("missing expression at or near \";\""));
            }
            self.at += 1;
            return Ok(Statement::Return { expression: None });
        }
        let first = self.at;
        let expression = self.expression(None)?;
        self.refuse_special_variables(first, self.at - 1)?;
        if self.context == Context::Do {
            return Err(SqlError::ReturnParameterInVoid);
        }
        Ok(Statement::Return {
            expression: Some(expression),
        })
    }

    /// Any other SQL statement, through its `;`, and `SELECT … INTO` among them.
    fn sql_statement(&mut self, first_token: Token) -> Result<Statement> {
        let first = self.at;
        let mut depth = 0;
        let mut into = Vec::new();
        loop {
            let Some(token) = self.peek() else {
                return Err(syntax(
                    "unexpected end of function definition at end of input",
                ));
            };
            if depth == 0 {
                if token.is_punct(self.source, ';') {
                    break;
                }
                if token.is_word(self.source, "INTO") {
                    into.push(self.at);
                }
            }
            depth = bracket(token, self.source, depth);
            self.at += 1;
        }
        let last = self.at;
        self.at += 1;
        self.refuse_special_variables(first, last)?;
        let verb = first_token.text(self.source).to_ascii_uppercase();
        // `INSERT INTO` and `MERGE INTO` spend their first `INTO` on the table.
        if matches!(verb.as_str(), "INSERT" | "MERGE") && !into.is_empty() {
            into.remove(0);
        }
        let Some(&at) = into.first() else {
            return Ok(Statement::Sql {
                text: self.text(first, last),
            });
        };
        if !matches!(verb.as_str(), "SELECT" | "WITH") {
            return Err(unsupported(format!("{verb} ... INTO")));
        }
        if into.len() > 1 {
            return Err(syntax("INTO specified more than once"));
        }
        self.select_into(first, at, last)
    }

    /// `SELECT … INTO <target> …`, whose `INTO` is the token at `into` of `first..last`.
    fn select_into(&self, first: usize, into: usize, last: usize) -> Result<Statement> {
        let mut next = into + 1;
        if self
            .token(next)
            .is_some_and(|token| token.is_word(self.source, "STRICT"))
        {
            return Err(unsupported("SELECT ... INTO STRICT"));
        }
        let name_token = self.token(next).filter(|_| next < last);
        let Some(name) = name_token.and_then(|token| token.identifier(self.source)) else {
            return Err(self.syntax_error(name_token.or_else(|| self.token(last))));
        };
        next += 1;
        let mut field = None;
        if next + 1 < last
            && self
                .token(next)
                .is_some_and(|token| token.is_punct(self.source, '.'))
            && let Some(field_name) = self
                .token(next + 1)
                .and_then(|token| token.identifier(self.source))
        {
            field = Some(field_name);
            next += 2;
        }
        if next < last
            && self
                .token(next)
                .is_some_and(|token| token.is_punct(self.source, ','))
        {
            return Err(unsupported("SELECT ... INTO more than one target"));
        }
        if field.is_none() && self.variable(&name) == Some(true) {
            return Err(unsupported("SELECT ... INTO a record"));
        }
        let target = self.target(name, field)?;
        let before = self.text(first, into);
        let after = self.text(next, last);
        let query = if after.is_empty() {
            before
        } else {
            format!("{before} {after}")
        };
        Ok(Statement::SelectInto { query, target })
    }
}

/// How many arguments a `RAISE` message asks for: every `%` that is not half of a `%%`.
fn placeholders(message: &str) -> usize {
    let mut count = 0;
    let mut chars = message.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            if chars.peek() == Some(&'%') {
                chars.next();
            } else {
                count += 1;
            }
        }
    }
    count
}
