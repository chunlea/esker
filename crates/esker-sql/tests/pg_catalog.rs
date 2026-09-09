//! Contract C3 for `pg_catalog`, and the first slice of it: `pg_type` and `pg_range`.
//!
//! The scoreboard ranked these two first and gave the reason: four of `ActiveRecord`'s 36 boot
//! statements read them, they are the *first* thing it asks for, and rung 2 of the ladder stopped
//! on `relation "pg_type" does not exist` — so they are the only item that could move it
//! (`docs/bench/rails-scoreboard.md`).
//!
//! The corpus is 46 statements put to a real PostgreSQL 19beta1 in one session and replayed the
//! same way against one node, with no fixture: both relations are built in. What it pins is that a
//! **computed** relation is a relation like any other — an alias, a qualifier, `WHERE`, `IN`,
//! `ORDER BY`, `DISTINCT`, `GROUP BY`, `count(*)` and both kinds of join all work over rows that
//! came from nowhere, because the last three units built them over rows and not over a scan.
//!
//! **41 of the 46 agreed on the first run.** The five that did not are the two decisions this unit
//! had to make, and both are declared below rather than worked around.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: `pg_type` and `pg_range` are built in.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why. Two decisions, and 28 statements of one fact.
///
/// * **`pg_type` lists the types this server has** — six, with PostgreSQL's own OIDs. A real
///   server has 669 in this database. The decision and its evidence are at the top of
///   `tests/corpus/pg19_pg_catalog.txt`; the short version is that it was taken from
///   `ActiveRecord`'s own source rather than from taste, and that a short map costs a client
///   nothing it can see because **this node never sends an OID that is not in it**.
/// * **Every write is `42501`**, DML included. A real server refuses the DDL exactly this way and
///   lets a *superuser* run the DML — which is how a capture probe took `int8` out of the
///   container and broke the database. There are no roles here and a computed relation has
///   nothing to write to, so all four verbs get the answer a real server gives everyone who is not
///   a superuser.
/// * And the `types` list, which is 28 statements of the same sentence: a real server's
///   `pg_type.oid` is an `oid`, `typname` is a `name`, `typdelim` and `typtype` are `"char"` and
///   `typinput` is a `regproc`. This node has none of those four types, so an `oid` is a `bigint`
///   and the other three are `text`. **Every value is identical** — the harness only reaches this
///   list when the rows already agree — and what differs is the OID in `RowDescription`.
///   `ActiveRecord` reads all five columns with `.to_i` or a string comparison, so nothing it does
///   can see it.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'int8'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'text'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'bool'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'bytea'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'float8'",
        "SELECT oid, typname, typelem, typdelim, typinput, typtype, typbasetype FROM pg_type WHERE typname = 'timestamptz'",
        "SELECT typname FROM pg_type WHERE typname IN ('int8', 'text', 'bool', 'bytea', 'float8', 'timestamptz') ORDER BY oid",
        "SELECT oid FROM pg_type WHERE typname IN ('int8', 'text', 'bool', 'bytea', 'float8', 'timestamptz') ORDER BY oid",
        "SELECT typname FROM pg_type WHERE typname = 'nosuchtype'",
        "SELECT t.oid FROM pg_type AS t WHERE t.typname = 'int8'",
        "SELECT t.typname FROM pg_type t WHERE t.oid = 20",
        "SELECT typname FROM pg_type WHERE oid = 20",
        "SELECT typtype FROM pg_type WHERE typname = 'int8'",
        "SELECT typdelim FROM pg_type WHERE typname = 'int8'",
        "SELECT typinput FROM pg_type WHERE typname = 'bool'",
        "SELECT typinput FROM pg_type WHERE typname = 'timestamptz'",
        "SELECT typelem, typbasetype FROM pg_type WHERE typname = 'text'",
        "SELECT oid FROM pg_type WHERE typname = 'int8' AND typtype = 'b'",
        "SELECT oid FROM pg_type WHERE typtype IN ('r', 'e', 'd') AND typname IN ('int8', 'text')",
        "SELECT oid FROM pg_type WHERE typelem IN (16, 17) AND typname IN ('int8', 'text')",
        "SELECT typname FROM pg_type WHERE typname IN ('int8', 'text') ORDER BY typname DESC",
        "SELECT DISTINCT typtype FROM pg_type WHERE typname IN ('int8', 'text', 'bool')",
        "SELECT typtype, count(*) FROM pg_type WHERE typname IN ('int8', 'text', 'bool') GROUP BY typtype",
        "SELECT rngsubtype FROM pg_range WHERE rngtypid = 20",
        "SELECT t.typname, r.rngsubtype FROM pg_type AS t LEFT JOIN pg_range AS r ON t.oid = r.rngtypid WHERE t.typname = 'int8'",
        "SELECT t.typname, r.rngsubtype FROM pg_type AS t LEFT JOIN pg_range AS r ON oid = rngtypid WHERE t.typname IN ('int8', 'text') ORDER BY t.oid",
        "SELECT t.typname FROM pg_type AS t JOIN pg_range AS r ON oid = rngtypid WHERE t.typname = 'int8'",
        "SELECT typname FROM pg_type WHERE typname = 'int8'",
    ],
    answers: &[
        (
            "SELECT typname FROM pg_type WHERE typname IN ('int8', 'numeric') ORDER BY typname",
            "**this server's `pg_type` lists this server's types.** A real server answers `int8` \
             and `numeric`; this one answers `int8`, because `numeric` is a type it refuses by \
             name. Listing PostgreSQL's standard OIDs instead would tell a client this node has \
             `numeric` and `int4`, which is a wrong answer rather than a short one. It closes one \
             type at a time as types arrive.",
            "pg19_pg_catalog.txt:59",
        ),
        (
            "SELECT typname FROM pg_type WHERE typname = 'numeric'",
            "the same, on its own and in the shape a client actually asks it: one row there, no \
             rows here.",
            "pg19_pg_catalog.txt:60",
        ),
        (
            "INSERT INTO pg_type (oid, typname) VALUES (99, 'nope')",
            "**every write to a catalog relation is `42501` here.** A real server refuses the \
             *DDL* exactly that way and permits this — it is a superuser session, and PostgreSQL \
             lets a superuser DML a system catalog; the row is refused only by a `NOT NULL` on a \
             column this node's `pg_type` does not have. There are no roles here and a computed \
             relation has nothing to write to, so the answer is the one a real server gives \
             everyone who is not a superuser. `docs/plans/phase-9-rails.md` §5.",
            "pg19_pg_catalog.txt:104",
        ),
        (
            "UPDATE pg_type SET typname = 'nope' WHERE typname = 'int8'",
            "the same, and here a real server **succeeds**. This is not a hypothetical: the \
             capture that produced this corpus ran the `DELETE` below without a transaction the \
             first time, `int8` left the database, and every later statement answered `XX000 \
             cache lookup failed for type 20`. The three write probes are inside rolled-back \
             blocks for that reason.",
            "pg19_pg_catalog.txt:107",
        ),
        (
            "DELETE FROM pg_type WHERE typname = 'int8'",
            "the same, and the one that did the damage.",
            "pg19_pg_catalog.txt:110",
        ),
    ],
};

#[test]
fn every_catalog_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_pg_catalog.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 44,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The four statements this unit is for, from `ActiveRecord`'s own boot, answered.
///
/// They are in `tests/corpus/activerecord_8_1_statements.txt` and `tests/activerecord_surface.rs`
/// counts them; this asserts what they *say*, which a count cannot. Numbering is that file's.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one assertion per query ActiveRecord sends"
)]
fn activerecord_s_four_type_map_queries_answer() {
    let mut node = parity::Node::new(&[]);

    // 4 — the first query `AbstractAdapter` ever sends. **All ten** of its names are types this
    // node has, as of the `oid` unit; it was six at the end of tier 1. The count has risen with each of
    // tier 1's types and the rows were never edited to match: `CatalogView::rows` is derived from
    // `ColumnType::ALL`, so the catalog grows on its own and this assertion is what notices.
    assert_eq!(
        node.rows(
            "SELECT t.oid, t.typname FROM pg_type as t WHERE t.typname IN ('int2', 'int4', \
             'int8', 'oid', 'float4', 'float8', 'numeric', 'bool', 'timestamp', 'timestamptz')"
        ),
        vec![
            vec!["16", "bool"],
            vec!["20", "int8"],
            // `smallint` arrived with tier 1's fourth type, and is in ActiveRecord's list of ten.
            vec!["21", "int2"],
            vec!["23", "int4"],
            // **The tenth and last of `ActiveRecord`'s ten names.** The comment above has said
            // "six of its ten" since tier 1; every one of them is answered now.
            vec!["26", "oid"],
            // `float4` is `real` in a `CREATE TABLE`, and is what `ActiveRecord` maps to `Float`.
            vec!["700", "float4"],
            vec!["701", "float8"],
            // `timestamp` is in ActiveRecord's list of ten and arrived with tier 1's third type.
            vec!["1114", "timestamp"],
            vec!["1184", "timestamptz"],
            // `numeric` is the tenth name ActiveRecord asks for and the last of the ten this node
            // did not have; the list it sends is now fully answered.
            vec!["1700", "numeric"],
        ]
    );

    // 7 — the `LEFT JOIN pg_range` one, with `ON oid = rngtypid` unqualified.
    //
    // **Four of these rows carry a `typelem` and this expectation used to say `0`.** It was
    // written when `typelem` was an array's element and nothing else; a real server sets it on the
    // fixed-length types that are internally an array of something, and answers exactly this
    // statement with `point` 701, `lseg` 600, `box` 600 and `line` 701 — while `path`, `polygon`
    // and `circle`, which are variable-length, stay `0`. Measured by running this query against
    // 19beta1 rather than by reading a rule, and it is the divergence
    // `array_delimiter.rs::every_base_type_has_an_array_or_is_listed` found its first guard
    // resting on. Every type this
    // node has is in its list of forty names, and none of them is a range, so every `rngsubtype`
    // is NULL.
    let seven = node.rows(
        "SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, r.rngsubtype, t.typtype, \
         t.typbasetype FROM pg_type as t LEFT JOIN pg_range as r ON oid = rngtypid WHERE \
         t.typname IN ('int2', 'int4', 'int8', 'oid', 'float4', 'float8', 'text', 'varchar', \
         'char', 'name', 'bpchar', 'bool', 'bit', 'varbit', 'date', 'money', 'bytea', 'point', \
         'hstore', 'json', 'jsonb', 'cidr', 'inet', 'uuid', 'xml', 'tsvector', 'macaddr', \
         'citext', 'ltree', 'line', 'lseg', 'box', 'path', 'polygon', 'circle', 'numeric', \
         'interval', 'time', 'timestamp', 'timestamptz')",
    );
    assert_eq!(
        seven,
        vec![
            vec!["16", "bool", "0", ",", "boolin", "\\N", "b", "0"],
            vec!["17", "bytea", "0", ",", "byteain", "\\N", "b", "0"],
            // **`name` answers this query now** (ADR 0084). It is one of the forty names
            // `ActiveRecord` asks for, and until the type existed the row simply was not there —
            // the adapter saw a type map with a hole where its own catalog columns are declared.
            vec!["19", "name", "0", ",", "namein", "\\N", "b", "0"],
            vec!["20", "int8", "0", ",", "int8in", "\\N", "b", "0"],
            vec!["21", "int2", "0", ",", "int2in", "\\N", "b", "0"],
            vec!["23", "int4", "0", ",", "int4in", "\\N", "b", "0"],
            vec!["25", "text", "0", ",", "textin", "\\N", "b", "0"],
            vec!["26", "oid", "0", ",", "oidin", "\\N", "b", "0"],
            // Tier 2's first pair, and `ActiveRecord`'s list of forty names holds both.
            vec!["114", "json", "0", ",", "json_in", "\\N", "b", "0"],
            // **`xml` is one of the forty names too**, and had been answering nothing. A `b`ase
            // type, `typelem` 0 — an xml is not an array of anything — and `xml_in`, which is
            // the string the adapter compares to decide the column is *not* an array.
            vec!["142", "xml", "0", ",", "xml_in", "\\N", "b", "0"],
            // **`point` is one of the forty names this query asks for**, and it answers now: a
            // geometric type is a `b`ase type like any other, and its `typelem` is 0 because a
            // point is not an array of its coordinates.
            vec!["600", "point", "701", ",", "point_in", "\\N", "b", "0"],
            // **`cidr`, `macaddr` and `inet`** — three more names the adapter's list has always
            // held and got nothing back for. `cidr_in`, `macaddr_in`, `inet_in`.
            // **The six geometric shapes**, every one of them already in the adapter's own list
            // and every one of them answering nothing until now. `poly_in`, not `polygon_in`.
            vec!["601", "lseg", "600", ",", "lseg_in", "\\N", "b", "0"],
            vec!["602", "path", "0", ",", "path_in", "\\N", "b", "0"],
            // **`;`, alone among the seventy-eight** — a box's own text holds commas, so an array
            // of them needs another separator. Found by r1's run-75 provenance probe against the
            // oracle, and it is `pg_type.typdelim` a client reads to parse one.
            vec!["603", "box", "600", ";", "box_in", "\\N", "b", "0"],
            vec!["604", "polygon", "0", ",", "poly_in", "\\N", "b", "0"],
            vec!["628", "line", "701", ",", "line_in", "\\N", "b", "0"],
            vec!["650", "cidr", "0", ",", "cidr_in", "\\N", "b", "0"],
            vec!["700", "float4", "0", ",", "float4in", "\\N", "b", "0"],
            vec!["701", "float8", "0", ",", "float8in", "\\N", "b", "0"],
            // **`money` is in the adapter's own list** and was one of the names it asked about
            // and got nothing back for. `cash_in`, not `money_in`: the input function is named
            // for the C type behind it.
            vec!["718", "circle", "0", ",", "circle_in", "\\N", "b", "0"],
            vec!["790", "money", "0", ",", "cash_in", "\\N", "b", "0"],
            vec!["829", "macaddr", "0", ",", "macaddr_in", "\\N", "b", "0"],
            vec!["869", "inet", "0", ",", "inet_in", "\\N", "b", "0"],
            // `bpchar` is `character(n)`'s internal name and is in this query's list of forty.
            vec!["1042", "bpchar", "0", ",", "bpcharin", "\\N", "b", "0"],
            vec!["1043", "varchar", "0", ",", "varcharin", "\\N", "b", "0"],
            // `date` is in `ActiveRecord`'s list of forty and arrived with tier 2's first
            // type — added to `ColumnType::ALL` and nowhere else, which is what "the catalog
            // derives itself" means: no row was written here by hand.
            vec!["1082", "date", "0", ",", "date_in", "\\N", "b", "0"],
            // `time` is in this list of forty too, and its `typinput` is `time_in` — the
            // capture's own spelling, underscore and all.
            vec!["1083", "time", "0", ",", "time_in", "\\N", "b", "0"],
            vec![
                "1114",
                "timestamp",
                "0",
                ",",
                "timestamp_in",
                "\\N",
                "b",
                "0"
            ],
            vec![
                "1184",
                "timestamptz",
                "0",
                ",",
                "timestamptz_in",
                "\\N",
                "b",
                "0"
            ],
            // `numeric` is in this list of forty too, and its `typinput` is PostgreSQL's own
            // `numeric_in` — derived from `ColumnType::ALL` like every row above it.
            vec!["1186", "interval", "0", ",", "interval_in", "\\N", "b", "0"],
            // **`bit` and `varbit`**, two more names the adapter's list has always held: the
            // internal name of `bit varying` is `varbit`, as `bigint`'s is `int8`.
            vec!["1560", "bit", "0", ",", "bit_in", "\\N", "b", "0"],
            vec!["1562", "varbit", "0", ",", "varbit_in", "\\N", "b", "0"],
            vec!["1700", "numeric", "0", ",", "numeric_in", "\\N", "b", "0"],
            // `uuid` is the tenth type in ADR 0033's tier 2 and the ninth of the twenty
            // refusals in `postgresql_specific_schema.rb`.
            vec!["2950", "uuid", "0", ",", "uuid_in", "\\N", "b", "0"],
            // **`tsvector` is one of ActiveRecord's forty names**, and this node now has it, so
            // the row appears where before the type was simply missing from the surface.
            vec!["3614", "tsvector", "0", ",", "tsvectorin", "\\N", "b", "0"],
            vec!["3802", "jsonb", "0", ",", "jsonb_in", "\\N", "b", "0"],
            // **Last, because its oid is in the user range.** An extension's types are allocated
            // when it is installed, so a real server's hstore is above 16384 too and this query
            // orders by oid — the adapter reads the name and the input function, both of which
            // are what a real server's are.
            vec!["16400", "hstore", "0", ",", "hstore_in", "\\N", "b", "0"],
            vec!["16402", "citext", "0", ",", "citextin", "\\N", "b", "0"],
            // **`ltree` is one of the forty names too**, and had been answering nothing. An
            // extension's oid is above 16384 and differs per database on a real server, which is
            // why the adapter reads this row by `typname` and why the number here is ours.
            vec!["16404", "ltree", "0", ",", "ltree_in", "\\N", "b", "0"],
        ]
    );

    // 8 — ranges, enums and domains. **Six ranges now, not three**: `range_test.rb` declares two
    // range *arrays* and an array type is built per element type, so the other five had to come
    // too. The partition
    // `TypeMapInitializer#run` registers is no longer empty: three `typtype = 'r'` rows, which is
    // what makes `ActiveRecord` decode a `tsrange` column instead of handing back a string. The
    // query's `LEFT JOIN pg_range` is why it is asked without one here — `pg_range` is not a
    // relation this node has, and `tests/tsrange.rs` declares that; the adapter reads `typname`
    // and `typtype`, and `rngsubtype` only to name the subtype it already got from `format_type`.
    assert_eq!(
        node.rows(
            "SELECT t.oid, t.typname, t.typtype FROM pg_type as t WHERE t.typtype IN \
             ('r', 'e', 'd') ORDER BY t.oid"
        ),
        vec![
            vec!["3904", "int4range", "r"],
            vec!["3906", "numrange", "r"],
            vec!["3908", "tsrange", "r"],
            vec!["3910", "tstzrange", "r"],
            vec!["3912", "daterange", "r"],
            vec!["3926", "int8range", "r"],
        ]
    );

    // 9 — array types, found by their element type. **Thirty-five rows**, which is every array
    // type whose element is in the adapter's list: `typelem` is the element's oid, which is how
    // `ActiveRecord` finds them, and `typinput` is `array_in`, which is how it decides a column
    // is an array at all. It answered nothing while this node had no arrays and five rows while
    // it had five; sixteen more arrived with the unit that made every `typarray` name a row that
    // exists, which is what `TypeError: can't quote Array` was. `_hstore` and `_citext` are not
    // here and are not missing: an extension's element oid is above 16384 and is not in the
    // adapter's fixed list at all. `_tsvector` is the thirty-fifth and arrived with the
    // `tsvector` type: **3614 was already in the adapter's list**, waiting for a row.
    assert_eq!(
        node.rows(
            "SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, r.rngsubtype, \
             t.typtype, t.typbasetype FROM pg_type as t LEFT JOIN pg_range as r ON oid = \
             rngtypid WHERE t.typelem IN (16, 17, 18, 19, 20, 21, 23, 25, 26, 114, 142, 600, \
             601, 602, 603, 604, 628, 700, 701, 718, 790, 829, 869, 650, 1042, 1043, 1082, 1083, \
             1114, 1184, 1186, 1560, 1562, 1700, 2950, 3614, 3802, 13356, 13359, 13361, 13367, \
             13369, 3904, 3906, 3908, 3910, 3912, 3926) ORDER BY t.oid"
        ),
        vec![
            // **The base types whose `typelem` is not zero**, which is what puts them in an *array*
            // query at all: `point` and `line` are two `float8`s, `lseg` and `box` two `point`s,
            // `int2vector` `int2`s and `oidvector` `oid`s. A real server answers this same statement
            // with exactly these rows — measured — and `ActiveRecord` has registered every one of
            // them by *name* long before it looks at `typelem`, which is why the value can be right
            // here without changing what the adapter does with it.
            vec![
                "22".to_owned(),
                "int2vector".to_owned(),
                "21".to_owned(),
                ",".to_owned(),
                "int2vectorin".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "30".to_owned(),
                "oidvector".to_owned(),
                "26".to_owned(),
                ",".to_owned(),
                "oidvectorin".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            // **`_xml`, and it sorts first** — 143, below `_json`'s 199, which is the shape
            // `xml`'s pair has and `json`'s does not: 142 and 143 are adjacent.
            vec![
                "143".to_owned(),
                "_xml".to_owned(),
                "142".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "199".to_owned(),
                "_json".to_owned(),
                "114".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "600".to_owned(),
                "point".to_owned(),
                "701".to_owned(),
                ",".to_owned(),
                "point_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "601".to_owned(),
                "lseg".to_owned(),
                "600".to_owned(),
                ",".to_owned(),
                "lseg_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "603".to_owned(),
                "box".to_owned(),
                "600".to_owned(),
                ";".to_owned(),
                "box_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "628".to_owned(),
                "line".to_owned(),
                "701".to_owned(),
                ",".to_owned(),
                "line_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "651".to_owned(),
                "_cidr".to_owned(),
                "650".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            // `_money`, which a real server makes with the type — the adapter's list has had
            // `790` in it all along and got nothing back for it until now.
            vec![
                "791".to_owned(),
                "_money".to_owned(),
                "790".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1000".to_owned(),
                "_bool".to_owned(),
                "16".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1001".to_owned(),
                "_bytea".to_owned(),
                "17".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1005".to_owned(),
                "_int2".to_owned(),
                "21".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1007".to_owned(),
                "_int4".to_owned(),
                "23".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1009".to_owned(),
                "_text".to_owned(),
                "25".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1014".to_owned(),
                "_bpchar".to_owned(),
                "1042".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1015".to_owned(),
                "_varchar".to_owned(),
                "1043".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1016".to_owned(),
                "_int8".to_owned(),
                "20".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            // `_point` joins them: its element `point` is in the adapter's list, so a
            // `t.point :array_of_points, array: true` column decodes instead of coming back as a
            // string. Ordered by oid, which puts it just before `_float4`.
            vec![
                "1017".to_owned(),
                "_point".to_owned(),
                "600".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            // **`_box`, and the semicolon is what `type_lookup_test` reads.** A box's own text
            // holds commas — `{(1,1),(0,0);(3,3),(2,2)}` is two boxes — so it is the only array
            // type in the catalog whose delimiter is not one (ADR 0084's sibling unit).
            // PostgreSQL answers five more rows here that this node names as gaps: `_lseg`,
            // `_path`, `_polygon`, `_line`, `_circle` and `_name`
            // (`array_delimiter.rs::every_base_type_has_an_array_or_is_listed`).
            vec![
                "1020".to_owned(),
                "_box".to_owned(),
                "603".to_owned(),
                ";".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1021".to_owned(),
                "_float4".to_owned(),
                "700".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1022".to_owned(),
                "_float8".to_owned(),
                "701".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1028".to_owned(),
                "_oid".to_owned(),
                "26".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1040".to_owned(),
                "_macaddr".to_owned(),
                "829".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1041".to_owned(),
                "_inet".to_owned(),
                "869".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1115".to_owned(),
                "_timestamp".to_owned(),
                "1114".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1182".to_owned(),
                "_date".to_owned(),
                "1082".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1183".to_owned(),
                "_time".to_owned(),
                "1083".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1185".to_owned(),
                "_timestamptz".to_owned(),
                "1184".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1187".to_owned(),
                "_interval".to_owned(),
                "1186".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1231".to_owned(),
                "_numeric".to_owned(),
                "1700".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1561".to_owned(),
                "_bit".to_owned(),
                "1560".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "1563".to_owned(),
                "_varbit".to_owned(),
                "1562".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "2951".to_owned(),
                "_uuid".to_owned(),
                "2950".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "3643".to_owned(),
                "_tsvector".to_owned(),
                "3614".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "3807".to_owned(),
                "_jsonb".to_owned(),
                "3802".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            // **Every range array joins them**, because the adapter's list holds all six range
            // oids: an array is found by its element's oid, and these are the rows that make
            // `t.tsrange :ranges, array: true` decode instead of coming back as strings.
            vec![
                "3905".to_owned(),
                "_int4range".to_owned(),
                "3904".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "3907".to_owned(),
                "_numrange".to_owned(),
                "3906".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "3909".to_owned(),
                "_tsrange".to_owned(),
                "3908".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "3911".to_owned(),
                "_tstzrange".to_owned(),
                "3910".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "3913".to_owned(),
                "_daterange".to_owned(),
                "3912".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
            vec![
                "3927".to_owned(),
                "_int8range".to_owned(),
                "3926".to_owned(),
                ",".to_owned(),
                "array_in".to_owned(),
                "\\N".to_owned(),
                "b".to_owned(),
                "0".to_owned(),
            ],
        ]
    );
}

/// A catalog relation is read-only, through every verb that could write one.
///
/// The corpus carries the three DML verbs; these are the DDL ones plus the two answers that are
/// **not** `42501`, which are the interesting half — a blanket refusal on the name would have got
/// both of them wrong.
#[test]
fn a_catalog_relation_is_read_only() {
    let mut node = parity::Node::new(&[]);

    for statement in [
        "DROP TABLE pg_type",
        // `IF EXISTS` does not excuse it. Measured: a real server answers `42501` for this too.
        "DROP TABLE IF EXISTS pg_type",
        "ALTER TABLE pg_type ADD COLUMN a bigint",
        "CREATE INDEX ix ON pg_type (oid)",
        "DROP TABLE pg_range",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "{statement}"
        );
        assert!(
            error.to_string().ends_with("is a system catalog"),
            "{statement}"
        );
    }

    // And the two that are a different question. `DROP INDEX` is asking about a *kind*, not about
    // a write, and a real server answers `42809 "pg_type" is not an index` — so the catalog
    // relation has to be visible to that check rather than refused before it.
    let error = node.run("DROP INDEX pg_type").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::WRONG_OBJECT_TYPE);

    // `CREATE TABLE pg_type` is `42P07`, which is what a real server answers for
    // `CREATE TABLE pg_catalog.pg_type`. The *unqualified* spelling succeeds there, because it
    // makes a `public.pg_type` that `pg_catalog.pg_type` still resolves ahead of; this node has
    // no schemas, so there is one name and it is taken. Not in the corpus: replaying it against a
    // real server leaves a table behind and the file would stop being idempotent.
    let error = node.run("CREATE TABLE pg_type (a bigint)").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::DUPLICATE_TABLE);
}

/// `SELECT *` expands to the view's columns, and hides none of them.
///
/// **This is not in the corpus, and it is the bug the corpus could not have caught.** A real
/// server's `pg_type` has some thirty columns and 669 rows, so `SELECT *` is not a line two
/// servers can agree on — and the first version of this unit answered it one column short. A
/// `TableDef` with no primary key means a *keyless table*, whose column 0 is an internal row id
/// that `SELECT *` hides; a computed relation has no key **and** no row id, because nothing stores
/// its rows. `pg_type.oid` is column 0, so the framework's own first question came back without
/// the answer in it.
#[test]
fn a_star_expands_to_every_column_of_the_view() {
    let mut node = parity::Node::new(&[]);

    // The `0` before `11` is `typcollation`, which phase 13 added **last** for exactly this
    // reason: `SELECT *` expands in the declared order, so a column added anywhere else moves
    // every one after it and every client reading by position reads the wrong value. `11` is
    // `typnamespace`, added the same way for boot statement 26 — this node has one namespace and
    // every type reports it, as every relation's `relnamespace` does. The `8` and `N` are `typlen`
    // and `typcategory`, appended last again for the uuid unit. `1016` and `0` are `typarray` and
    // `typrelid`, appended last for the **fourth** time by `CREATE TYPE`: `_int8` really is 1016
    // on a real server, and nothing but a composite owns a `pg_class` row. The `f` and the NULL
    // are `typnotnull` and `typdefault`, appended last for the **fifth** time by `CREATE DOMAIN`
    // (ADR 0065): only a domain can constrain its own values, so every other type answers exactly
    // this pair.
    assert_eq!(
        node.rows("SELECT * FROM pg_type WHERE typname = 'int8'"),
        vec![vec![
            "20", "int8", "0", ",", "int8in", "b", "0", "0", "11", "8", "N", "1016", "0", "f",
            "\\N"
        ]]
    );
    assert_eq!(
        node.rows("SELECT t.* FROM pg_type AS t WHERE t.oid = 20"),
        vec![vec![
            "20", "int8", "0", ",", "int8in", "b", "0", "0", "11", "8", "N", "1016", "0", "f",
            "\\N"
        ]]
    );
    assert_eq!(
        node.rows("SELECT * FROM pg_type").len(),
        esker_sql::value::ColumnType::ALL.len(),
        "one row per type this server has, and the catalog cannot fall behind the enum"
    );

    // **`pg_range` was empty until this node had range types**, and this assertion said so. It
    // has six now — one per built-in range — and each carries the subtype's **own** oid, which is
    // not the one this node reads its bounds with: `int4range` reports `integer` (23) where every
    // integer here is an `i64`. `ActiveRecord`'s boot query is
    // `pg_type LEFT JOIN pg_range ON oid = rngtypid` and a NULL `rngsubtype` is a range type it
    // does not register, so these rows are what make a user-defined range readable at all.
    match node.answer("SELECT * FROM pg_range") {
        parity::Answer::Rows { types, rows } => {
            assert_eq!(types.len(), 2, "rngtypid and rngsubtype, and no `oid`");
            assert_eq!(
                rows,
                vec![
                    vec!["3904".to_owned(), "23".to_owned()],
                    vec!["3906".to_owned(), "1700".to_owned()],
                    vec!["3908".to_owned(), "1114".to_owned()],
                    vec!["3910".to_owned(), "1184".to_owned()],
                    vec!["3912".to_owned(), "1082".to_owned()],
                    vec!["3926".to_owned(), "20".to_owned()],
                ],
                "int4range, numrange, tsrange, tstzrange, daterange, int8range, in oid order"
            );
        }
        other => panic!("SELECT * FROM pg_range answered {other}"),
    }
}

/// `EXPLAIN` over a computed relation says what it is: one access path, and no cost.
#[test]
fn explain_names_the_catalog_scan() {
    let mut node = parity::Node::new(&[]);
    let plan: Vec<String> = node
        .rows("EXPLAIN SELECT typname FROM pg_type WHERE oid = 20")
        .into_iter()
        .map(|row| row.join(""))
        .collect();
    assert_eq!(
        plan,
        vec![
            "Project (1 columns)".to_owned(),
            "  Filter".to_owned(),
            "    Condition: (oid = 20)".to_owned(),
            "    Catalog Scan on pg_type".to_owned(),
        ]
    );
}
