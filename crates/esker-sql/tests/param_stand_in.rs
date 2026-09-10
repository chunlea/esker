//! **A parameter's stand-in must answer the parameter's type** — wire v3 family **F8**.
//!
//! `Describe` puts a stand-in where the parameter will go, and `exec::bind`'s `placeholder()` had
//! **six** array types in one arm sharing `money`'s element:
//!
//! ```text
//! ColumnType::BitArray | VarBitArray | InetArray | CidrArray | MacAddrArray | MoneyArray
//!     => Datum::Array(ArrayValue::empty(ColumnType::Money))
//! ```
//!
//! So `placeholder(BitArray).column_type()` answered **`money[]`**, the wrapper that carries a
//! parameter's type emitted `CAST(money[] AS bit[])`, and the cast table refused it — correctly,
//! because nothing casts a money array to a bit array. **The stand-in was wrong, not the cast.**
//! The arm below it has always been right for the other forty-odd arrays:
//! `ArrayValue::empty(element_of(ty))`, and `element_of` covers all six.
//!
//! **`varbit[]` was in that arm and had no probe row**, so the census could see the defect on five
//! of the six and call the sixth clean. Two rows were added to
//! `esker-coord/wire-v3-probes.txt`, `expected` measured (`1563`, `bit varying[]`). Measure the
//! arm, not the rows — the same rule that put `least` in the list beside `greatest`.
//!
//! **The `reg*` half of this family is closed and its tests moved.** A test stood in this file
//! pinning the divergence — a bare name beside a `reg*` column answered where 19beta1 refuses —
//! and `tests/reg_comparison.rs` closed it: `pg_operator` has no `=` for any of the three, the
//! comparison is `oid`'s, and an `unknown` beside one is digits and not a name. Deleted rather
//! than kept, because a pin whose divergence is gone is a test that measures the past.
//!
//! What the census called a *parameter* defect was a **comparison** one: the literal and the
//! bound parameter behave identically and the `INSERT` takes the name either way. The shape of
//! the probe is what made it look otherwise.
//!
//! **The array-cast row that was pinned here is closed and its tests moved.** `'{1}'::bit[]::money[]`
//! answered `{$1.00}` where 19beta1 refuses `42846`; `parse::lower`'s literal-array arm read the
//! value's text through the target's `array_in` and returned before the `casts_to` guard.
//! `tests/cast_matrix.rs` is that unit — one check, 3,504 of the matrix's 4,013 diverging rows,
//! none regressed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Frontend, Target};
use esker_sql::pgwire::session::Session;

/// One `Parse · Describe · Bind · Execute · Sync`, the shape a driver sends for `where(col: v)`.
///
/// **The `Sync` is not decoration**: after a failure a session discards every message up to the
/// next one, so an ask that follows a failed ask comes back as the empty string and reads exactly
/// like "no error" (`tests/bind_infers_over_the_wire.rs`).
fn ask(
    node: &mut parity::Node,
    session: &mut Session,
    name: &str,
    sql: &str,
    value: &str,
) -> String {
    let mut send = |message: &Frontend| {
        let mut out = Vec::new();
        session.handle(message, &mut node.executor, &mut out);
        let text = String::from_utf8_lossy(&out).into_owned();
        let parts: Vec<&str> = text.split('\u{0}').collect();
        if parts.iter().any(|p| *p == "SERROR" || *p == "VERROR") {
            let message = parts
                .iter()
                .find(|p| p.starts_with('M'))
                .map_or("", |p| &p[1..]);
            let code = parts
                .iter()
                .find(|p| p.starts_with('C'))
                .map_or("", |p| &p[1..]);
            return format!("!{code} {message}");
        }
        text.chars().filter(|c| !c.is_control()).collect()
    };
    let answer = (|| {
        let parsed = send(&Frontend::Parse {
            statement: name.to_owned(),
            sql: sql.to_owned(),
            param_types: vec![0],
        });
        if parsed.starts_with('!') {
            return format!("AT-PARSE {parsed}");
        }
        let described = send(&Frontend::Describe {
            target: Target::Statement,
            name: name.to_owned(),
        });
        if described.starts_with('!') {
            return format!("AT-DESCRIBE {described}");
        }
        let bound = send(&Frontend::Bind {
            portal: name.to_owned(),
            statement: name.to_owned(),
            param_formats: Vec::new(),
            params: vec![Some(value.as_bytes().to_vec())],
            result_formats: Vec::new(),
        });
        if bound.starts_with('!') {
            return format!("AT-BIND {bound}");
        }
        send(&Frontend::Execute {
            portal: name.to_owned(),
            max_rows: 0,
        })
    })();
    send(&Frontend::Sync);
    answer
}

/// **All six of the arm**, each over its own column, through the protocol a driver uses.
///
/// 19beta1 answers the row for every one (`esker-coord/wire-v3-probes.txt`'s `where_eq` rows, and
/// `varbit[]` measured today at `bit varying[]`); this node refused five of them at `Describe`
/// with `42846 cannot cast type money[] to <the array>`.
#[test]
fn every_array_in_the_arm_takes_its_own_element() {
    for (ty, value) in [
        ("bit[]", "{1}"),
        ("varbit[]", "{101}"),
        ("inet[]", "{127.0.0.1}"),
        ("cidr[]", "{127.0.0.0/24}"),
        ("macaddr[]", "{08:00:2b:01:02:03}"),
        ("money[]", "{$1.00}"),
    ] {
        let mut node = parity::Node::new(&[
            &format!("CREATE TABLE p (c {ty})"),
            &format!("INSERT INTO p VALUES ('{value}')"),
        ]);
        let mut session = Session::new();
        let answer = ask(
            &mut node,
            &mut session,
            "s",
            "SELECT c AS v FROM p WHERE c = $1",
            value,
        );
        assert!(
            !answer.contains('!'),
            "a {ty} parameter must not be stood in for by a money[]: {answer}"
        );
    }
}

/// **The lower bound: the stand-in still carries a type, and a parameter that cannot reach the
/// column still refuses.**
///
/// The sentence the fix must not delete — it was right all along and firing for the wrong reason.
/// A `text` parameter *declared* `text` against a `bit[]` column has no cast on either server.
#[test]
fn a_parameter_that_cannot_reach_the_column_still_refuses() {
    let mut node = parity::Node::new(&["CREATE TABLE p (c bit[])"]);
    let mut session = Session::new();
    let answer = ask(
        &mut node,
        &mut session,
        "s",
        "SELECT c AS v FROM p WHERE c = '{1}'::money[]",
        "{1}",
    );
    assert!(
        answer.contains("42846") || answer.contains("42883"),
        "a money[] beside a bit[] column has no operator and no cast on either server: {answer}"
    );
}
