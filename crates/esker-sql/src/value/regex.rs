//! POSIX extended regular expressions, for `~`, `~*`, `!~` and `!~*` — written here rather than
//! bought.
//!
//! `deny.toml` forbids a regex crate, and this needs a small part of one: `ActiveRecord`'s
//! PostgreSQL adapter sends exactly four patterns, and between them they use an anchor, `.`, `*`
//! and alternation. The subset built here is that plus its natural closure — character classes
//! with ranges and `[[:name:]]`, grouping, `+`, `?`, `{m}`/`{m,n}` and `\` escapes — and anything
//! outside it is refused by name (ADR 0031) rather than approximated.
//!
//! # Why a Thompson NFA rather than backtracking
//!
//! A pattern is **user input**, and the natural recursive matcher is quadratic-to-exponential on
//! shapes like `(a*)*b` — the same trap `like_matches` avoided by keeping one backtrack point.
//! Here the pattern is richer, so the matcher is a Thompson construction simulated with a thread
//! list: every position of the subject is visited once and every instruction at most once per
//! position, which is `O(subject × program)` with no recursion and no stack to overflow.
//!
//! The **parser** is iterative too, with an explicit stack of alternation contexts, so a pattern of
//! a thousand nested groups is a large `Vec` rather than a thousand stack frames — `CLAUDE.md`
//! forbids a panic on user input, and a recursive-descent parser over `((((…` is exactly that.
//!
//! # What the capture pinned, and this reproduces
//!
//! * **`.` matches a newline.** POSIX says so and most engines outside it do not; `E'a\nb' ~ 'a.b'`
//!   is true on a real server.
//! * **`^` and `$` are string anchors**, not line anchors: `E'a\nb' ~ '^a.b$'` is true, so `$` did
//!   not stop at the newline.
//! * **The empty pattern matches everything**, and a pattern is a *search* rather than a whole
//!   match unless it is anchored.
//! * **`]` first in a class is a literal**: `[]]` is the class containing `]`.
//! * **`\a` is not `a`.** A backslash before an ordinary letter is an escape with its own meaning,
//!   so escaping is not "drop the backslash" — `'a' ~ '^\a$'` is **false**.

use crate::error::SqlError;

/// How many instructions a compiled pattern may hold.
///
/// Bounds are expanded by copying, so `a{100}{100}{100}` is a million states from twelve
/// characters. The limit is what turns that into a named refusal instead of a memory spike.
const PROGRAM_LIMIT: usize = 10_000;

/// One instruction of the compiled program.
#[derive(Debug, Clone)]
enum Inst {
    /// Consume one character the class admits, then go to the target.
    ///
    /// **The target is explicit rather than `pc + 1`.** A bound is expanded by copying a fragment
    /// to the end of the program, so an instruction's successor is not the one written after it —
    /// and a matcher that assumed otherwise would follow `a{2}` into whatever was pasted next.
    Class(Class, usize),
    /// Two ways on, both tried at this position.
    Split(usize, usize),
    /// One way on.
    Jump(usize),
    /// The subject's start, then the target.
    AssertStart(usize),
    /// The subject's end, then the target.
    AssertEnd(usize),
    /// The pattern is satisfied.
    Match,
}

/// What one character position accepts.
#[derive(Debug, Clone)]
struct Class {
    /// Literal characters and ranges. `.` is the empty class with `negated`, which admits every
    /// character **including a newline** — POSIX's rule and the one a copied engine gets wrong.
    ranges: Vec<(char, char)>,
    /// `[^…]`, and `.`.
    negated: bool,
}

impl Class {
    fn any() -> Self {
        Class {
            ranges: Vec::new(),
            negated: true,
        }
    }

    fn literal(c: char) -> Self {
        Class {
            ranges: vec![(c, c)],
            negated: false,
        }
    }

    /// Whether this class admits `c`, folding when the operator is one of the `*` pair.
    ///
    /// Folding is applied to the **subject** rather than to the pattern, so a range keeps the
    /// bounds it was written with: `[a-z]` under `~*` admits `F` because `f` is tried as well.
    /// ASCII is what the captures use; a fold outside it is the one place this is approximate, and
    /// it is approximate in the direction of the simple rule rather than of a table.
    fn admits(&self, c: char, case_insensitive: bool) -> bool {
        if self.holds(c) {
            return true;
        }
        if !case_insensitive {
            return false;
        }
        c.to_lowercase().any(|folded| self.holds(folded))
            || c.to_uppercase().any(|folded| self.holds(folded))
    }

    fn holds(&self, c: char) -> bool {
        let inside = self.ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi);
        inside != self.negated
    }
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub struct Regex {
    program: Vec<Inst>,
    /// Where the program begins. **Not always `0`**: an alternation emits its `Split` after the
    /// branches it chooses between, so the last instruction written is often the first to run.
    start: usize,
}

impl Regex {
    /// Whether the pattern matches anywhere in `subject`.
    ///
    /// **A search, not a whole match**: a new thread starts at every position, which is what makes
    /// `'abc' ~ 'b'` true and what `^`/`$` are for. The empty pattern therefore matches everything,
    /// which is measured.
    #[must_use]
    pub fn is_match(&self, subject: &str, case_insensitive: bool) -> bool {
        let chars: Vec<char> = subject.chars().collect();
        let mut current: Vec<usize> = Vec::new();
        let mut next: Vec<usize> = Vec::new();
        let mut seen = vec![usize::MAX; self.program.len()];

        for at in 0..=chars.len() {
            // A fresh start at every position is the unanchored search; `AssertStart` is what
            // stops a `^` pattern from taking it.
            self.add(&mut current, self.start, at, chars.len(), &mut seen, at);
            if current
                .iter()
                .any(|&pc| matches!(self.program[pc], Inst::Match))
            {
                return true;
            }
            if at == chars.len() {
                break;
            }
            next.clear();
            for &pc in &current {
                if let Inst::Class(class, target) = &self.program[pc]
                    && class.admits(chars[at], case_insensitive)
                {
                    next.push(*target);
                }
            }
            core::mem::swap(&mut current, &mut next);
            // The thread list for the next position is seeded from these, and `add` below expands
            // them; the `seen` generation is the position, so nothing is added twice.
            let seeded = core::mem::take(&mut current);
            for pc in seeded {
                self.add(&mut current, pc, at + 1, chars.len(), &mut seen, at + 1);
            }
        }
        current
            .iter()
            .any(|&pc| matches!(self.program[pc], Inst::Match))
    }

    /// Adds a thread and everything it reaches without consuming a character.
    ///
    /// Iterative, with its own stack: a program with ten thousand `Split`s must not become ten
    /// thousand stack frames. `seen` is stamped with the position so one pass never re-adds a
    /// state, which is what bounds the work at `O(program)` per character.
    fn add(
        &self,
        list: &mut Vec<usize>,
        start: usize,
        at: usize,
        len: usize,
        seen: &mut [usize],
        generation: usize,
    ) {
        let mut pending = vec![start];
        while let Some(pc) = pending.pop() {
            if seen[pc] == generation {
                continue;
            }
            seen[pc] = generation;
            match &self.program[pc] {
                Inst::Jump(to) => pending.push(*to),
                Inst::Split(a, b) => {
                    pending.push(*b);
                    pending.push(*a);
                }
                Inst::AssertStart(target) => {
                    if at == 0 {
                        pending.push(*target);
                    }
                }
                Inst::AssertEnd(target) => {
                    if at == len {
                        pending.push(*target);
                    }
                }
                Inst::Class(..) | Inst::Match => list.push(pc),
            }
        }
    }
}

/// One piece of a pattern under construction: where it starts, and the holes to be patched.
#[derive(Debug, Clone)]
struct Fragment {
    start: usize,
    /// Instruction slots whose target is not known yet, as `(index, which)` — `which` is `0` for a
    /// `Jump`'s only target or a `Split`'s first, `1` for a `Split`'s second.
    holes: Vec<(usize, u8)>,
}

/// Compiles a POSIX ERE, or says why it will not.
///
/// # Errors
///
/// [`SqlError::InvalidRegex`] with PostgreSQL's own sentence for a malformed pattern, and
/// [`SqlError::FeatureNotSupported`] naming the construct for one outside the subset.
pub fn compile(pattern: &str) -> Result<Regex, SqlError> {
    Compiler::new(pattern).run()
}

/// The iterative parser and emitter, in one pass.
struct Compiler {
    chars: Vec<char>,
    at: usize,
    program: Vec<Inst>,
    /// The alternation being built outside every group: the alternatives closed so far by `|`,
    /// and the concatenation in progress.
    ///
    /// **A field rather than the bottom of the stack**, so that "there is always a context" is a
    /// fact about the type instead of an `expect` in six places — `CLAUDE.md` allows one only on
    /// an invariant proven in the same function, and this one is proven by construction.
    outer: Context,
    /// One entry per open group, innermost last.
    groups: Vec<Context>,
}

/// One alternation under construction.
type Context = (Vec<Fragment>, Option<Fragment>);

/// A fragment lifted out of the program so a bound can paste it back: its instructions, the index
/// they were lifted from, and its holes relative to that index.
type Body = (Vec<Inst>, usize, Vec<(usize, u8)>);

impl Compiler {
    fn new(pattern: &str) -> Self {
        Compiler {
            chars: pattern.chars().collect(),
            at: 0,
            program: Vec::new(),
            outer: (Vec::new(), None),
            groups: Vec::new(),
        }
    }

    /// The innermost alternation being built, which always exists.
    fn context(&mut self) -> &mut Context {
        self.groups.last_mut().unwrap_or(&mut self.outer)
    }

    fn run(mut self) -> Result<Regex, SqlError> {
        while self.at < self.chars.len() {
            let c = self.chars[self.at];
            match c {
                '(' => {
                    self.refuse_advanced()?;
                    self.at += 1;
                    self.groups.push((Vec::new(), None));
                    continue;
                }
                ')' => {
                    if self.groups.is_empty() {
                        return Err(unbalanced_parentheses());
                    }
                    self.at += 1;
                    let group = self.close_alternation();
                    let group = self.quantified(group)?;
                    self.concat(group);
                    continue;
                }
                '|' => {
                    self.at += 1;
                    let done = self.take_concat();
                    self.context().0.push(done);
                    continue;
                }
                _ => {}
            }
            let atom = self.atom()?;
            let atom = self.quantified(atom)?;
            self.concat(atom);
        }
        if !self.groups.is_empty() {
            return Err(unbalanced_parentheses());
        }
        let whole = self.close_alternation();
        let matched = self.emit(Inst::Match);
        self.patch(&whole.holes, matched);
        Ok(Regex {
            start: whole.start,
            program: self.program,
        })
    }

    /// `(?…)` and the rest of PostgreSQL's *advanced* regular expressions, which are not ERE.
    fn refuse_advanced(&self) -> Result<(), SqlError> {
        if self.chars.get(self.at + 1) == Some(&'?') {
            return Err(SqlError::unsupported(
                "an advanced regular expression, which is PostgreSQL's own extension to POSIX",
            ));
        }
        Ok(())
    }

    /// One atom: a class, `.`, an anchor, an escape, or a literal.
    fn atom(&mut self) -> Result<Fragment, SqlError> {
        let c = self.chars[self.at];
        self.at += 1;
        let class = match c {
            '.' => Class::any(),
            '^' => return Ok(self.assertion(Inst::AssertStart(0))),
            '$' => return Ok(self.assertion(Inst::AssertEnd(0))),
            '[' => self.character_class()?,
            // A quantifier with nothing to quantify. `{` is in the same arm: a bound at the
            // start of a branch has no operand either.
            '*' | '+' | '?' | '{' => return Err(quantifier_operand_invalid()),
            '\\' => self.escape()?,
            other => Class::literal(other),
        };
        let start = self.emit(Inst::Class(class, 0));
        Ok(Fragment {
            start,
            holes: vec![(start, 0)],
        })
    }

    /// `^` or `$`: a zero-width instruction, which still needs a fragment to concatenate.
    fn assertion(&mut self, inst: Inst) -> Fragment {
        let start = self.emit(inst);
        Fragment {
            start,
            holes: vec![(start, 0)],
        }
    }

    /// `\.`, `\*` — and `\a`, which is **not** `a`.
    ///
    /// **Escaping is not "drop the backslash".** A backslash before punctuation quotes it, but
    /// before a letter it is one of PostgreSQL's character escapes with its own meaning: `\a` is
    /// the alert character, which is why `'a' ~ '^\a$'` is **false**. Measured, and an
    /// implementation that stripped the backslash would answer true.
    ///
    /// The class shorthands `\d`, `\s`, `\w` and the rest of PostgreSQL's *advanced* escapes are
    /// not POSIX ERE and are refused by name; nothing in the captures writes one.
    fn escape(&mut self) -> Result<Class, SqlError> {
        let Some(&c) = self.chars.get(self.at) else {
            return Err(SqlError::InvalidRegex(
                "trailing backslash on RE".to_owned(),
            ));
        };
        self.at += 1;
        let quoted = match c {
            'a' => '\u{7}',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\u{b}',
            'e' => '\u{1b}',
            other if other.is_ascii_alphanumeric() => {
                return Err(SqlError::unsupported(format!(
                    "the regular-expression escape \\{other}"
                )));
            }
            other => other,
        };
        Ok(Class::literal(quoted))
    }

    /// `[abc]`, `[^a-z]`, `[[:alpha:]]`, and `[]]` — where a leading `]` is a literal.
    fn character_class(&mut self) -> Result<Class, SqlError> {
        let mut ranges = Vec::new();
        let negated = self.chars.get(self.at) == Some(&'^');
        if negated {
            self.at += 1;
        }
        let mut first = true;
        loop {
            let Some(&c) = self.chars.get(self.at) else {
                return Err(unbalanced_brackets());
            };
            if c == ']' && !first {
                self.at += 1;
                return Ok(Class { ranges, negated });
            }
            first = false;
            // `[:alpha:]` inside the brackets, which is POSIX's named class.
            if c == '[' && self.chars.get(self.at + 1) == Some(&':') {
                self.at += 2;
                let mut name = String::new();
                while let Some(&c) = self.chars.get(self.at) {
                    if c == ':' {
                        break;
                    }
                    name.push(c);
                    self.at += 1;
                }
                if self.chars.get(self.at) != Some(&':')
                    || self.chars.get(self.at + 1) != Some(&']')
                {
                    return Err(unbalanced_brackets());
                }
                self.at += 2;
                ranges.extend(named_class(&name)?);
                continue;
            }
            self.at += 1;
            let lo = if c == '\\' {
                let escaped = self.escape()?;
                escaped.ranges.first().map_or(c, |&(lo, _)| lo)
            } else {
                c
            };
            // A `-` that is not last opens a range.
            if self.chars.get(self.at) == Some(&'-')
                && self.chars.get(self.at + 1).is_some_and(|&c| c != ']')
            {
                self.at += 1;
                let hi = self.chars[self.at];
                self.at += 1;
                if hi < lo {
                    return Err(SqlError::InvalidRegex("invalid character range".to_owned()));
                }
                ranges.push((lo, hi));
            } else {
                ranges.push((lo, lo));
            }
        }
    }

    /// `*`, `+`, `?`, `{m}` and `{m,n}` applied to the fragment just built, as many as are written.
    fn quantified(&mut self, mut fragment: Fragment) -> Result<Fragment, SqlError> {
        loop {
            match self.chars.get(self.at) {
                Some('*') => {
                    self.at += 1;
                    fragment = self.repeat(&fragment, 0, None)?;
                }
                Some('+') => {
                    self.at += 1;
                    fragment = self.repeat(&fragment, 1, None)?;
                }
                Some('?') => {
                    self.at += 1;
                    fragment = self.repeat(&fragment, 0, Some(1))?;
                }
                Some('{') if self.bound_follows() => {
                    let (least, most) = self.bound()?;
                    fragment = self.repeat(&fragment, least, most)?;
                }
                _ => return Ok(fragment),
            }
        }
    }

    /// Whether the `{` at the cursor opens a bound rather than being a literal brace.
    ///
    /// PostgreSQL takes a `{` that is not a valid bound as an ordinary character, which is why the
    /// shape is looked at before it is read.
    fn bound_follows(&self) -> bool {
        let mut at = self.at + 1;
        let digits = core::iter::from_fn(|| {
            let c = *self.chars.get(at)?;
            c.is_ascii_digit().then(|| {
                at += 1;
                c
            })
        })
        .count();
        digits > 0 && matches!(self.chars.get(at), Some(',' | '}'))
    }

    /// `{m}` or `{m,n}` or `{m,}`.
    fn bound(&mut self) -> Result<(usize, Option<usize>), SqlError> {
        self.at += 1;
        let least = self.number();
        let most = match self.chars.get(self.at) {
            Some('}') => Some(least),
            Some(',') => {
                self.at += 1;
                if self.chars.get(self.at) == Some(&'}') {
                    None
                } else {
                    Some(self.number())
                }
            }
            _ => return Err(unbalanced_brackets()),
        };
        if self.chars.get(self.at) != Some(&'}') {
            return Err(unbalanced_brackets());
        }
        self.at += 1;
        if most.is_some_and(|most| most < least) {
            return Err(SqlError::InvalidRegex(
                "invalid repetition count(s)".to_owned(),
            ));
        }
        Ok((least, most))
    }

    fn number(&mut self) -> usize {
        let mut value: usize = 0;
        while let Some(digit) = self.chars.get(self.at).and_then(|c| c.to_digit(10)) {
            value = value.saturating_mul(10).saturating_add(digit as usize);
            self.at += 1;
        }
        value
    }

    /// A fragment repeated between `least` and `most` times, by copying it.
    ///
    /// Copying is what keeps the simulator a plain NFA with no counters, and [`PROGRAM_LIMIT`] is
    /// what keeps a copied bound from being a way to ask for a gigabyte.
    fn repeat(
        &mut self,
        fragment: &Fragment,
        least: usize,
        most: Option<usize>,
    ) -> Result<Fragment, SqlError> {
        let body = self.extract(fragment);
        let mut built: Option<Fragment> = None;
        for _ in 0..least {
            let copy = self.paste(&body);
            built = Some(match built {
                None => copy,
                Some(before) => self.join(&before, copy),
            });
        }
        match most {
            // `*` and `+`: a loop.
            None => {
                let split = self.emit(Inst::Split(0, 0));
                let copy = self.paste(&body);
                self.program[split] = Inst::Split(copy.start, 0);
                let jump = self.emit(Inst::Jump(split));
                self.patch(&copy.holes, jump);
                let looped = Fragment {
                    start: split,
                    holes: vec![(split, 1)],
                };
                built = Some(match built {
                    None => looped,
                    Some(before) => self.join(&before, looped),
                });
            }
            Some(most) => {
                for _ in least..most {
                    let split = self.emit(Inst::Split(0, 0));
                    let copy = self.paste(&body);
                    self.program[split] = Inst::Split(copy.start, 0);
                    let optional = Fragment {
                        start: split,
                        holes: [vec![(split, 1)], copy.holes].concat(),
                    };
                    built = Some(match built {
                        None => optional,
                        Some(before) => self.join(&before, optional),
                    });
                }
            }
        }
        if self.program.len() > PROGRAM_LIMIT {
            return Err(SqlError::unsupported(
                "a regular expression whose repetition bounds expand past this server's limit",
            ));
        }
        Ok(built.unwrap_or_else(|| {
            // `{0}`: matches the empty string and nothing else.
            let jump = self.emit(Inst::Jump(0));
            Fragment {
                start: jump,
                holes: vec![(jump, 0)],
            }
        }))
    }

    /// Lifts a fragment's instructions out of the program so they can be pasted back repeatedly.
    fn extract(&mut self, fragment: &Fragment) -> Body {
        let from = fragment.start;
        let body: Vec<Inst> = self.program.split_off(from);
        let holes = fragment
            .holes
            .iter()
            .map(|&(at, which)| (at - from, which))
            .collect();
        (body, from, holes)
    }

    /// Pastes a lifted fragment back at the end of the program.
    fn paste(&mut self, body: &Body) -> Fragment {
        let (instructions, origin, holes) = body;
        let base = self.program.len();
        for inst in instructions {
            // Every target is relative to where the fragment was lifted from. A hole's target is
            // meaningless until it is patched, and rebasing it is harmless because the patch
            // overwrites it.
            let rebase = |to: &usize| to.saturating_sub(*origin) + base;
            self.program.push(match inst {
                Inst::Jump(to) => Inst::Jump(rebase(to)),
                Inst::Split(a, b) => Inst::Split(rebase(a), rebase(b)),
                Inst::Class(class, to) => Inst::Class(class.clone(), rebase(to)),
                Inst::AssertStart(to) => Inst::AssertStart(rebase(to)),
                Inst::AssertEnd(to) => Inst::AssertEnd(rebase(to)),
                Inst::Match => Inst::Match,
            });
        }
        Fragment {
            start: base,
            holes: holes
                .iter()
                .map(|&(at, which)| (at + base, which))
                .collect(),
        }
    }

    /// Two fragments one after the other.
    fn join(&mut self, first: &Fragment, second: Fragment) -> Fragment {
        self.patch(&first.holes, second.start);
        Fragment {
            start: first.start,
            holes: second.holes,
        }
    }

    /// Adds an atom to the concatenation in progress.
    fn concat(&mut self, atom: Fragment) {
        let before = self.context().1.take();
        let joined = match before {
            None => atom,
            Some(before) => self.join(&before, atom),
        };
        self.context().1 = Some(joined);
    }

    /// The concatenation in progress, or an empty fragment where nothing was written.
    fn take_concat(&mut self) -> Fragment {
        let taken = self.context().1.take();
        taken.unwrap_or_else(|| {
            // **An empty branch matches the empty string**, which is what makes `'abc' ~ ''` true
            // and `(a|)` legal.
            let jump = self.emit(Inst::Jump(0));
            Fragment {
                start: jump,
                holes: vec![(jump, 0)],
            }
        })
    }

    /// Closes the innermost context into one fragment: its alternatives, tried in order.
    fn close_alternation(&mut self) -> Fragment {
        let last = self.take_concat();
        let (mut alternatives, _) = self
            .groups
            .pop()
            .unwrap_or_else(|| core::mem::take(&mut self.outer));
        alternatives.push(last);
        let mut built: Option<Fragment> = None;
        for alternative in alternatives.into_iter().rev() {
            built = Some(match built {
                None => alternative,
                Some(rest) => {
                    let split = self.emit(Inst::Split(alternative.start, rest.start));
                    Fragment {
                        start: split,
                        holes: [alternative.holes, rest.holes].concat(),
                    }
                }
            });
        }
        // `alternatives` was pushed to just above, so the loop ran at least once. An empty
        // pattern still reaches here with the empty fragment `take_concat` builds.
        built.unwrap_or_else(|| {
            let jump = self.emit(Inst::Jump(0));
            Fragment {
                start: jump,
                holes: vec![(jump, 0)],
            }
        })
    }

    fn emit(&mut self, inst: Inst) -> usize {
        self.program.push(inst);
        self.program.len() - 1
    }

    fn patch(&mut self, holes: &[(usize, u8)], to: usize) {
        for &(at, which) in holes {
            match (&mut self.program[at], which) {
                (
                    Inst::Jump(target)
                    | Inst::Split(target, _)
                    | Inst::Class(_, target)
                    | Inst::AssertStart(target)
                    | Inst::AssertEnd(target),
                    0,
                )
                | (Inst::Split(_, target), 1) => *target = to,
                _ => {}
            }
        }
    }
}

/// The character ranges a `[[:name:]]` class stands for.
fn named_class(name: &str) -> Result<Vec<(char, char)>, SqlError> {
    Ok(match name {
        "alpha" => vec![('a', 'z'), ('A', 'Z')],
        "digit" => vec![('0', '9')],
        "alnum" => vec![('a', 'z'), ('A', 'Z'), ('0', '9')],
        "upper" => vec![('A', 'Z')],
        "lower" => vec![('a', 'z')],
        "space" => vec![(' ', ' '), ('\t', '\r')],
        "punct" => vec![('!', '/'), (':', '@'), ('[', '`'), ('{', '~')],
        "xdigit" => vec![('0', '9'), ('a', 'f'), ('A', 'F')],
        other => {
            return Err(SqlError::unsupported(format!(
                "the character class [:{other}:]"
            )));
        }
    })
}

/// PostgreSQL's own sentences, which name **which** bracket is unbalanced.
fn unbalanced_brackets() -> SqlError {
    SqlError::InvalidRegex("brackets [] not balanced".to_owned())
}

fn unbalanced_parentheses() -> SqlError {
    SqlError::InvalidRegex("parentheses () not balanced".to_owned())
}

fn quantifier_operand_invalid() -> SqlError {
    SqlError::InvalidRegex("quantifier operand invalid".to_owned())
}

#[cfg(test)]
mod tests {
    use super::compile;

    fn matches(subject: &str, pattern: &str) -> bool {
        compile(pattern)
            .expect("the pattern compiles")
            .is_match(subject, false)
    }

    #[test]
    fn a_pattern_is_a_search_until_it_is_anchored() {
        assert!(matches("abc", "b"));
        assert!(!matches("abc", "^b"));
        assert!(matches("abc", "^abc$"));
        assert!(!matches("xabc", "^abc$"));
    }

    /// **The empty pattern matches everything**, which is measured and is the opposite of what a
    /// "no elements, no match" reading would give.
    #[test]
    fn the_empty_pattern_matches_everything() {
        assert!(matches("abc", ""));
        assert!(matches("", ""));
    }

    /// **`.` matches a newline**, which POSIX says and most engines do not.
    #[test]
    fn dot_matches_a_newline() {
        assert!(matches("a\nb", "a.b"));
        assert!(matches("a\nb", "^a.b$"));
        assert!(!matches("", "."));
    }

    #[test]
    fn a_leading_bracket_inside_a_class_is_a_literal() {
        assert!(matches("a]b", "^a[]]b$"));
    }

    /// A shape a backtracking matcher goes exponential on, and this one does not.
    #[test]
    fn a_pathological_pattern_still_answers() {
        assert!(!matches(&"a".repeat(30), "^(a*)*b$"));
        assert!(matches(&"a".repeat(30), "^(a*)*$"));
    }
}
