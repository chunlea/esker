//! The expression a fragment filters with, and what it means.
//!
//! A small language on purpose: comparison, three-valued `AND`/`OR`/`NOT`, and `IS NULL`. It is
//! exactly the shape `esker_sql::plan::Expr` carries, minus the parts a filter cannot use —
//! arithmetic is absent there too, and for the same reason recorded in that module: every
//! operator brings its own overflow and type-resolution rules and each is a way to return a
//! confidently wrong number.
//!
//! # Slots, not columns
//!
//! A [`Expr::Column`] names a **projection slot**, never a table column. The projection is the
//! only place a table column index appears in a fragment, so an expression cannot reach a column
//! the fragment did not ask for. That is what makes "decode only what was projected" a property
//! of the format rather than a discipline the evaluator has to keep.
//!
//! # Three-valued logic, and why it is not optional
//!
//! `WHERE` keeps a row only when the expression is definitely **true**. Unknown is not true, and
//! that single rule is the whole of three-valued logic in a filter — the same sentence
//! `esker_sql::exec::cursor` writes above the same code. The tables below are PostgreSQL's:
//!
//! ```text
//! AND   a definite false makes it false whatever the other side is
//! OR    a definite true  makes it true  whatever the other side is
//! NOT   of unknown is unknown
//! =     with any NULL operand is unknown -- which is why `x = NULL` never matches
//! ```
//!
//! `IS NULL` is the exception that proves it: it is never unknown itself, which is the whole
//! point of the operator and the reason `x = NULL` is not a way to write it.

use crate::value::{ColumnType, Value, ValueRef};

/// A comparison. The connectives are nodes of their own, so this is only the six.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `=`.
    Eq,
    /// `<>`, which PostgreSQL also spells `!=`.
    NotEq,
    /// `<`.
    Lt,
    /// `<=`.
    LtEq,
    /// `>`.
    Gt,
    /// `>=`.
    GtEq,
}

impl CompareOp {
    /// The tag byte this operator is stored as. Frozen.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            CompareOp::Eq => 1,
            CompareOp::NotEq => 2,
            CompareOp::Lt => 3,
            CompareOp::LtEq => 4,
            CompareOp::Gt => 5,
            CompareOp::GtEq => 6,
        }
    }

    /// The operator a tag names, or `None` for one no version has written.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(CompareOp::Eq),
            2 => Some(CompareOp::NotEq),
            3 => Some(CompareOp::Lt),
            4 => Some(CompareOp::LtEq),
            5 => Some(CompareOp::Gt),
            6 => Some(CompareOp::GtEq),
            _ => None,
        }
    }

    /// The symbol, for messages.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            CompareOp::Eq => "=",
            CompareOp::NotEq => "<>",
            CompareOp::Lt => "<",
            CompareOp::LtEq => "<=",
            CompareOp::Gt => ">",
            CompareOp::GtEq => ">=",
        }
    }

    /// Whether `ordering` satisfies this operator.
    #[must_use]
    pub fn holds(self, ordering: std::cmp::Ordering) -> bool {
        match self {
            CompareOp::Eq => ordering.is_eq(),
            CompareOp::NotEq => !ordering.is_eq(),
            CompareOp::Lt => ordering.is_lt(),
            CompareOp::LtEq => ordering.is_le(),
            CompareOp::Gt => ordering.is_gt(),
            CompareOp::GtEq => ordering.is_ge(),
        }
    }
}

/// A filter expression over a fragment's projected columns.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// The value of projection slot `n` in the row being evaluated.
    Column(u32),
    /// A constant. `Value::Null` is the untyped NULL, which fits every comparison and makes it
    /// unknown.
    Literal(Value),
    /// A comparison, evaluated with [`ValueRef::pg_cmp`].
    Compare {
        /// Which comparison.
        op: CompareOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// Three-valued `AND`.
    And(Box<Expr>, Box<Expr>),
    /// Three-valued `OR`.
    Or(Box<Expr>, Box<Expr>),
    /// `NOT`, which is unknown when its operand is.
    Not(Box<Expr>),
    /// `x IN (v1, v2, …)` over a **strictly ascending, NULL-free** value list.
    ///
    /// It exists for one caller: a join pushed down as a semi-join, where the values are the inner
    /// side's key set (`docs/plans/phase-16-mpp.md` §J4). Writing that as an `Or` chain of `Eq` is
    /// already possible and does not scale — a few thousand keys against a few hundred thousand
    /// rows is hundreds of millions of comparisons, which loses to the row engine the push-down is
    /// replacing. Ascending order is required rather than sorted on arrival, so the wire form is
    /// canonical, duplicates are refused rather than silently kept, and a row costs a binary
    /// search.
    ///
    /// Three-valued like every other comparison here: **unknown** when the operand is NULL, true
    /// on a `pg_cmp` match, false otherwise. The list itself holds no NULL — `x IN (NULL)` is
    /// unknown for every `x` and is a shape no caller of this wants, so it is refused at decode
    /// rather than evaluated.
    In {
        /// What is being tested.
        operand: Box<Expr>,
        /// The values, strictly ascending in [`Value::pg_cmp`] order.
        values: Vec<Value>,
    },
    /// `x IS NULL`, or `IS NOT NULL` when negated. Never unknown itself.
    IsNull {
        /// What is being tested.
        operand: Box<Expr>,
        /// `IS NOT NULL`.
        negated: bool,
    },
}

impl Expr {
    /// A `slot op literal` comparison, which is the shape most filters are made of.
    #[must_use]
    pub fn compare(slot: u32, op: CompareOp, literal: Value) -> Self {
        Expr::Compare {
            op,
            left: Box::new(Expr::Column(slot)),
            right: Box::new(Expr::Literal(literal)),
        }
    }

    /// How deeply this expression nests, counting itself as one.
    ///
    /// Computed iteratively, with an explicit stack: a recursive depth check on an expression
    /// deep enough to be a problem is itself the problem.
    #[must_use]
    pub fn depth(&self) -> usize {
        let mut deepest = 0;
        let mut stack = vec![(self, 1usize)];
        while let Some((node, depth)) = stack.pop() {
            deepest = deepest.max(depth);
            match node {
                Expr::Column(_) | Expr::Literal(_) => {}
                Expr::Compare { left, right, .. }
                | Expr::And(left, right)
                | Expr::Or(left, right) => {
                    stack.push((left, depth + 1));
                    stack.push((right, depth + 1));
                }
                Expr::Not(operand) | Expr::IsNull { operand, .. } | Expr::In { operand, .. } => {
                    stack.push((operand, depth + 1));
                }
            }
        }
        deepest
    }

    /// The type this expression produces, or `None` when it has none — a NULL literal, which
    /// takes whatever type it is compared against.
    ///
    /// `slots` gives the type of each projection slot. A slot outside it yields `None`, which
    /// [`crate::fragment::Fragment::validate`] has already refused by the time anything evaluates.
    #[must_use]
    pub fn result_type(&self, slots: &[ColumnType]) -> Option<ColumnType> {
        match self {
            Expr::Column(slot) => slots.get(*slot as usize).copied(),
            Expr::Literal(value) => value.column_type(),
            Expr::Compare { .. }
            | Expr::And(..)
            | Expr::Or(..)
            | Expr::Not(_)
            | Expr::IsNull { .. }
            | Expr::In { .. } => Some(ColumnType::Bool),
        }
    }

    /// Evaluates against one row of projected values.
    ///
    /// Total: it cannot fail, because [`crate::fragment::Fragment::validate`] has already refused
    /// every fragment whose types do not line up. Where an impossible shape does reach a
    /// connective anyway, it is read as *unknown* — which filters the row out, the safe direction.
    #[must_use]
    pub fn evaluate<'a>(&'a self, row: &[ValueRef<'a>]) -> ValueRef<'a> {
        match self {
            Expr::Column(slot) => row.get(*slot as usize).copied().unwrap_or(ValueRef::Null),
            Expr::Literal(value) => value.as_ref(),

            Expr::IsNull { operand, negated } => {
                ValueRef::Bool(operand.evaluate(row).is_null() != *negated)
            }

            // **Binary search, which is the whole reason this node exists.** The values are
            // strictly ascending in `pg_cmp` order — refused at decode if they are not — so this
            // is `log n` where the `Or` chain it replaces is `n`.
            Expr::In { operand, values } => {
                let probe = operand.evaluate(row);
                if probe.is_null() {
                    // Unknown, exactly as `x = NULL` is: a NULL is in no list.
                    return ValueRef::Null;
                }
                ValueRef::Bool(
                    values
                        .binary_search_by(|value| value.as_ref().pg_cmp(&probe))
                        .is_ok(),
                )
            }

            Expr::Not(operand) => match truth(operand.evaluate(row)) {
                Some(value) => ValueRef::Bool(!value),
                // NOT of unknown is unknown.
                None => ValueRef::Null,
            },

            // Neither connective is symmetric in its short-circuit: a definite false makes an AND
            // false whatever the other side is, and a definite true makes an OR true.
            Expr::And(left, right) => {
                match (truth(left.evaluate(row)), truth(right.evaluate(row))) {
                    (Some(false), _) | (_, Some(false)) => ValueRef::Bool(false),
                    (Some(true), Some(true)) => ValueRef::Bool(true),
                    _ => ValueRef::Null,
                }
            }
            Expr::Or(left, right) => {
                match (truth(left.evaluate(row)), truth(right.evaluate(row))) {
                    (Some(true), _) | (_, Some(true)) => ValueRef::Bool(true),
                    (Some(false), Some(false)) => ValueRef::Bool(false),
                    _ => ValueRef::Null,
                }
            }

            Expr::Compare { op, left, right } => {
                let (left, right) = (left.evaluate(row), right.evaluate(row));
                // Any NULL operand makes a comparison unknown. This is why `x = NULL` never
                // matches and `IS NULL` has to exist.
                if left.is_null() || right.is_null() {
                    return ValueRef::Null;
                }
                ValueRef::Bool(op.holds(left.pg_cmp(&right)))
            }
        }
    }

    /// Whether this expression is definitely true for `row`, which is what `WHERE` keeps.
    #[must_use]
    pub fn matches(&self, row: &[ValueRef<'_>]) -> bool {
        matches!(self.evaluate(row), ValueRef::Bool(true))
    }

    /// The top-level conjuncts: this expression if it is not an `AND`, otherwise its parts,
    /// flattened.
    ///
    /// What the pruner walks. Only conjunctions count — a comparison under an `OR` constrains
    /// nothing, so a stripe cannot be skipped for it — which is the same rule
    /// `esker_sql::exec::query` applies to required constants, for the same reason.
    pub fn conjuncts<'a>(&'a self, out: &mut Vec<&'a Expr>) {
        match self {
            Expr::And(left, right) => {
                left.conjuncts(out);
                right.conjuncts(out);
            }
            other => out.push(other),
        }
    }
}

/// Reads an evaluated operand as a truth value. Anything that is not a boolean is unknown.
fn truth(value: ValueRef<'_>) -> Option<bool> {
    match value {
        ValueRef::Bool(value) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{CompareOp, Expr, truth};
    use crate::value::{ColumnType, Value, ValueRef};

    fn refs(values: &[Value]) -> Vec<ValueRef<'_>> {
        values.iter().map(Value::as_ref).collect()
    }

    #[test]
    fn comparison_tags_are_frozen() {
        for (op, tag) in [
            (CompareOp::Eq, 1),
            (CompareOp::NotEq, 2),
            (CompareOp::Lt, 3),
            (CompareOp::LtEq, 4),
            (CompareOp::Gt, 5),
            (CompareOp::GtEq, 6),
        ] {
            assert_eq!(op.as_u8(), tag);
            assert_eq!(CompareOp::from_u8(tag), Some(op));
            assert!(!op.symbol().is_empty());
        }
        assert_eq!(CompareOp::from_u8(0), None);
        assert_eq!(CompareOp::from_u8(7), None);
    }

    /// The one rule of three-valued logic in a `WHERE`: unknown is not true.
    #[test]
    fn a_comparison_with_null_is_unknown_and_keeps_nothing() {
        let values = [Value::Null, Value::Int8(5)];
        let row = refs(&values);

        let eq_null = Expr::compare(0, CompareOp::Eq, Value::Null);
        assert!(eq_null.evaluate(&row).is_null());
        assert!(!eq_null.matches(&row), "x = NULL must never match");

        let eq_value = Expr::compare(0, CompareOp::Eq, Value::Int8(5));
        assert!(eq_value.evaluate(&row).is_null(), "NULL = 5 is unknown");
        assert!(!eq_value.matches(&row));

        // IS NULL is the operator that is never unknown.
        let is_null = Expr::IsNull {
            operand: Box::new(Expr::Column(0)),
            negated: false,
        };
        assert!(is_null.matches(&row));
        let is_not_null = Expr::IsNull {
            operand: Box::new(Expr::Column(1)),
            negated: true,
        };
        assert!(is_not_null.matches(&row));
    }

    /// PostgreSQL's tables, both asymmetries included.
    #[test]
    fn the_connectives_are_three_valued() {
        let values = [Value::Bool(true), Value::Bool(false), Value::Null];
        let row = refs(&values);
        let t = || Box::new(Expr::Column(0));
        let f = || Box::new(Expr::Column(1));
        let u = || Box::new(Expr::Column(2));

        assert_eq!(Expr::And(t(), t()).evaluate(&row), ValueRef::Bool(true));
        assert_eq!(Expr::And(t(), f()).evaluate(&row), ValueRef::Bool(false));
        assert_eq!(
            Expr::And(f(), u()).evaluate(&row),
            ValueRef::Bool(false),
            "a definite false makes an AND false whatever the other side is"
        );
        assert!(Expr::And(t(), u()).evaluate(&row).is_null());

        assert_eq!(Expr::Or(f(), f()).evaluate(&row), ValueRef::Bool(false));
        assert_eq!(
            Expr::Or(t(), u()).evaluate(&row),
            ValueRef::Bool(true),
            "a definite true makes an OR true whatever the other side is"
        );
        assert!(Expr::Or(f(), u()).evaluate(&row).is_null());

        assert_eq!(Expr::Not(t()).evaluate(&row), ValueRef::Bool(false));
        assert!(
            Expr::Not(u()).evaluate(&row).is_null(),
            "NOT unknown is unknown"
        );
    }

    /// `NaN` is the largest float here, so it matches `> anything` — the rule the statistics had
    /// to be corrected for.
    #[test]
    fn comparison_uses_this_systems_order() {
        let values = [Value::Double(f64::NAN), Value::Double(-0.0)];
        let row = refs(&values);

        assert!(Expr::compare(0, CompareOp::Gt, Value::Double(5.0)).matches(&row));
        assert!(Expr::compare(0, CompareOp::Gt, Value::Double(f64::INFINITY)).matches(&row));
        assert!(
            Expr::compare(0, CompareOp::Eq, Value::Double(f64::NAN)).matches(&row),
            "NaN equals itself here, unlike IEEE"
        );
        assert!(
            Expr::compare(1, CompareOp::Eq, Value::Double(0.0)).matches(&row),
            "-0.0 equals 0.0"
        );

        // Text sorts by bytes, which is the declared divergence from a collation.
        let text = [Value::Text("B".into())];
        assert!(Expr::compare(0, CompareOp::Lt, Value::Text("a".into())).matches(&refs(&text)));
    }

    #[test]
    fn depth_is_measured_without_recursing() {
        assert_eq!(Expr::Column(0).depth(), 1);
        assert_eq!(Expr::compare(0, CompareOp::Eq, Value::Int8(1)).depth(), 2);

        let mut deep = Expr::Column(0);
        for _ in 0..10_000 {
            deep = Expr::Not(Box::new(deep));
        }
        assert_eq!(
            deep.depth(),
            10_001,
            "a deep expression must not blow the stack"
        );
    }

    #[test]
    fn conjuncts_flatten_only_through_and() {
        let a = Expr::compare(0, CompareOp::Eq, Value::Int8(1));
        let b = Expr::compare(1, CompareOp::Gt, Value::Int8(2));
        let c = Expr::compare(2, CompareOp::Lt, Value::Int8(3));

        let mut out = Vec::new();
        let nested = Expr::And(
            Box::new(Expr::And(Box::new(a.clone()), Box::new(b.clone()))),
            Box::new(c.clone()),
        );
        nested.conjuncts(&mut out);
        assert_eq!(out, vec![&a, &b, &c]);

        // An OR is one conjunct, opaque: nothing under it constrains a stripe.
        let mut out = Vec::new();
        let disjunction = Expr::Or(Box::new(a.clone()), Box::new(b.clone()));
        disjunction.conjuncts(&mut out);
        assert_eq!(out, vec![&disjunction]);
    }

    #[test]
    fn result_types_are_what_validation_reads() {
        let slots = [ColumnType::Int8, ColumnType::Text];
        assert_eq!(Expr::Column(0).result_type(&slots), Some(ColumnType::Int8));
        assert_eq!(Expr::Column(1).result_type(&slots), Some(ColumnType::Text));
        assert_eq!(Expr::Column(9).result_type(&slots), None);
        assert_eq!(
            Expr::Literal(Value::Null).result_type(&slots),
            None,
            "an untyped NULL takes the other side's type"
        );
        assert_eq!(
            Expr::Literal(Value::Double(1.0)).result_type(&slots),
            Some(ColumnType::Double)
        );
        assert_eq!(
            Expr::compare(0, CompareOp::Eq, Value::Int8(1)).result_type(&slots),
            Some(ColumnType::Bool)
        );
        assert_eq!(truth(ValueRef::Int(1)), None, "a non-boolean is unknown");
    }

    /// `IN` and the `Or` chain of `Eq` it replaces must answer identically, including on the
    /// values `pg_cmp` treats specially. The whole point of the node is speed, and a faster
    /// answer that differs is not the same answer.
    #[test]
    fn an_in_list_agrees_with_the_or_chain_it_replaces() {
        let values = vec![Value::Int8(1), Value::Int8(4), Value::Int8(9)];
        let in_list = Expr::In {
            operand: Box::new(Expr::Column(0)),
            values: values.clone(),
        };
        let chain = values
            .iter()
            .map(|value| Expr::compare(0, CompareOp::Eq, value.clone()))
            .reduce(|left, right| Expr::Or(Box::new(left), Box::new(right)))
            .expect("three values reduce");
        for probe in [-1_i64, 0, 1, 2, 4, 5, 9, 10] {
            let row = [ValueRef::Int(probe)];
            assert_eq!(
                in_list.evaluate(&row),
                chain.evaluate(&row),
                "IN and the OR chain disagree at {probe}"
            );
        }
        // A NULL operand is unknown in both, which is what keeps the row out.
        let row = [ValueRef::Null];
        assert!(in_list.evaluate(&row).is_null());
        assert!(chain.evaluate(&row).is_null());
    }

    /// `-0.0` and `0.0` are one value under `pg_cmp` and two under IEEE bit equality, and two
    /// `NaN`s are one value here and never equal under IEEE. A membership test that used bit
    /// equality would split a group the row engine keeps together — the defect class ADR 0040
    /// Decision 5 exists to prevent.
    #[test]
    fn an_in_list_is_pg_cmp_and_not_bit_equality() {
        let zero = Expr::In {
            operand: Box::new(Expr::Column(0)),
            values: vec![Value::Double(0.0)],
        };
        assert_eq!(
            zero.evaluate(&[ValueRef::Double(-0.0)]),
            ValueRef::Bool(true),
            "-0.0 is 0.0 under pg_cmp"
        );
        let nan = Expr::In {
            operand: Box::new(Expr::Column(0)),
            values: vec![Value::Double(f64::NAN)],
        };
        assert_eq!(
            nan.evaluate(&[ValueRef::Double(f64::NAN)]),
            ValueRef::Bool(true),
            "two NaNs are one value under pg_cmp"
        );
    }

    /// The node counts as one level, like every other operand-taking node.
    #[test]
    fn an_in_list_nests_like_its_neighbours() {
        let expr = Expr::In {
            operand: Box::new(Expr::Column(0)),
            values: vec![Value::Int8(1)],
        };
        assert_eq!(expr.depth(), 2);
        assert_eq!(
            Expr::Not(Box::new(expr)).depth(),
            3,
            "a list under a NOT is one deeper"
        );
    }
}
