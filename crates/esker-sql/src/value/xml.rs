//! `xml`: the text **as it was written**, once it is known to be well formed.
//!
//! # It is `json`'s shape with a different validator
//!
//! Like [`crate::value::json`], an `xml` is stored as the characters that were sent — `'  <a/>  '`
//! keeps its spaces and `'<a></a>'` does not become `'<a/>'` — because the type has no canonical
//! form to normalise towards. And like `json` it has **no equality at all**, so it is not an index
//! key, cannot be `DISTINCT`ed and cannot be ordered. Measured, and each of those is a *different
//! sentence*, which is why all four were probed one at a time:
//!
//! | | |
//! |---|---|
//! | `payload = payload` | `42883 operator does not exist: xml = xml` |
//! | `count(DISTINCT payload)` | `42883 could not identify an equality operator for type xml` |
//! | `ORDER BY payload` | `42883 could not identify an ordering operator for type xml` + HINT |
//! | `CREATE INDEX … (payload)` | `42704 data type xml has no default operator class …` |
//!
//! `min(xml)` is a fifth and is not implied by any of them: `42883 function min(xml) does not
//! exist`, the same shape `bit` has.
//!
//! # It accepts *content*, not only a document
//!
//! `'plain text'::xml` is a value on a real server, and so is `'<a/><b/>'`: the type takes an XML
//! *content* fragment, which may be bare text or several top-level elements. What it refuses is
//! text that is not well formed, and the class is its own — **`2200N invalid XML content`**, not
//! the `22P02` every other input function raises — with a DETAIL that is `libxml`'s own message
//! reaching the client unchanged. [`validate`] reproduces the first line of that DETAIL for the
//! shapes below, all measured on 19beta1:
//!
//! | | |
//! |---|---|
//! | `<a>` | `line 1: Premature end of data in tag a line 1` |
//! | `<a><b>` | `line 1: Premature end of data in tag b line 1` — the **innermost** open tag |
//! | `</a>`, `x</a>` | `line 1: chunk is not well balanced` |
//! | `<a></b>` | `line 1: Opening and ending tag mismatch: a line 1 and b` |
//! | `<`, `<1a/>`, `< a/>` | `line 1: StartTag: invalid element name` |
//! | `<a` | `line 1: Couldn't find end of Start Tag a line 1` |
//! | `<a b=1/>` | `line 1: AttValue: " or ' expected` |
//! | `<a b/>` | `line 1: Specification mandates value for attribute b` |
//! | `&`, `a & b` | `line 1: xmlParseEntityRef: no name` |
//! | `&foo;` | `line 1: Entity 'foo' not defined` |
//! | `<!-- x` | `line 1: Comment not terminated` |
//! | `<![CDATA[x` | `line 1: CData section not finished` |
//!
//! The two line numbers in a message are **different questions**: the one in the `line N:` prefix
//! is where the parser stopped, and the one inside names where the offending tag was *opened*.
//! `E'<a>\n</b>'` is `line 2: Opening and ending tag mismatch: a line 1 and b`, which is what
//! settles that they cannot be one counter.
//!
//! # The one thing that is not stored as written
//!
//! An **XML declaration** is dropped: `'<?xml version="1.0"?><a/>'::xml` prints `<a/>`. A real
//! server drops it in the type's *output function* and keeps it in the value, so
//! `('<?xml version="1.0"?><a/>'::xml)::text` is 25 characters there and 4 here — the single
//! declared divergence of this type, and it is on the `::text` path rather than the one every
//! client reads. [`strip_declaration`] is where it goes, on the way in, so that every path in
//! this node agrees with every other. A declaration anywhere but the very start is an error, and
//! a processing instruction that merely looks like one (`<?php … ?>`) is kept.

use crate::error::{Result, SqlError};

/// The predefined entities. `&foo;` is `Entity 'foo' not defined` because an `xml` fragment has no
/// DTD to define one in.
const PREDEFINED: [&str; 5] = ["lt", "gt", "amp", "apos", "quot"];

/// Drops a leading XML declaration, which is the one part of the text the type does not keep.
///
/// See the module doc: a real server drops it when it prints the value rather than when it reads
/// it, and doing it here is what keeps this node's `xml` self-consistent.
#[must_use]
pub fn strip_declaration(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("<?xml") else {
        return text;
    };
    // `<?xmlfoo?>` is a processing instruction whose target happens to start with those letters,
    // not a declaration — the space is what tells them apart.
    if !rest.starts_with([' ', '\t', '\n', '\r']) {
        return text;
    }
    match rest.find("?>") {
        Some(end) => &rest[end + 2..],
        None => text,
    }
}

/// Checks that the text is well-formed XML **content**.
///
/// A scanner rather than a parser, and the module doc says why that is the whole job: the value is
/// the characters, so nothing is built out of them. What it checks is what a real server refuses,
/// with the message a real server gives.
pub fn validate(text: &str) -> Result<()> {
    Scanner::new(text).run()
}

/// One open element: its name and the line it was opened on, which the mismatch message needs.
struct Open {
    name: String,
    line: usize,
}

struct Scanner {
    chars: Vec<char>,
    at: usize,
    line: usize,
    stack: Vec<Open>,
}

impl Scanner {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            at: 0,
            line: 1,
            stack: Vec::new(),
        }
    }

    /// `line N: <message>`, which is the whole of the first DETAIL line a real server sends.
    fn refuse<T>(&self, message: impl AsRef<str>) -> Result<T> {
        Err(SqlError::InvalidXmlContent(format!(
            "line {}: {}",
            self.line,
            message.as_ref()
        )))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn starts_with(&self, prefix: &str) -> bool {
        prefix
            .chars()
            .enumerate()
            .all(|(offset, want)| self.chars.get(self.at + offset) == Some(&want))
    }

    /// Advances one character, counting lines as it goes.
    fn bump(&mut self) {
        if self.chars.get(self.at) == Some(&'\n') {
            self.line += 1;
        }
        self.at += 1;
    }

    /// Advances to just past `needle`, or to the end and `false` if it is not there.
    fn skip_past(&mut self, needle: &str) -> bool {
        while self.at < self.chars.len() {
            if self.starts_with(needle) {
                for _ in 0..needle.chars().count() {
                    self.bump();
                }
                return true;
            }
            self.bump();
        }
        false
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.bump();
        }
    }

    /// An XML name: a letter, `_` or `:` first, then letters, digits, `.`, `-`, `_` and `:`.
    fn name(&mut self) -> String {
        let mut name = String::new();
        match self.peek() {
            Some(first) if first.is_alphabetic() || first == '_' || first == ':' => {
                name.push(first);
                self.bump();
            }
            _ => return name,
        }
        while let Some(next) = self.peek() {
            if next.is_alphanumeric() || matches!(next, '.' | '-' | '_' | ':') {
                name.push(next);
                self.bump();
            } else {
                break;
            }
        }
        name
    }

    fn run(mut self) -> Result<()> {
        while self.at < self.chars.len() {
            match self.peek() {
                Some('&') => self.entity()?,
                Some('<') => self.markup()?,
                _ => self.bump(),
            }
        }
        match self.stack.last() {
            None => Ok(()),
            // **The innermost open tag**, measured: `<a><b>` names `b` and `<a><b/>` names `a`.
            Some(open) => self.refuse(format!(
                "Premature end of data in tag {} line {}",
                open.name, open.line
            )),
        }
    }

    fn entity(&mut self) -> Result<()> {
        self.bump();
        // `&#65;` and `&#x41;` are character references and need no definition.
        if self.peek() == Some('#') {
            self.bump();
            while matches!(self.peek(), Some(c) if c != ';' && c != '<' && c != '&') {
                self.bump();
            }
            if self.peek() == Some(';') {
                self.bump();
            }
            return Ok(());
        }
        let name = self.name();
        if name.is_empty() {
            return self.refuse("xmlParseEntityRef: no name");
        }
        if self.peek() != Some(';') {
            return self.refuse("xmlParseEntityRef: expecting ';'");
        }
        self.bump();
        if PREDEFINED.contains(&name.as_str()) {
            return Ok(());
        }
        self.refuse(format!("Entity '{name}' not defined"))
    }

    fn markup(&mut self) -> Result<()> {
        if self.starts_with("<!--") {
            return if self.skip_past("-->") {
                Ok(())
            } else {
                self.refuse("Comment not terminated")
            };
        }
        if self.starts_with("<![CDATA[") {
            return if self.skip_past("]]>") {
                Ok(())
            } else {
                self.refuse("CData section not finished")
            };
        }
        if self.starts_with("<?") {
            return self.instruction();
        }
        if self.starts_with("</") {
            return self.close_tag();
        }
        self.open_tag()
    }

    fn instruction(&mut self) -> Result<()> {
        let declaration = self.starts_with("<?xml")
            && matches!(self.chars.get(self.at + 5), Some(' ' | '\t' | '\n' | '\r'));
        // `strip_declaration` has already removed one at offset 0, so anything reaching here is
        // in the wrong place by construction.
        if declaration {
            return self.refuse("XML declaration allowed only at the start of the document");
        }
        self.bump();
        self.bump();
        let target = self.name();
        if self.skip_past("?>") {
            Ok(())
        } else {
            self.refuse(format!("ParsePI: PI {target} never end ..."))
        }
    }

    fn close_tag(&mut self) -> Result<()> {
        self.bump();
        self.bump();
        let name = self.name();
        if name.is_empty() {
            return self.refuse("StartTag: invalid element name");
        }
        self.skip_space();
        if self.peek() != Some('>') {
            return self.refuse(format!(
                "Couldn't find end of Start Tag {name} line {}",
                self.line
            ));
        }
        self.bump();
        match self.stack.pop() {
            // **A close with nothing open is not a mismatch**, and the message says so in its own
            // words: `</a>` and `x</a>` are both `chunk is not well balanced`.
            None => self.refuse("chunk is not well balanced"),
            Some(open) if open.name == name => Ok(()),
            Some(open) => self.refuse(format!(
                "Opening and ending tag mismatch: {} line {} and {name}",
                open.name, open.line
            )),
        }
    }

    fn open_tag(&mut self) -> Result<()> {
        let line = self.line;
        self.bump();
        let name = self.name();
        if name.is_empty() {
            return self.refuse("StartTag: invalid element name");
        }
        loop {
            let unterminated = format!("Couldn't find end of Start Tag {name} line {line}");
            self.skip_space();
            match self.peek() {
                None => return self.refuse(unterminated),
                Some('>') => {
                    self.bump();
                    self.stack.push(Open { name, line });
                    return Ok(());
                }
                // `<a/>` opens and closes at once.
                Some('/') => {
                    self.bump();
                    if self.peek() != Some('>') {
                        return self.refuse(unterminated);
                    }
                    self.bump();
                    return Ok(());
                }
                _ => {}
            }
            let attribute = self.name();
            if attribute.is_empty() {
                return self.refuse("error parsing attribute name");
            }
            self.skip_space();
            if self.peek() != Some('=') {
                return self.refuse(format!(
                    "Specification mandates value for attribute {attribute}"
                ));
            }
            self.bump();
            self.skip_space();
            let quote = match self.peek() {
                Some(quote @ ('"' | '\'')) => quote,
                _ => return self.refuse("AttValue: \" or ' expected"),
            };
            self.bump();
            if !self.skip_past(&quote.to_string()) {
                return self.refuse(unterminated);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{strip_declaration, validate};

    /// **Content, not only a document** — and nothing here is changed on the way through.
    #[test]
    fn content_is_enough_and_nothing_is_normalised() {
        for text in [
            "<foo>bar</foo>",
            "<a><b c=\"d\">e</b></a>",
            "<a b=\"1\" c='2'/>",
            "plain text",
            "  <a/>  ",
            "<a></a>",
            "<a></a >",
            "<a/>",
            "<a/><b/>",
            "",
            "<!-- <a> --><b/>",
            "<?php echo 1; ?><a/>",
            "<a><![CDATA[<not a tag>]]></a>",
            "&amp;",
            "&#65;",
            "<_a/>",
            "<a-b/>",
            "<a:b/>",
        ] {
            assert!(validate(text).is_ok(), "{text}");
        }
    }

    /// Every refusal in the module doc's table, message for message.
    #[test]
    fn a_refusal_is_libxml_s_own_sentence() {
        for (text, detail) in [
            ("<a>", "line 1: Premature end of data in tag a line 1"),
            ("<a><b>", "line 1: Premature end of data in tag b line 1"),
            ("<a><b/>", "line 1: Premature end of data in tag a line 1"),
            ("<a>x", "line 1: Premature end of data in tag a line 1"),
            ("</a>", "line 1: chunk is not well balanced"),
            ("x</a>", "line 1: chunk is not well balanced"),
            (
                "<a></b>",
                "line 1: Opening and ending tag mismatch: a line 1 and b",
            ),
            (
                "<a><b></a>",
                "line 1: Opening and ending tag mismatch: b line 1 and a",
            ),
            (
                "<a></b></c>",
                "line 1: Opening and ending tag mismatch: a line 1 and b",
            ),
            ("<", "line 1: StartTag: invalid element name"),
            ("< a/>", "line 1: StartTag: invalid element name"),
            ("<1a/>", "line 1: StartTag: invalid element name"),
            ("<a", "line 1: Couldn't find end of Start Tag a line 1"),
            ("<a b=1/>", "line 1: AttValue: \" or ' expected"),
            (
                "<a b/>",
                "line 1: Specification mandates value for attribute b",
            ),
            ("&", "line 1: xmlParseEntityRef: no name"),
            ("a & b", "line 1: xmlParseEntityRef: no name"),
            ("<a>&</a>", "line 1: xmlParseEntityRef: no name"),
            ("&foo;", "line 1: Entity 'foo' not defined"),
            ("<!-- unterminated", "line 1: Comment not terminated"),
            (
                "<![CDATA[unterminated",
                "line 1: CData section not finished",
            ),
        ] {
            let refused = validate(text).unwrap_err();
            assert_eq!(refused.sqlstate(), "2200N", "{text}");
            assert_eq!(refused.to_string(), "invalid XML content", "{text}");
            assert_eq!(refused.detail().as_deref(), Some(detail), "{text}");
        }
    }

    /// **The two line numbers are different questions**: where the parser stopped, and where the
    /// tag was opened. One counter would have answered `line 1` twice.
    #[test]
    fn the_prefix_line_and_the_tag_s_line_are_not_the_same_number() {
        let refused = validate("<a>\n</b>").unwrap_err();
        assert_eq!(
            refused.detail().as_deref(),
            Some("line 2: Opening and ending tag mismatch: a line 1 and b")
        );
        let refused = validate("<a>\n<b>\n").unwrap_err();
        assert_eq!(
            refused.detail().as_deref(),
            Some("line 3: Premature end of data in tag b line 2")
        );
    }

    /// The declaration goes, and only when it is a declaration and only at the start.
    #[test]
    fn a_leading_declaration_is_dropped_and_nothing_else_is() {
        assert_eq!(strip_declaration("<?xml version=\"1.0\"?><a/>"), "<a/>");
        assert_eq!(
            strip_declaration("<?xml version=\"1.0\" encoding=\"UTF-8\"?><a>x</a>"),
            "<a>x</a>"
        );
        assert_eq!(strip_declaration("<?xmlfoo?><a/>"), "<?xmlfoo?><a/>");
        assert_eq!(
            strip_declaration("<?php echo 1; ?><a/>"),
            "<?php echo 1; ?><a/>"
        );
        assert_eq!(strip_declaration("<a/>"), "<a/>");
        // Not at the start: kept here, and then refused by the scanner.
        let text = " <?xml version=\"1.0\"?><a/>";
        assert_eq!(strip_declaration(text), text);
        assert_eq!(
            validate(text).unwrap_err().detail().as_deref(),
            Some("line 1: XML declaration allowed only at the start of the document")
        );
    }
}
