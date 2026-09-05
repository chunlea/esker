//! `tsquery` — lexemes joined by `&`, `|`, `!` and `<->`, and the text it prints as.
//!
//! **A different grammar from `tsvector`'s, not a different spelling of it.** The capture makes the
//! point in two lines that a reader would otherwise get backwards:
//!
//! ```text
//! 'a b'::tsvector -> 'a' 'b'
//! 'a b'::tsquery  -> 42601 syntax error in tsquery: "a b"
//! ```
//!
//! A tsvector is a *set* — whitespace separates its lexemes — so two words side by side are two
//! lexemes. A tsquery is an *expression*, so two words side by side have no operator between them
//! and are a syntax error. The two input functions share nothing but the shape of a lexeme.
//!
//! Precedence, lowest binding first: `|`, then `&`, then `<->`, then the prefix `!`. Printing adds
//! parentheses only where the tree needs them, which is why `websearch_to_tsquery('"fat cat" -dog')`
//! is `'fat' <-> 'cat' & !'dog'` and not `('fat' <-> 'cat') & (!'dog')`.

use crate::error::{Result, SqlError};

/// A parsed query. Stored as its canonical text; this tree exists only between the two.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Node {
    /// A single lexeme.
    Lexeme(String),
    /// `!a`.
    Not(Box<Node>),
    /// `a <-> b`, the phrase operator.
    Phrase(Box<Node>, Box<Node>),
    /// `a & b`.
    And(Box<Node>, Box<Node>),
    /// `a | b`.
    Or(Box<Node>, Box<Node>),
}

/// Binding power, lowest first, so the printer knows when a child needs parentheses.
fn level(node: &Node) -> u8 {
    match node {
        Node::Or(..) => 1,
        Node::And(..) => 2,
        Node::Phrase(..) => 3,
        Node::Not(_) => 4,
        Node::Lexeme(_) => 5,
    }
}

/// The number of nodes, which is what `numnode` reports: `'fat' & 'cat'` is **3**.
#[must_use]
pub fn numnode(node: &Node) -> usize {
    match node {
        Node::Lexeme(_) => 1,
        Node::Not(inner) => 1 + numnode(inner),
        Node::Phrase(a, b) | Node::And(a, b) | Node::Or(a, b) => 1 + numnode(a) + numnode(b),
    }
}

/// The canonical text, which is what is stored and printed.
#[must_use]
pub fn to_text(node: &Node) -> String {
    fn side(node: &Node, parent: u8, out: &mut String) {
        if level(node) < parent {
            out.push('(');
            write(node, out);
            out.push(')');
        } else {
            write(node, out);
        }
    }
    fn write(node: &Node, out: &mut String) {
        match node {
            Node::Lexeme(word) => {
                out.push('\'');
                for c in word.chars() {
                    if c == '\'' {
                        out.push('\'');
                    }
                    out.push(c);
                }
                out.push('\'');
            }
            Node::Not(inner) => {
                out.push('!');
                side(inner, level(node), out);
            }
            Node::Phrase(a, b) | Node::And(a, b) | Node::Or(a, b) => {
                let (own, sign) = match node {
                    Node::Phrase(..) => (3, " <-> "),
                    Node::And(..) => (2, " & "),
                    _ => (1, " | "),
                };
                side(a, own, out);
                out.push_str(sign);
                // The right operand of a left-associative operator needs a parenthesis at its own
                // level too, or `a & (b & c)` would print as `a & b & c` and reparse differently.
                side(b, own + 1, out);
            }
        }
    }
    let mut out = String::new();
    write(node, &mut out);
    out
}

/// Parses a `tsquery` literal.
pub fn from_text(text: &str) -> Result<Node> {
    let chars: Vec<char> = text.chars().collect();
    let mut parser = Parser {
        chars: &chars,
        at: 0,
        source: text,
    };
    let node = parser.or()?;
    parser.space();
    if parser.at < parser.chars.len() {
        // Something is left over, and the commonest cause is two operands with no operator —
        // which is exactly what a valid tsvector looks like.
        return Err(parser.syntax());
    }
    Ok(node)
}

struct Parser<'a> {
    chars: &'a [char],
    at: usize,
    source: &'a str,
}

impl Parser<'_> {
    fn syntax(&self) -> SqlError {
        SqlError::TsQuerySyntax(self.source.to_owned())
    }

    fn no_operand(&self) -> SqlError {
        SqlError::TsQueryNoOperand(self.source.to_owned())
    }

    fn space(&mut self) {
        while self.chars.get(self.at).is_some_and(|c| c.is_whitespace()) {
            self.at += 1;
        }
    }

    fn eat(&mut self, want: &str) -> bool {
        self.space();
        let wanted: Vec<char> = want.chars().collect();
        if self.chars[self.at..].starts_with(&wanted) {
            self.at += wanted.len();
            return true;
        }
        false
    }

    fn or(&mut self) -> Result<Node> {
        let mut left = self.and()?;
        while self.eat("|") {
            left = Node::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Node> {
        let mut left = self.phrase()?;
        while self.eat("&") {
            left = Node::And(Box::new(left), Box::new(self.phrase()?));
        }
        Ok(left)
    }

    fn phrase(&mut self) -> Result<Node> {
        let mut left = self.unary()?;
        while self.eat("<->") {
            left = Node::Phrase(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Node> {
        if self.eat("!") {
            return Ok(Node::Not(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Node> {
        self.space();
        // **An operator with nothing after it is `no operand`, not `syntax error`.** Measured:
        // `to_tsquery('english', 'fat &')` is `no operand in tsquery: "fat &"`.
        let Some(&first) = self.chars.get(self.at) else {
            return Err(self.no_operand());
        };
        if first == ')' {
            return Err(self.no_operand());
        }
        if first == '(' {
            self.at += 1;
            let inner = self.or()?;
            if !self.eat(")") {
                return Err(self.syntax());
            }
            return Ok(inner);
        }
        let mut word = String::new();
        if first == '\'' {
            self.at += 1;
            loop {
                let Some(&c) = self.chars.get(self.at) else {
                    return Err(self.syntax());
                };
                self.at += 1;
                if c == '\'' {
                    if self.chars.get(self.at) == Some(&'\'') {
                        word.push('\'');
                        self.at += 1;
                        continue;
                    }
                    break;
                }
                word.push(c);
            }
        } else {
            while let Some(&c) = self.chars.get(self.at) {
                if c.is_whitespace() || "&|!()<".contains(c) {
                    break;
                }
                word.push(c);
                self.at += 1;
            }
        }
        if word.is_empty() {
            return Err(self.no_operand());
        }
        Ok(Node::Lexeme(word))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(text: &str) -> String {
        to_text(&from_text(text).expect("a valid tsquery"))
    }

    /// The capture's own rows: `to_tsquery` prints its operators with spaces and quotes every
    /// lexeme, and `!` binds to what follows it with no space.
    #[test]
    fn the_operators_print_the_way_postgresql_prints_them() {
        assert_eq!(canon("fat & cat"), "'fat' & 'cat'");
        assert_eq!(canon("fat | cat"), "'fat' | 'cat'");
        assert_eq!(canon("!fat"), "!'fat'");
        assert_eq!(canon("fat <-> cat"), "'fat' <-> 'cat'");
        assert_eq!(canon("cat"), "'cat'");
    }

    /// `websearch_to_tsquery('"fat cat" -dog')` is `'fat' <-> 'cat' & !'dog'` — **no parentheses**,
    /// because `<->` binds tighter than `&`. A printer that bracketed every child would disagree
    /// with the oracle on a row the capture pins.
    #[test]
    fn parentheses_appear_only_where_the_tree_needs_them() {
        assert_eq!(canon("fat <-> cat & !dog"), "'fat' <-> 'cat' & !'dog'");
        assert_eq!(canon("(fat | cat) & dog"), "('fat' | 'cat') & 'dog'");
        assert_eq!(canon("fat & (cat & dog)"), "'fat' & ('cat' & 'dog')");
    }

    /// **The two grammars differ, and this is the line that proves it**: `'a b'` is a two-lexeme
    /// tsvector and a syntax error as a tsquery. Measured, with PostgreSQL's own sentence.
    #[test]
    fn two_operands_with_no_operator_is_a_syntax_error() {
        let error = from_text("a b").expect_err("two lexemes side by side are not a query");
        assert_eq!(error.to_string(), "syntax error in tsquery: \"a b\"");
    }

    /// An operator with nothing to apply to is a **different** sentence, and both are `42601`.
    #[test]
    fn a_dangling_operator_has_no_operand() {
        let error = from_text("fat &").expect_err("an operator needs a right-hand side");
        assert_eq!(error.to_string(), "no operand in tsquery: \"fat &\"");
    }

    /// `numnode(to_tsquery('english', 'fat & cat'))` is **3**: two lexemes and the operator.
    #[test]
    fn numnode_counts_the_operators_too() {
        assert_eq!(numnode(&from_text("fat & cat").unwrap()), 3);
        assert_eq!(numnode(&from_text("cat").unwrap()), 1);
        assert_eq!(numnode(&from_text("!cat").unwrap()), 2);
    }
}
