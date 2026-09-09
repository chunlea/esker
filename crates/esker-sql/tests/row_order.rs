//! Contract C3 for order: an index scan must return rows in the order PostgreSQL would.
//!
//! The property tests in `crate::row` check that byte order equals `Datum::pg_cmp`. That is only
//! worth having if `pg_cmp` is right, and this is what makes it so: `tests/corpus/pg19_order.txt`
//! is a real PostgreSQL 19beta1's own `ORDER BY` output, and the keys we build for those values
//! have to sort the same way as plain bytes.
//!
//! The one place the two disagree on purpose is `text`, and the fixture says so: it was captured
//! with `COLLATE "C"`, because a byte-ordered key space is what a project that compiles no C can
//! honestly offer (`crate::row` has the argument). Capturing it under the database's `en_US.utf8`
//! default and calling the difference a rounding error is the thing this file exists to prevent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::row::index_key;
use esker_sql::value::PgDatum;
use esker_sql::value::{ColumnType, Datum};

#[test]
fn encoded_keys_sort_the_way_postgresql_sorts_the_values() {
    let mut types_seen = Vec::new();

    for (line_number, name, values) in fixture() {
        let column_type = column_type(&name);
        types_seen.push(column_type);

        let keyed: Vec<(Datum, Vec<u8>)> = values
            .iter()
            .map(|text| {
                let datum = Datum::from_text(column_type, text).unwrap_or_else(|error| {
                    panic!("line {line_number}: {name} {text:?} did not read back: {error}")
                });
                let key = index_key(1, 2, 3, std::slice::from_ref(&datum), None).unwrap();
                (datum, key)
            })
            .collect();

        for pair in keyed.windows(2) {
            let ((left, left_key), (right, right_key)) = (&pair[0], &pair[1]);
            // PostgreSQL returned these in ascending order, and values it considers equal may
            // come back in either order -- so the requirement is non-decreasing, plus the
            // stronger property that a tie encodes to one key.
            assert!(
                left_key <= right_key,
                "line {line_number}: {name} sorts {left:?} before {right:?}, our keys do not"
            );
            if left.pg_cmp(right).is_eq() {
                assert_eq!(
                    left_key, right_key,
                    "line {line_number}: {left:?} and {right:?} are one value and need one key"
                );
            }
        }
    }

    for ty in ColumnType::ALL {
        // `json` and `jsonb` are the two types that **cannot** be index columns, so there is no
        // key order for a fixture to check: `jsonb`'s equality is not its byte equality, which is
        // exactly why ADR 0042 keeps it out of a key. Exempting them here rather than inventing a
        // fixture is the honest form of this assertion — and it is narrow, so the next type that
        // forgets its fixture still fails.
        // **`hstore` joins them, and its reason is its own.** A `jsonb` cannot be a key because
        // its equality is not its bytes'; an hstore's *is*, and it is its **order** that is not —
        // measured, `'a=>NULL'` sorts first among hstores sharing a key where its canonical text
        // sorts it after `"a"=>"2"`. Half of `text`'s comparison is shared and half is not, which
        // ADR 0042's rule already forbids: a fixture here would have to claim an order the key
        // encoding cannot produce.
        // **The ranges join them, with a third reason.** A `jsonb`'s equality is not its bytes';
        // an hstore's *order* is not its text's; and a range's ordering is PostgreSQL's own —
        // lower bound, then its inclusivity, then upper — which the canonical text does not
        // reproduce. All three are disqualified from being an index key, and a fixture here would
        // have to claim an order the key encoding cannot produce.
        // **And an array is disqualified exactly when its element is.** `json[]` and `jsonb[]`
        // are the only two: an array's order is its elements' order, so the fourteen other array
        // types have fixtures below and these two cannot — `ARRAY['{}'::json] = ARRAY['{}'::json]`
        // is `42883 could not identify an equality operator for type json` on a real server,
        // measured, which is the scalar's own refusal one level up.
        if matches!(
            ty,
            ColumnType::Json
                | ColumnType::Jsonb
                | ColumnType::Hstore
                | ColumnType::HstoreArray
                | ColumnType::TsRange
                | ColumnType::TstzRange
                | ColumnType::Int4Range
                | ColumnType::DateRange
                | ColumnType::NumRange
                | ColumnType::Int8Range
                | ColumnType::TsRangeArray
                | ColumnType::TstzRangeArray
                | ColumnType::Int4RangeArray
                | ColumnType::DateRangeArray
                | ColumnType::NumRangeArray
                | ColumnType::Int8RangeArray
                | ColumnType::JsonArray
                | ColumnType::JsonbArray
                // **And `point`, with the sharpest reason of the four.** `json` has no equality
                // with another type; a point has none *with itself* — `point = point` is `42883`
                // on a real server, and `CREATE INDEX` on one is `42704 data type point has no
                // default operator class for access method "btree"`, which this node now answers
                // too. There is no order for a fixture to check.
                //
                // `point[]` is excluded for a **different** reason and it is a gap rather than a
                // rule: a real server *can* index one (measured — `point[]`, `json[]` and every
                // other array have a default btree opclass there), and this node cannot, because
                // an array key is built out of its element's key encoding and a point has none.
                | ColumnType::Point
                | ColumnType::PointArray
                // `box[]` is excluded for `point[]`'s reason exactly: an array key is its
                // element's key encoding, and a `box` has none. It arrived with the delimiter
                // unit — `type_lookup_test.rb` looks `_box` up by oid — and it is a relation a
                // client reads about rather than one this node indexes.
                | ColumnType::BoxArray
                // **And the five that arrived with ADR 0091, for `point[]`'s reason exactly.** An
                // array key is its element's key encoding and a shape has none. A real server
                // cannot *order* one either — measured, `ORDER BY` over a `circle[]` is
                // `42883 could not identify an ordering operator for type circle[]`, and the
                // message names the array where the equality one names the element — so there is
                // no order for a fixture to check on either side.
                | ColumnType::LsegArray
                | ColumnType::PathArray
                | ColumnType::PolygonArray
                | ColumnType::CircleArray
                | ColumnType::LineArray
                // **The other six geometric shapes, for `point`'s reason and with a real
                // server's agreement**: `CREATE INDEX` on an `lseg` column is
                // `42704 data type lseg has no default operator class for access method "btree"`,
                // and `count(DISTINCT)` over one is
                // `42883 could not identify an equality operator` — both measured, and both true
                // even though the `=` operator itself answers.
                | ColumnType::Lseg
                | ColumnType::Box
                | ColumnType::Path
                | ColumnType::Polygon
                | ColumnType::Circle
                | ColumnType::Line
                // **And `xml`, and `xml[]` with it**, for `json`'s reason exactly: the type has
                // no equality operator at all — `'<a/>'::xml = '<a/>'::xml` is `42883 operator
                // does not exist: xml = xml` — so `ORDER BY` over one is `42883 could not
                // identify an ordering operator for type xml` and there is no order to fix.
                | ColumnType::Xml
                | ColumnType::XmlArray
                // `ltree[]` and not `ltree`: an array key is built from its element's encoding,
                // and the element's here is the separator rewrite, which has nowhere to put the
                // array's own delimiters. The scalar has a fixture two screens up.
                | ColumnType::LtreeArray
                // **A pattern has no comparison at all**, which is `json`'s reason: `lquery`
                // exists to be matched with `~` and never to be stored, ordered or indexed.
                | ColumnType::LQuery
                // **A tsvector's order is not its printed form's.** Measured on 19beta1 over ten
                // values: `'b'` sorts before `'ab'` and `'a':1A` before `'a':1`, where plain bytes
                // give the reverse of both, and a two-lexeme vector lands between two one-lexeme
                // ones. PostgreSQL orders by lexeme length, then bytes, then positions. Equality
                // *is* the text's, so the type is stored, compared and grouped here — it is only
                // the **order** it cannot offer, so it is not an index column and has no fixture.
                | ColumnType::TsVector
                | ColumnType::TsQuery
                | ColumnType::TsVectorArray
                | ColumnType::TsQueryArray
                // A `regtype` is not an index column either, and for a reason of its own: its
                // value is an oid and its name is only how it prints, so a key over one would have
                // to choose between the order the comparison uses and the one a reader expects
                // ([ADR 0077](../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md)).
                | ColumnType::RegType
                | ColumnType::RegTypeArray
                // **And a `regproc` for exactly the same reason one catalog over**: its value is
                // an oid and its name is only how it prints, and the two orders disagree —
                // `int4in` is 42 and `namein` is 34, so the names sort the other way from the
                // oids (ADR 0098).
                | ColumnType::RegProc
                | ColumnType::RegProcArray | ColumnType::RegClassArray
                // A `regclass` is not one for `regtype`'s reason — an oid that prints as a name —
                // and `is_index_key` in `esker_keys::row` refuses it, with the decoder agreeing.
                // **A pseudo-type is not a column**, so there is nothing for a key to order:
                // `CREATE TABLE (c void)` is `42P16` on both servers (ADR 0092).
                | ColumnType::Void
                | ColumnType::RegClass
                // **The two catalog vectors are stored as their text and are never keys**: `indkey`
                // is what they exist for, and no index is built over an index's own column list.
                | ColumnType::Int2Vector
                | ColumnType::OidVector
        ) {
            assert!(
                !types_seen.contains(&ty),
                "{ty:?} has an ordering fixture and cannot be an index column"
            );
            continue;
        }
        assert!(
            types_seen.contains(&ty),
            "{ty:?} has no ordering fixture; a type whose key order is unchecked is a type whose \
             range scans are unchecked"
        );
    }
}

fn column_type(name: &str) -> ColumnType {
    match name {
        "int8" => ColumnType::Int8,
        // Cents in an `i64`, so the order is the integer's — and both ends of the range are in
        // the fixture, because a `money` really can hold either.
        "money" => ColumnType::Money,
        // Family, then address, then prefix — an order no sorting of the printed text gives.
        // Bit by bit, then by length — which the digits' byte order already gives.
        "bit" => ColumnType::Bit,
        "bit varying" => ColumnType::VarBit,
        "bit[]" => ColumnType::BitArray,
        "bit varying[]" => ColumnType::VarBitArray,
        "inet" => ColumnType::Inet,
        "cidr" => ColumnType::Cidr,
        "macaddr" => ColumnType::MacAddr,
        "inet[]" => ColumnType::InetArray,
        "cidr[]" => ColumnType::CidrArray,
        "macaddr[]" => ColumnType::MacAddrArray,
        "money[]" => ColumnType::MoneyArray,
        "citext" => ColumnType::Citext,
        // **The fixture that says the order is not the text's.** `a.b` sits *before* `a-b` here
        // and after it under `COLLATE "C"`, because a real server compares label by label — and
        // the key `esker_keys::row` builds for an ltree lowers the separator below every byte a
        // label can hold, which is what makes plain byte order reproduce this row.
        "ltree" => ColumnType::Ltree,
        "int4" => ColumnType::Int4,
        "int2" => ColumnType::Int2,
        "text" => ColumnType::Text,
        // Captured under `COLLATE "C"` for the same reason `text` is, and separately, because a
        // type whose key order is unchecked is a type whose range scans are unchecked — which is
        // what the exhaustiveness assertion at the end of the test is for.
        "varchar" => ColumnType::Varchar,
        "name" => ColumnType::Name,
        // **`"char"` sorts by byte too**, and its fixture leads with the empty string — a legal
        // `"char"` that is zero characters and not NULL. Measured on the oracle; the digits, the
        // capitals, the lower case and `~` fall in ASCII order, which is what a memcomparable key
        // gives for free (ADR 0076).
        "char" => ColumnType::Char,
        "char[]" => ColumnType::CharArray,
        // Its values are captured already padded, which is what a `character(n)` stores.
        "bpchar" => ColumnType::Bpchar,
        "timestamp" => ColumnType::Timestamp,
        // `1.0` and `1.00` are in the fixture on purpose: PostgreSQL returns them in either
        // order because they are **equal**, and the test's stronger half — a tie encodes to one
        // key — is what proves the index encoding normalises where the row does not.
        "numeric" => ColumnType::Numeric,
        "date" => ColumnType::Date,
        // The top of the range is in the fixture because it is a *value*: `24:00:00` sorts above
        // `23:59:59.999999` rather than being the bound a range scan stops at.
        "time" => ColumnType::Time,
        "uuid" => ColumnType::Uuid,
        // `1 mon` and `30 days` are in the fixture on purpose: they are **equal** — comparison
        // converts a month to thirty days — so the test's stronger half, that a tie encodes to
        // one key, is what proves the index encoding converts where the row does not.
        "interval" => ColumnType::Interval,
        // `4294967295` is in the fixture because it is the value that separates this type from
        // `int4`: unsigned, so it sorts *above* everything and is not negative.
        "oid" => ColumnType::Oid,
        "bool" => ColumnType::Bool,
        "bytea" => ColumnType::Bytea,
        "timestamptz" => ColumnType::TimestampTz,
        "float8" => ColumnType::Double,
        "real" => ColumnType::Real,
        // The five array types, named the way a `CREATE TABLE` declares them.
        "int8[]" => ColumnType::Int8Array,
        "int4[]" => ColumnType::Int4Array,
        "int2[]" => ColumnType::Int2Array,
        "numeric[]" => ColumnType::NumericArray,
        "text[]" => ColumnType::TextArray,
        "bool[]" => ColumnType::BoolArray,
        "bytea[]" => ColumnType::ByteaArray,
        "bpchar[]" => ColumnType::BpcharArray,
        "varchar[]" => ColumnType::VarcharArray,
        // **`name[]`'s capitals sort before its lower case** where `text[]`'s do not: the elements
        // carry `name`'s C collation, which is byte order, and a memcomparable key is already in
        // byte order (ADR 0076). Measured on the oracle rather than copied from `text[]`'s line,
        // because that line has no capitals in it and could not have shown the difference.
        "name[]" => ColumnType::NameArray,
        "citext[]" => ColumnType::CitextArray,
        "date[]" => ColumnType::DateArray,
        "time[]" => ColumnType::TimeArray,
        "timestamp[]" => ColumnType::TimestampArray,
        "timestamptz[]" => ColumnType::TimestampTzArray,
        "interval[]" => ColumnType::IntervalArray,
        "float4[]" => ColumnType::RealArray,
        "float8[]" => ColumnType::DoubleArray,
        "uuid[]" => ColumnType::UuidArray,
        "oid[]" => ColumnType::OidArray,
        other => panic!("the fixture names a type this crate does not have: {other}"),
    }
}

fn fixture() -> impl Iterator<Item = (usize, String, Vec<String>)> {
    include_str!("corpus/pg19_order.txt")
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|(index, line)| {
            let mut fields = line.split('\t');
            let name = fields.next().expect("a type name").to_owned();
            (index + 1, name, fields.map(str::to_owned).collect())
        })
}
