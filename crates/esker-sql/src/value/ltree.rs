//! `ltree`: a path of dot-separated labels, and an order that is not its text's.
//!
//! # The order is the whole of what makes it a type
//!
//! `'a.b'::ltree < 'a-b'::ltree` is **true**, and `'a.b' < 'a-b'` as bytes is **false**. A real
//! server compares an ltree *label by label* — a shorter path that is a prefix of a longer one
//! first, and each label byte for byte — where a plain string comparison sees a `.` (0x2E) sitting
//! above a `-` (0x2D) and puts the two the other way round. Measured, eight paths at once:
//!
//! | | |
//! |---|---|
//! | as `ltree` | `A a a.B a.a a.b a-b ab b` |
//! | as bytes (`COLLATE "C"`) | `A a a-b a.B a.a a.b ab b` |
//!
//! Equality *is* the text's — nothing is normalised, `A` and `a` are different labels — so this is
//! ADR 0042's rule with only half of it failing: the type may share `text`'s representation in the
//! row and may not share its **key**. `esker_keys::row` writes the separator as `\x01` there,
//! which no label can contain, and reads it back; the table above is the fixture that check runs.
//!
//! # A label
//!
//! One or more of `A-Za-z0-9_-` or any non-ASCII letter — measured one character at a time:
//! `a-b`, `a_b`, `a1.B2` and `héllo` are paths, and `a b`, `a@b`, `a!b`, `a*b`, `a+b`, `a%b`,
//! `a~b` and `"a b"` are not. The **empty path** is a value with zero labels, and `''::ltree` is
//! how `nlevel` reaches 0.
//!
//! # Its refusal is a *syntax* error
//!
//! `42601 ltree syntax error at character N`, one-based — not the `22P02` every other input
//! function raises, and the position is part of the message. The one shape without a position is
//! a trailing separator: `'a.'` is `42601 ltree syntax error` with `DETAIL: Unexpected end of
//! input.` Both measured.

use crate::error::{Result, SqlError};

/// Reads a path, and answers it **unchanged**.
///
/// The value is its characters, so nothing is built from them — what this does is refuse what a
/// real server refuses, with the position a real server reports.
pub fn from_text(text: &str) -> Result<String> {
    // The empty path is a value: zero labels, and `nlevel('')` is 0.
    if text.is_empty() {
        return Ok(String::new());
    }
    let mut label = 0;
    for (at, ch) in text.char_indices() {
        if ch == '.' {
            // **A separator with no label before it**, which covers `.a`, `a..b` and a leading
            // dot alike: the position is the one-based index of the offending character.
            if label == 0 {
                return Err(syntax_at(at));
            }
            label = 0;
            continue;
        }
        if !is_label_char(ch) {
            return Err(syntax_at(at));
        }
        label += 1;
    }
    // A trailing separator has no character to point at, and a real server says so differently.
    if label == 0 {
        return Err(SqlError::LtreeSyntax(None));
    }
    Ok(text.to_owned())
}

/// The number of labels: `nlevel`.
#[must_use]
pub fn nlevel(path: &str) -> i32 {
    if path.is_empty() {
        return 0;
    }
    i32::try_from(path.split('.').count()).unwrap_or(i32::MAX)
}

/// Whether `outer` is a prefix path of `inner` — the `@>` operator, and `<@` reversed.
///
/// **Label-wise, not character-wise**: `a` contains `a.b` and does *not* contain `ab`, which is
/// the one thing a `starts_with` on the text would get wrong.
#[must_use]
pub fn contains(outer: &str, inner: &str) -> bool {
    if outer.is_empty() {
        return true;
    }
    let mut left = outer.split('.');
    let mut right = inner.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, _) => return true,
            // Either the inner path ran out first, or a label differs: both are "not a prefix".
            (Some(_), None) => return false,
            (Some(a), Some(b)) => {
                if a != b {
                    return false;
                }
            }
        }
    }
}

/// `'a.b'::ltree || 'c.d'::ltree`, which is the two paths joined by one separator.
#[must_use]
pub fn concat(left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, _) => right.to_owned(),
        (_, true) => left.to_owned(),
        _ => format!("{left}.{right}"),
    }
}

/// PostgreSQL's own order over two paths: label by label, each byte for byte.
#[must_use]
pub fn cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let mut a = left.split('.');
    let mut b = right.split('.');
    loop {
        match (a.next(), b.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => match x.as_bytes().cmp(y.as_bytes()) {
                std::cmp::Ordering::Equal => {}
                other => return other,
            },
        }
    }
}

/// `A-Za-z0-9_-`, or any non-ASCII letter — `'héllo'::ltree` is a path.
fn is_label_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || (!ch.is_ascii() && ch.is_alphabetic())
}

/// `42601 ltree syntax error at character N`, one-based over **characters**.
fn syntax_at(byte_offset: usize) -> SqlError {
    SqlError::LtreeSyntax(Some(byte_offset + 1))
}

/// One item of an `lquery`: what a single position in the pattern accepts.
enum Item {
    /// `*`, `*{n}`, `*{n,m}`, `*{n,}` — a **run** of labels, not one label.
    Any { least: usize, most: usize },
    /// A list of alternatives, negated by a leading `!`. Each carries its own modifiers.
    One {
        negated: bool,
        options: Vec<Option_>,
    },
}

/// One alternative inside an item, with the two modifiers that change how it matches.
struct Option_ {
    label: String,
    /// `@`: fold both sides before comparing.
    fold: bool,
    /// `*`: the label is a prefix. `%` is the same with a word boundary after it.
    prefix: Option<Prefix>,
}

/// Which of the two prefix modifiers a term carries.
#[derive(PartialEq)]
enum Prefix {
    /// `ab*` matches `abc`.
    Any,
    /// `ab%` does **not** match `abc` and does match `ab_c`: the prefix has to end a word, and a
    /// word ends at an `_` or at the end of the label. Measured, and it is the one modifier a
    /// reader would take for `*`'s synonym.
    Word,
}

/// Reads a pattern, and answers it unchanged — the shape [`from_text`] has for a path.
pub fn lquery_checked(text: &str) -> Result<String> {
    compile(text)?;
    Ok(text.to_owned())
}

/// Whether a path matches a pattern: the `~` operator.
///
/// **The pattern has to match the whole path**, which is the rule a reader would miss:
/// `'a.b.c' ~ 'a.b'` is `f` and `'a.b.c' ~ 'a.*'` is `t`, because only `*` consumes a run.
pub fn matches(path: &str, pattern: &str) -> Result<bool> {
    let items = compile(pattern)?;
    let labels: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('.').collect()
    };
    Ok(walk(&items, &labels))
}

/// Reads the pattern into its items, or `42601 lquery syntax error`.
fn compile(pattern: &str) -> Result<Vec<Item>> {
    // The empty pattern is not one, and neither is a trailing separator: both are
    // `lquery syntax error` with `Unexpected end of input.` and no position. Measured.
    if pattern.is_empty() {
        return Err(SqlError::LQuerySyntax(None));
    }
    let mut items = Vec::new();
    for (at, piece) in pattern.split('.').enumerate() {
        items.push(item(piece, at, pattern)?);
    }
    Ok(items)
}

fn item(piece: &str, at: usize, pattern: &str) -> Result<Item> {
    if piece.is_empty() {
        return Err(SqlError::LQuerySyntax(None));
    }
    if let Some(rest) = piece.strip_prefix('*') {
        return quantifier(rest).ok_or_else(|| position(at, pattern));
    }
    let (negated, rest) = match piece.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, piece),
    };
    let mut options = Vec::new();
    for alternative in rest.split('|') {
        options.push(term(alternative).ok_or_else(|| position(at, pattern))?);
    }
    Ok(Item::One { negated, options })
}

/// `*`, `*{n}`, `*{n,m}` or `*{n,}` — the run's bounds.
fn quantifier(rest: &str) -> Option<Item> {
    if rest.is_empty() {
        return Some(Item::Any {
            least: 0,
            most: usize::MAX,
        });
    }
    let inner = rest.strip_prefix('{')?.strip_suffix('}')?;
    let (low, high) = match inner.split_once(',') {
        // `*{n,}` is "n or more"; `*{n,m}` is a closed range.
        Some((low, "")) => (low, None),
        Some((low, high)) => (low, Some(high)),
        // `*{n}` is exactly n.
        None => (inner, Some(inner)),
    };
    let least: usize = low.trim().parse().ok()?;
    let most = match high {
        Some(high) => high.trim().parse().ok()?,
        None => usize::MAX,
    };
    (least <= most).then_some(Item::Any { least, most })
}

fn term(alternative: &str) -> Option<Option_> {
    let mut label = alternative;
    let mut fold = false;
    let mut prefix = None;
    // The modifiers are a suffix and may be written in either order — `a@*` and `a*@` are one
    // term. Read off the end until none is left.
    loop {
        if let Some(rest) = label.strip_suffix('@') {
            fold = true;
            label = rest;
            continue;
        }
        if let Some(rest) = label.strip_suffix('*') {
            prefix = Some(Prefix::Any);
            label = rest;
            continue;
        }
        if let Some(rest) = label.strip_suffix('%') {
            prefix = Some(Prefix::Word);
            label = rest;
            continue;
        }
        break;
    }
    if label.is_empty() || !label.chars().all(is_label_char) {
        return None;
    }
    Some(Option_ {
        label: label.to_owned(),
        fold,
        prefix,
    })
}

/// `42601 lquery syntax error at character N`, one-based, over the whole pattern.
fn position(at: usize, pattern: &str) -> SqlError {
    let before: usize = pattern
        .split('.')
        .take(at)
        .map(|piece| piece.chars().count() + 1)
        .sum();
    SqlError::LQuerySyntax(Some(before + 1))
}

/// Whether the items consume exactly the labels, `*` runs and all.
fn walk(items: &[Item], labels: &[&str]) -> bool {
    let Some((first, rest)) = items.split_first() else {
        return labels.is_empty();
    };
    match first {
        Item::Any { least, most } => {
            let ceiling = (*most).min(labels.len());
            (*least..=ceiling).any(|taken| walk(rest, &labels[taken..]))
        }
        Item::One { negated, options } => {
            let Some((label, tail)) = labels.split_first() else {
                return false;
            };
            let hit = options.iter().any(|option| accepts(option, label));
            hit != *negated && walk(rest, tail)
        }
    }
}

/// Whether one alternative accepts one label.
fn accepts(option: &Option_, label: &str) -> bool {
    let (want, have) = if option.fold {
        (option.label.to_lowercase(), label.to_lowercase())
    } else {
        (option.label.clone(), label.to_owned())
    };
    match option.prefix {
        None => want == have,
        Some(Prefix::Any) => have.starts_with(&want),
        // The prefix has to end a word: `ab%` matches `ab` and `ab_c` and not `abc`.
        Some(Prefix::Word) => {
            have.starts_with(&want)
                && (have.len() == want.len() || have[want.len()..].starts_with('_'))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{cmp, concat, contains, from_text, nlevel};

    /// Every path the capture accepts, and it comes back exactly as it went in.
    #[test]
    fn a_path_is_its_characters() {
        for path in [
            "1.2.3",
            "1.2.3.4",
            "Top.Science.Astronomy",
            "a",
            "",
            "a-b",
            "a_b",
            "a1.B2",
            "héllo",
            "0123456789",
            "_-",
        ] {
            assert_eq!(from_text(path).unwrap(), path);
        }
    }

    /// And every refusal, with the position a real server reports.
    #[test]
    fn a_refusal_is_a_syntax_error_with_a_position() {
        for (path, at) in [
            ("a..b", Some(3)),
            (".a", Some(1)),
            ("a b", Some(2)),
            ("a@b", Some(2)),
            ("a!b", Some(2)),
            ("a*b", Some(2)),
            ("a+b", Some(2)),
            ("a%b", Some(2)),
            ("a~b", Some(2)),
            ("\"a b\"", Some(1)),
            // The one without a position: nothing to point at past the end.
            ("a.", None),
        ] {
            let refused = from_text(path).unwrap_err();
            assert_eq!(refused.sqlstate(), "42601", "{path}");
            let (message, detail) = match at {
                Some(at) => (format!("ltree syntax error at character {at}"), None),
                // The one without a position: a real server moves what it knows into the DETAIL.
                None => (
                    "ltree syntax error".to_owned(),
                    Some("Unexpected end of input."),
                ),
            };
            assert_eq!(refused.to_string(), message, "{path}");
            assert_eq!(refused.detail().as_deref(), detail, "{path}");
        }
    }

    /// **The module doc's table**, which is what says the order is not the text's.
    #[test]
    fn the_order_is_the_labels_and_not_the_bytes() {
        let mut paths = vec!["a.b", "a-b", "ab", "a", "a.a", "b", "A", "a.B"];
        paths.sort_by(|left, right| cmp(left, right));
        assert_eq!(paths, ["A", "a", "a.B", "a.a", "a.b", "a-b", "ab", "b"]);

        let mut bytes = vec!["a.b", "a-b", "ab", "a", "a.a", "b", "A", "a.B"];
        bytes.sort_unstable();
        assert_eq!(bytes, ["A", "a", "a-b", "a.B", "a.a", "a.b", "ab", "b"]);
        assert_ne!(paths, bytes);
    }

    /// **Every `lquery` row of the capture**, and the whole-path rule that holds them together:
    /// only `*` consumes a run, so `'a.b.c' ~ 'a.b'` is false.
    #[test]
    fn a_pattern_matches_the_whole_path() {
        for (path, pattern, want) in [
            ("a.b.c", "a.*", true),
            ("a.b.c", "*.c", true),
            ("a.b.c", "*.b.*", true),
            ("a.b.c", "a.b.c", true),
            ("a.b.c", "a.b", false),
            ("a.b.c", "*", true),
            // The quantifier counts labels the run stands for, and `a.*{1}` is one label after
            // `a` where the path has two.
            ("a.b.c", "a.*{1}", false),
            ("a.b.c", "a.*{2}", true),
            ("a.b.c", "a.*{1,2}", true),
            ("a.b.c", "a.*{0}", false),
            ("a.b.c", "*{2}.c", true),
            ("a.b.c", "*{1,}", true),
            // `!` negates the alternatives at that one position.
            ("a.b.c", "!a.*", false),
            ("a.b.c", "!b.*", true),
            ("a.b.c", "a|x.*", true),
            ("a.b.c", "x|y.*", false),
            // `@` folds, and without it the case is part of the label.
            ("A.b.c", "a@.*", true),
            ("A.b.c", "a.*", false),
            // **`%` is not `*`**: it wants the prefix to end a word.
            ("abc.d", "ab%.*", false),
            ("abc.d", "ab*.*", true),
            ("ab_c.d", "ab%.*", true),
            ("", "*", true),
            ("", "a", false),
        ] {
            assert_eq!(
                super::matches(path, pattern).unwrap(),
                want,
                "{path:?} ~ {pattern:?}"
            );
        }
        // Both refusals have no position, which is what a real server says for each.
        for pattern in ["", "a."] {
            let refused = super::matches("a.b", pattern).unwrap_err();
            assert_eq!(refused.sqlstate(), "42601");
            assert_eq!(refused.to_string(), "lquery syntax error");
            assert_eq!(
                refused.detail().as_deref(),
                Some("Unexpected end of input.")
            );
        }
    }

    /// `nlevel`, `@>` and `||`, and the one thing a text `starts_with` gets wrong.
    #[test]
    fn the_operators_are_label_wise() {
        assert_eq!(nlevel(""), 0);
        assert_eq!(nlevel("a"), 1);
        assert_eq!(nlevel("1.2.3"), 3);

        assert!(contains("a", "a.b.c"));
        assert!(contains("a.b", "a.b"));
        assert!(contains("", "a.b"));
        // `a` does **not** contain `ab`, where `"ab".starts_with("a")` is true.
        assert!(!contains("a", "ab"));
        assert!(!contains("a.b.c", "a.b"));

        assert_eq!(concat("a.b", "c.d"), "a.b.c.d");
        assert_eq!(concat("", "c"), "c");
        assert_eq!(concat("a", ""), "a");
    }
}
