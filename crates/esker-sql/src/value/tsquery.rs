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

/// The nesting this text reaches, counted without recursing: every `(`, and every `!`, which
/// stacks a `Not` without a bracket of its own. It can only over-count, which spends a thread that
/// was not needed (`debts-v1.1.md` #101).
fn nesting_of(text: &str) -> usize {
    text.bytes()
        .filter(|byte| *byte == b'(' || *byte == b'!')
        .count()
}

/// Runs one whole `tsquery` operation, on a stack sized for [`crate::value::MAX_VALUE_DEPTH`] when
/// the text is deep.
///
/// **The whole operation and not just the parse.** A `Node` is `Box`-linked, so walking it and
/// dropping it each recurse once per level, and the drop happens wherever the tree is last owned.
/// Parsing on a big stack and handing the tree back left the free running off the caller's 2 MiB:
/// measured, the child died at **6,000 levels, about 349 bytes each**, which is a `Box` drop frame.
/// Everything the entries below return is non-recursive, so only the answer crosses back.
fn deeply<T: Send + 'static>(
    text: &str,
    run: impl FnOnce(&str) -> Result<T> + Send + 'static,
) -> Result<T> {
    if nesting_of(text) > crate::value::INLINE_VALUE_DEPTH {
        let owned = text.to_owned();
        return crate::value::on_a_deep_stack("tsquery", move || run(&owned));
    }
    run(text)
}

/// A `tsquery` literal read and printed back **canonical**, which is what the type stores.
pub fn canonical(text: &str) -> Result<String> {
    deeply(text, |text| Ok(to_text(&from_text(text)?)))
}

/// `to_tsquery(config, text)` as the text it prints, empty when the query is all stop words —
/// which is [`to_tsquery`]'s own `None` and not an error.
pub fn to_tsquery_text(config: Config, text: &str) -> Result<String> {
    deeply(text, move |text| {
        Ok(to_tsquery(config, text)?.map_or_else(String::new, |node| to_text(&node)))
    })
}

/// `tsvector @@ tsquery`, from the text of both. The vector is parsed inside, so nothing borrowed
/// crosses to the sized thread.
pub fn matches_text(vector: &str, query: &str) -> Result<bool> {
    let vector = vector.to_owned();
    deeply(query, move |query| {
        Ok(matches(&tsvector::from_text(&vector)?, &from_text(query)?))
    })
}

/// `ts_rank(tsvector, tsquery)`, from the text of both.
pub fn rank_text(vector: &str, query: &str) -> Result<f32> {
    let vector = vector.to_owned();
    deeply(query, move |query| {
        Ok(rank(&tsvector::from_text(&vector)?, &from_text(query)?))
    })
}

/// The lexemes a query names, which is what `ts_headline` highlights.
pub fn lexemes_of_text(query: &str) -> Result<Vec<String>> {
    deeply(query, |query| Ok(lexemes(&from_text(query)?)))
}

/// `numnode(tsquery)`: how many nodes the query has.
pub fn numnode_of_text(query: &str) -> Result<usize> {
    deeply(query, |query| Ok(numnode(&from_text(query)?)))
}

/// `tsquery || tsquery`, which is an **`OR` of the two and not a splice of their text** — the
/// printed form re-parenthesises by precedence. The one entry that owns two trees at once, so the
/// deeper of the two decides whether they are built on a sized thread.
pub fn or_of_texts(left: &str, right: &str) -> Result<String> {
    let (left, right) = (left.to_owned(), right.to_owned());
    let deepest = if nesting_of(&left) >= nesting_of(&right) {
        left.clone()
    } else {
        right.clone()
    };
    deeply(&deepest, move |_| {
        let (left, right) = (from_text(&left)?, from_text(&right)?);
        Ok(to_text(&Node::Or(Box::new(left), Box::new(right))))
    })
}

/// Parses a `tsquery` literal.
///
/// **Hands the tree out of the module**, which is why the sized thread is not here: whoever owns it
/// last does the freeing, so it is the entries above that take a whole operation to a big stack.
pub fn from_text(text: &str) -> Result<Node> {
    let chars: Vec<char> = text.chars().collect();
    let mut parser = Parser {
        chars: &chars,
        at: 0,
        source: text,
    };
    let node = parser.or(0)?;
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

    /// The top of the grammar, at `depth` brackets deep.
    ///
    /// **Two ways into the cycle and so two checks**: a `(` sends `primary` back here, and a `!`
    /// sends `unary` to itself without passing this line (`debts-v1.1.md` #101).
    fn or(&mut self, depth: usize) -> Result<Node> {
        if depth > crate::value::MAX_VALUE_DEPTH {
            return Err(SqlError::StatementTooComplex);
        }
        let mut left = self.and(depth)?;
        while self.eat("|") {
            left = Node::Or(Box::new(left), Box::new(self.and(depth)?));
        }
        Ok(left)
    }

    fn and(&mut self, depth: usize) -> Result<Node> {
        let mut left = self.phrase(depth)?;
        while self.eat("&") {
            left = Node::And(Box::new(left), Box::new(self.phrase(depth)?));
        }
        Ok(left)
    }

    fn phrase(&mut self, depth: usize) -> Result<Node> {
        let mut left = self.unary(depth)?;
        while self.eat("<->") {
            left = Node::Phrase(Box::new(left), Box::new(self.unary(depth)?));
        }
        Ok(left)
    }

    fn unary(&mut self, depth: usize) -> Result<Node> {
        if depth > crate::value::MAX_VALUE_DEPTH {
            return Err(SqlError::StatementTooComplex);
        }
        if self.eat("!") {
            return Ok(Node::Not(Box::new(self.unary(depth + 1)?)));
        }
        self.primary(depth)
    }

    fn primary(&mut self, depth: usize) -> Result<Node> {
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
            let inner = self.or(depth + 1)?;
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

use super::tsvector::{self, Config, Lexeme, Weight};

/// `to_tsquery(config, text)`: the grammar, then the configuration over each lexeme.
///
/// **A query's lexemes are stemmed and stop-filtered exactly as a vector's are**, which is what
/// makes `@@` work at all — `to_tsquery('english', 'cats')` is `'cat'`, and it has to be, because
/// the vector it will be matched against holds `cat`.
///
/// A stop word **takes its operator with it**: `to_tsquery('english', 'running & the')` is `'run'`,
/// not `'run' & <nothing>`. Measured, and it is why this returns an `Option`: a query made only
/// of stop words is no query at all.
pub fn to_tsquery(config: Config, text: &str) -> Result<Option<Node>> {
    Ok(prune(&from_text(text)?, config))
}

/// `plainto_tsquery(config, text)`: every word `AND`ed together.
#[must_use]
pub fn plainto_tsquery(config: Config, text: &str) -> Option<Node> {
    join(config, text, |left, right| {
        Node::And(Box::new(left), Box::new(right))
    })
}

/// `phraseto_tsquery(config, text)`: every word joined by the phrase operator, so the order is
/// part of the question. `'cat fat'` does **not** match `'fat' <-> 'cat'`, measured.
#[must_use]
pub fn phraseto_tsquery(config: Config, text: &str) -> Option<Node> {
    join(config, text, |left, right| {
        Node::Phrase(Box::new(left), Box::new(right))
    })
}

/// `ts_rank(tsvector, tsquery)`.
///
/// **Derived from sixteen controlled rows rather than from the source**, because a rank is judged
/// to eight significant figures and a plausible formula is not a measured one. The rows are in
/// `docs/plans/tsvector.md` §5; what they settle:
///
/// * the weights are `{A 1.0, B 0.4, C 0.2, D 0.1}` — the A/B/C rows are exactly 10x, 4x and 2x D;
/// * **position does not matter, but the number of positions does**: `Σ w/(j+1)²`, so two
///   positions are 1.25x one;
/// * **an OR is the mean over the query's operands**, not an accumulation — `'a':1B` against
///   `a | b` is `0.4/2`, and an operand the vector lacks contributes zero and still counts. This is
///   the row that refused the first derivation, and one probe with unequal weights settled it;
/// * an OR is then divided by `π²/6`, so the constant `0.6079271` in every single-term answer is
///   `6/π²`;
/// * **an AND is `√(w₁·w₂·word_distance(d))` with no such division**, which is the second
///   asymmetry the first derivation could not explain.
#[must_use]
pub fn rank(vector: &[Lexeme], query: &Node) -> f32 {
    /// `6/π²`, the value every single-term rank is a multiple of.
    const OR_NORM: f32 = 0.607_927_1;

    let operands = lexemes(query);
    if operands.is_empty() {
        return 0.0;
    }
    // The top operator chooses the rule, which is what makes `a & b` and `a | b` different
    // questions over the same two lexemes.
    if matches!(query, Node::And(..)) {
        return and_rank(vector, &operands);
    }
    let total: f32 = operands
        .iter()
        .map(|word| weighted_positions(vector, word))
        .sum();
    // A query with more operands than an `f32` can count is not one anybody writes; saturating
    // keeps the cast honest rather than silently rounding a huge one.
    let count = u16::try_from(operands.len()).unwrap_or(u16::MAX);
    total / f32::from(count) * OR_NORM
}

/// `Σ w/(j+1)²` over a lexeme's positions, zero when the vector does not hold it.
fn weighted_positions(vector: &[Lexeme], word: &str) -> f32 {
    let Some(lexeme) = vector.iter().find(|lexeme| lexeme.word == word) else {
        return 0.0;
    };
    if lexeme.positions.is_empty() {
        // A stripped vector has no positions and still matches; `D` is the weight it carries.
        return weight_of(Weight::D);
    }
    lexeme
        .positions
        .iter()
        .enumerate()
        .map(|(at, (_, weight))| {
            let rank = f32::from(u16::try_from(at + 1).unwrap_or(u16::MAX));
            weight_of(*weight) / (rank * rank)
        })
        .sum()
}

/// **Every pair of operands, at their closest.** `1 - (1 - res)(1 - curw)` is how a third operand
/// would join, which the capture does not reach and is left as PostgreSQL spells it.
fn and_rank(vector: &[Lexeme], operands: &[String]) -> f32 {
    let mut res: f32 = -1.0;
    for (i, left) in operands.iter().enumerate() {
        for right in operands.iter().skip(i + 1) {
            for (left_at, left_weight) in positions_of(vector, left) {
                for (right_at, right_weight) in positions_of(vector, right) {
                    let distance = left_at.abs_diff(right_at);
                    let curw = (weight_of(left_weight)
                        * weight_of(right_weight)
                        * word_distance(distance))
                    .sqrt();
                    res = if res < 0.0 {
                        curw
                    } else {
                        1.0 - (1.0 - res) * (1.0 - curw)
                    };
                }
            }
        }
    }
    res.max(0.0)
}

fn positions_of(vector: &[Lexeme], word: &str) -> Vec<(u16, Weight)> {
    vector
        .iter()
        .find(|lexeme| lexeme.word == word)
        .map(|lexeme| lexeme.positions.clone())
        .unwrap_or_default()
}

fn weight_of(weight: Weight) -> f32 {
    match weight {
        Weight::A => 1.0,
        Weight::B => 0.4,
        Weight::C => 0.2,
        Weight::D => 0.1,
    }
}

/// How much two lexemes `w` tokens apart count for.
///
/// Reproduces the three distances captured — `0.98214` at 1, `0.947867` at 3, `0.15823` at 10 —
/// and PostgreSQL's own cut-off past a hundred, where the term stops mattering at all.
fn word_distance(distance: u16) -> f32 {
    if distance > 100 {
        return 1e-30;
    }
    1.0 / (1.005 + 0.05 * (f32::from(distance) / 1.5 - 2.0).exp())
}

/// `websearch_to_tsquery(config, text)`: the search-box syntax.
///
/// Measured on 19beta1:
///
/// ```text
/// websearch_to_tsquery('english', '"fat cat" -dog')  ->  'fat' <-> 'cat' & !'dog'
/// websearch_to_tsquery('english', 'fat or cat')      ->  'fat' | 'cat'
/// ```
///
/// Four rules: a **quoted run** becomes a phrase, a leading `-` negates the word that follows,
/// the bare word `or` is the `|` operator, and everything else is joined with `&`.
#[must_use]
pub fn websearch_to_tsquery(config: Config, text: &str) -> Option<Node> {
    let mut built: Option<Node> = None;
    let mut or_next = false;
    for token in websearch_tokens(text) {
        let (negated, body) = match token.strip_prefix('-') {
            Some(rest) => (true, rest.to_owned()),
            None => (false, token.clone()),
        };
        if !negated && body.eq_ignore_ascii_case("or") {
            or_next = true;
            continue;
        }
        // A quoted run is a phrase; a bare word is one lexeme, which `phraseto` also gives.
        let Some(mut next) = phraseto_tsquery(config, &body) else {
            continue;
        };
        if negated {
            next = Node::Not(Box::new(next));
        }
        built = Some(match built.take() {
            None => next,
            Some(left) if or_next => Node::Or(Box::new(left), Box::new(next)),
            Some(left) => Node::And(Box::new(left), Box::new(next)),
        });
        or_next = false;
    }
    built
}

/// Splits on whitespace, except that a double-quoted run is one token with its quotes removed.
fn websearch_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn join(config: Config, text: &str, with: fn(Node, Node) -> Node) -> Option<Node> {
    let mut built: Option<Node> = None;
    for lexeme in tsvector::to_lexemes(config, text) {
        let next = Node::Lexeme(lexeme.word);
        built = Some(match built {
            None => next,
            Some(left) => with(left, next),
        });
    }
    built
}

/// Applies a configuration to every lexeme of a parsed query, dropping the ones that are stop
/// words and collapsing the operators left without an operand.
fn prune(node: &Node, config: Config) -> Option<Node> {
    match node {
        Node::Lexeme(word) => tsvector::to_tsvector(config, word)
            .into_iter()
            .next()
            .map(|lexeme| Node::Lexeme(lexeme.word)),
        Node::Not(inner) => prune(inner, config).map(|inner| Node::Not(Box::new(inner))),
        Node::Phrase(a, b) | Node::And(a, b) | Node::Or(a, b) => {
            let (left, right) = (prune(a, config), prune(b, config));
            match (left, right) {
                // **One side surviving is the whole query**, which is what makes
                // `running & the` into `'run'`.
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
                (Some(left), Some(right)) => Some(match node {
                    Node::Phrase(..) => Node::Phrase(Box::new(left), Box::new(right)),
                    Node::And(..) => Node::And(Box::new(left), Box::new(right)),
                    _ => Node::Or(Box::new(left), Box::new(right)),
                }),
            }
        }
    }
}

/// `tsvector @@ tsquery`.
///
/// **The phrase operator is positional and the rest is set membership.** `'cat fat'` holds both
/// lexemes and still does not match `'fat' <-> 'cat'`, because `<->` asks where they are and not
/// whether they are there.
#[must_use]
pub fn matches(vector: &[Lexeme], query: &Node) -> bool {
    match query {
        Node::Lexeme(word) => vector.iter().any(|lexeme| lexeme.word == *word),
        Node::Not(inner) => !matches(vector, inner),
        Node::And(a, b) => matches(vector, a) && matches(vector, b),
        Node::Or(a, b) => matches(vector, a) || matches(vector, b),
        Node::Phrase(..) => !phrase_positions(vector, query).is_empty(),
    }
}

/// Where a node's match *ends*, in token positions, for the phrase operator's benefit.
///
/// A lexeme ends wherever it occurs; `a <-> b` ends where `b` occurs one position after some
/// ending of `a`. Anything else has no position, which is why `!x <-> y` cannot be answered here
/// and is not something the capture asks.
fn phrase_positions(vector: &[Lexeme], node: &Node) -> Vec<u16> {
    match node {
        Node::Lexeme(word) => vector
            .iter()
            .filter(|lexeme| lexeme.word == *word)
            .flat_map(|lexeme| lexeme.positions.iter().map(|(at, _)| *at))
            .collect(),
        Node::Phrase(a, b) => {
            let left = phrase_positions(vector, a);
            phrase_positions(vector, b)
                .into_iter()
                .filter(|at| {
                    at.checked_sub(1)
                        .is_some_and(|before| left.contains(&before))
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Every lexeme a query mentions, in no particular order — what `ts_headline` marks up.
///
/// A `!` operand is included: `ts_headline` marks what the query *names*, and a real server does
/// the same, which is the sort of thing only a measurement settles.
#[must_use]
pub fn lexemes(node: &Node) -> Vec<String> {
    let mut out = Vec::new();
    collect_lexemes(node, &mut out);
    out
}

fn collect_lexemes(node: &Node, out: &mut Vec<String>) {
    match node {
        Node::Lexeme(word) => out.push(word.clone()),
        Node::Not(inner) => collect_lexemes(inner, out),
        Node::Phrase(a, b) | Node::And(a, b) | Node::Or(a, b) => {
            collect_lexemes(a, out);
            collect_lexemes(b, out);
        }
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
    /// **Every line here is an answer PostgreSQL 19beta1 gave**, captured in one rolled-back
    /// session beside the tsvector capture.
    #[test]
    fn a_query_is_stemmed_and_stop_filtered_like_a_vector() {
        let q = |text: &str| {
            to_tsquery(Config::English, text)
                .unwrap()
                .map(|n| to_text(&n))
        };
        // A query's lexemes are stemmed, which is what makes `@@` meet the vector at all.
        assert_eq!(q("cats").as_deref(), Some("'cat'"));
        // **A stop word takes its operator with it**: `running & the` is `'run'`, not `'run' & …`.
        assert_eq!(q("running & the").as_deref(), Some("'run'"));
        // And a query that is nothing but stop words is no query at all.
        assert_eq!(q("the"), None);
    }

    #[test]
    fn plainto_and_phraseto_join_the_words_they_keep() {
        let plain = plainto_tsquery(Config::English, "the fat cats").map(|n| to_text(&n));
        assert_eq!(plain.as_deref(), Some("'fat' & 'cat'"));
        let phrase = phraseto_tsquery(Config::English, "the fat cats").map(|n| to_text(&n));
        assert_eq!(phrase.as_deref(), Some("'fat' <-> 'cat'"));
        assert_eq!(plainto_tsquery(Config::English, "the a of"), None);
    }

    /// **`@@` is set membership until a phrase asks where.** `'cat fat'` holds both lexemes and
    /// still does not match `'fat' <-> 'cat'` — measured, and the reason `<->` needed positions
    /// rather than a second `AND`.
    #[test]
    fn a_phrase_match_is_positional_and_the_rest_is_not() {
        let vector = tsvector::to_tsvector(Config::English, "The Fat Cats ate a rat");
        assert!(matches(
            &vector,
            &to_tsquery(Config::English, "cats").unwrap().unwrap()
        ));
        assert!(matches(
            &vector,
            &to_tsquery(Config::English, "fat & cat").unwrap().unwrap()
        ));
        assert!(!matches(
            &vector,
            &to_tsquery(Config::English, "fat & dog").unwrap().unwrap()
        ));
        assert!(matches(
            &vector,
            &to_tsquery(Config::English, "!dog").unwrap().unwrap()
        ));

        let forwards = tsvector::to_tsvector(Config::English, "fat cat");
        let backwards = tsvector::to_tsvector(Config::English, "cat fat");
        let phrase = phraseto_tsquery(Config::English, "fat cat").unwrap();
        assert!(
            matches(&forwards, &phrase),
            "the words are adjacent and in order"
        );
        assert!(
            !matches(&backwards, &phrase),
            "both lexemes are present and the order is wrong, which is the whole point of <->"
        );
    }

    /// **All sixteen captured rows**, from `docs/plans/tsvector.md` §5 and the probe that
    /// settled the OR rule. A rank is judged to eight significant figures, so these are compared
    /// to a tolerance that would catch a wrong constant and not a rounding difference.
    #[test]
    fn every_rank_is_the_one_postgresql_gave() {
        let rank_of = |vector: &str, query: &str| {
            rank(
                &tsvector::from_text(vector).unwrap(),
                &from_text(query).unwrap(),
            )
        };
        for (vector, query, want) in [
            ("'a'", "a", 0.060_792_71_f32),
            ("'a':1", "a", 0.060_792_71),
            ("'a':5", "a", 0.060_792_71),
            ("'a':1,2", "a", 0.075_990_885),
            ("'a':1A", "a", 0.607_927_1),
            ("'a':1B", "a", 0.243_170_84),
            ("'a':1C", "a", 0.121_585_42),
            // The rows that decided the OR rule: a mean over the operands, absent ones included.
            ("'a':1 'b':2", "a | b", 0.060_792_71),
            ("'a':1B 'b':2C", "a | b", 0.182_378_13),
            ("'a':1B", "a | b", 0.121_585_42),
            ("'b':2C", "a | b", 0.060_792_71),
            ("'a':1 'b':2 'c':3", "a | b | c", 0.060_792_71),
            // And the rows that decided the AND rule and `word_distance`.
            ("'a':1 'b':2", "a & b", 0.099_103_22),
            ("'a':1B 'b':2C", "a & b", 0.280_306_25),
            ("'a':1 'b':4", "a & b", 0.097_358_48),
            ("'a':1 'b':11", "a & b", 0.039_771_15),
        ] {
            let got = rank_of(vector, query);
            assert!(
                (got - want).abs() < 1e-6,
                "ts_rank({vector}, {query}) is {got}, PostgreSQL says {want}"
            );
        }
    }

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
