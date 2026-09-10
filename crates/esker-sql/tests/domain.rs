//! `CREATE DOMAIN` / `DROP DOMAIN` — run 70's row, 8 tests over two files.
//!
//! **A domain is a name and a constraint over a base type**
//! ([ADR 0065](../../../docs/adr/0065-a-domain-is-a-name-and-a-constraint-over-a-base-type.md)), so
//! it is a fourth `TypeKind` beside the range, the composite and the enum, and everything
//! downstream — the record, `pg_type`, the drop, the dependency edge — is the path all four take.
//!
//! `domain_test.rb` asks the question that decides the shape: a `custom_money` column over
//! `numeric(8,2)` must report `column.type` **`:decimal`** and `column.sql_type`
//! **`"custom_money"`** at once. The value is the base type's and the name is the domain's, and a
//! node that answered one of those for both would pass half the file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own domain and table.
const CORPUS_FIXTURE: &[&str] = &[];

const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The declared types this entry named all agree now** — `name` (ADR 0084) and `"char"`
    // (ADR 0095) — and it stays because the same statement is an `answers` divergence too: the
    // harness reads a `types` entry only once the *rows* agree, so this one is not read at all
    // and will be deleted with the answer it shadows.
    types: &[
        "SELECT 'r', n.nspname, t.typname, t.typtype FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname = 'text' ORDER BY n.nspname",
        "SELECT 'r', format_type(a.atttypid, a.atttypmod), t.typtype FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid WHERE a.attrelid = 'dm_shadow'::regclass AND a.attname = 'c'",
    ],
    answers: &[
        (
            "SELECT 'r', n.nspname, t.typname, t.typtype FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname = 'text' ORDER BY n.nspname",
            "**The domain's row agrees; the built-in's schema does not.** `dm_s|text|d` is \
             identical, which is the half this unit is about — a domain is reported in the schema \
             it was created in. The other row is `pg_catalog|text|b` there and `public|text|b` \
             here, which is the standing difference in the **schema model** rather than anything \
             about domains: this node has one schema for its built-in types and a real server puts \
             them in `pg_catalog`. `pg_type`'s `typnamespace` column already carries that note.",
            "pg19_domain.txt:38",
        ),
        (
            "SELECT 'r', format_type(a.atttypid, a.atttypmod), t.typtype FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid WHERE a.attrelid = 'dm_shadow'::regclass AND a.attname = 'c'",
            "**A domain does not shadow a built-in type's name.** With `search_path = dm_s, \
             pg_catalog` and a domain `dm_s.text`, a column declared `text` is the *domain* on a \
             real server (`typtype` `d`) and the built-in here (`b`). The name is resolved as a \
             type before the catalog is consulted at all — `crate::exec::ddl::resolve_user_type` \
             is reached only for a name lowering could not read — so shadowing needs type \
             resolution to walk the `search_path` ahead of the built-in vocabulary, which is a \
             unit of its own and not one `schema_test.rb` needs: measured, that file raises this \
             shape **zero** times now and `format_type` agrees on the spelling either way.",
            "pg19_domain.txt:41",
        ),
    ],
};

#[test]
fn every_domain_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_domain.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The query `ActiveRecord` loads its type map with, verbatim, returning the five
/// `information_schema` domains** — `debts-v1.1.md` #37, family two.
///
/// It runs on **every connection** — 712 occurrences across 164 captured files — and it is the
/// constraint the rest of this row has to meet: **an oid this node sends that does not come back
/// from here has no decoder, and a correct value arrives as a string.** So these rows come first
/// and the wire follows them, never the other way round
/// ([ADR 0103](../../../docs/adr/0103-a-domain-is-a-type-a-client-can-be-sent.md)).
///
/// Measured on 19beta1, 2026-09-10 (`tests/captures/pg19_domain_type.txt`). Three of these are
/// not guessable:
///
/// * **`typinput` is `domain_in`** for all five, not the base's input function;
/// * **the array oid is the domain's minus one**, because `initdb` allocates the array first —
///   where this node's own convention for a user type is `oid + 1`;
/// * **`time_stamp` carries a default**, `CURRENT_TIMESTAMP(2)`, and the other four do not.
#[test]
fn the_type_map_query_returns_the_information_schema_domains() {
    let mut node = parity::Node::new(&[]);
    // **The query verbatim, and then the domains taken out of what it returned.** It asks for
    // three kinds and this node answers six ranges beside the five domains — which is the point of
    // the one query and not a complication: `ActiveRecord` learns every range, enum and domain the
    // server has in a single load. The ranges belong to their own row and are not pinned here.
    let answered = node.rows(
        "SELECT t.oid, t.typname, t.typelem, t.typdelim, t.typinput, r.rngsubtype, \
         t.typtype, t.typbasetype \
         FROM pg_type as t LEFT JOIN pg_range as r ON oid = rngtypid \
         WHERE t.typtype IN ('r', 'e', 'd') ORDER BY t.typname",
    );
    let domains: Vec<Vec<String>> = answered
        .into_iter()
        .filter(|row| row.get(6).is_some_and(|typtype| typtype == "d"))
        .collect();
    assert_eq!(
        domains,
        vec![
            row("13356", "cardinal_number", "23"),
            row("13359", "character_data", "1043"),
            row("13361", "sql_identifier", "19"),
            row("13367", "time_stamp", "1184"),
            row("13369", "yes_or_no", "1043"),
        ]
    );
}

/// One expected row of the type-map query: the four columns that vary, and the four that do not.
fn row(oid: &str, name: &str, base: &str) -> Vec<String> {
    vec![
        oid.to_owned(),
        name.to_owned(),
        // `typelem` 0 and `rngsubtype` NULL: a domain over a scalar is not an array and not a
        // range, however its base prints. A NULL comes back from the harness as `\N`, which is the
        // corpus format's own spelling and not an empty string.
        "0".to_owned(),
        ",".to_owned(),
        "domain_in".to_owned(),
        "\\N".to_owned(),
        "d".to_owned(),
        base.to_owned(),
    ]
}

/// **A domain's array is reachable, and is not itself a domain** — which is why the query above
/// does not return it and does not need to.
///
/// Measured: `_sql_identifier` is `typtype = 'b'` with `typelem` pointing at the domain, so
/// `ActiveRecord` finds it through the domain's own row. The array oid is the domain's **minus
/// one**.
#[test]
fn a_domains_array_is_a_base_type_that_points_back_at_it() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT typname, typtype, typelem, typinput FROM pg_type \
             WHERE typname IN ('_sql_identifier', '_time_stamp') ORDER BY typname"
        ),
        vec![
            vec![
                "_sql_identifier".to_owned(),
                "b".to_owned(),
                "13361".to_owned(),
                "array_in".to_owned(),
            ],
            vec![
                "_time_stamp".to_owned(),
                "b".to_owned(),
                "13367".to_owned(),
                "array_in".to_owned(),
            ],
        ]
    );
}

/// **A domain column declares its BASE type on the wire, with the domain's typmod** — and this
/// test says the opposite of what it said when `37745bc7` wrote it, because that was wrong.
///
/// Measured on 19beta1 through `PG::Result#ftype`, which reads the `RowDescription` bytes:
///
/// ```text
/// SELECT table_name, is_nullable, ordinal_position FROM information_schema.columns
///   table_name        ftype 19   (name)      fmod -1
///   is_nullable       ftype 1043 (varchar)   fmod 7     <- the DOMAIN's typmod, on the BASE type
///   ordinal_position  ftype 23   (int4)      fmod -1
/// SELECT d FROM wt   -- d is a user domain over integer
///   d                 ftype 23   (int4)      fmod -1
/// ```
///
/// **The mistake was reading a server-side view as the wire.** ADR 0103's third family was built
/// on `pg_prepared_statements.result_types`, which for the first query says
/// `{information_schema.sql_identifier}` — and that is the **plan's** type. PostgreSQL's
/// `printtup.c` replaces it on the way out: *"If column is a domain, send the base type and typmod
/// instead"*. Two answers, one of them the bytes, and only the bytes are the wire.
///
/// **`is_nullable`'s `fmod 7` is the half a base-type-only fix would still get wrong**, and it is
/// why the domain's typmod moved off the column list in the first place: `atttypmod` is -1 on
/// every `information_schema` column and the width lives on `pg_type.typtypmod`. The wire wants it
/// back — on the base type.
///
/// **The `pg_type` rows stay.** `ActiveRecord` loads its type map with `typtype IN ('r','e','d')`
/// and a real server has those rows too; what it must never see is an oid on the wire that its
/// decoder was not registered for. Sending the base is what makes both true.
///
/// **`Describe` is asked as well as the simple path**, for the reason `enum_extended_protocol.rs`
/// exists.
/// One column as the wire declares it: `(oid, type_size, type_modifier)`.
type Declared = (u32, i16, i32);

#[test]
fn a_domain_column_declares_its_base_type_on_the_wire() {
    use esker_sql::parse::parse_statements;
    use esker_sql::pgwire::session::Execute;

    let mut node = parity::Node::new(&[
        "CREATE TABLE t (a int)",
        "CREATE DOMAIN wt_dom AS integer",
        "CREATE TABLE wt (d wt_dom, t text)",
        "INSERT INTO wt VALUES (1, 'x')",
    ]);
    // Per statement, what 19beta1 puts on the wire for each of its columns.
    let cases: &[(&str, &[Declared])] = &[
        (
            "SELECT table_name, is_nullable, ordinal_position FROM information_schema.columns \
             WHERE table_name = 't'",
            &[(19, 64, -1), (1043, -1, 7), (23, 4, -1)],
        ),
        ("SELECT d, t FROM wt", &[(23, 4, -1), (25, -1, -1)]),
    ];
    for (statement, expected) in cases {
        let esker_sql::pgwire::session::Outcome::Rows { fields, .. } =
            node.run(statement).expect("it answers")
        else {
            panic!("no rows: {statement}");
        };
        let declared: Vec<Declared> = fields
            .iter()
            .map(|field| (field.type_oid, field.type_size, field.type_modifier))
            .collect();
        assert_eq!(&declared, expected, "simple protocol: {statement}");

        let parsed = parse_statements(statement).expect("it parses");
        let described = node
            .executor
            .describe(&parsed[0], &[])
            .expect("it describes");
        let fields = described.fields.expect("a SELECT returns rows");
        assert_eq!(
            &fields
                .iter()
                .map(|field| (field.type_oid, field.type_size, field.type_modifier))
                .collect::<Vec<Declared>>(),
            expected,
            "extended protocol: {statement}"
        );
    }
    // **And the values are untouched**, which is what makes this a declared type and not an
    // answer: a domain adds a constraint, not a representation.
    assert_eq!(node.rows("SELECT d, t FROM wt"), vec![vec!["1", "x"]]);
}

/// **Every column of every `information_schema` view is a domain**, which is the ratchet under
/// the claim that a column's *name* decides which one.
///
/// Measured on 19beta1: all 122 columns of the seven views this node has are one of four domains,
/// and no column name maps to two. So a view added — or a column added to one — without an entry
/// in `pg_catalog::INFORMATION_SCHEMA_DOMAINS` fails here rather than quietly declaring a base
/// type to a client, which is the failure this whole row is about and the one no corpus can see.
#[test]
fn every_information_schema_column_is_a_domain() {
    let mut node = parity::Node::new(&[]);
    let query = "SELECT c.relname, a.attname FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 WHERE n.nspname = 'information_schema'";
    assert_eq!(
        node.rows(&format!(
            "{query} AND t.typtype <> 'd' ORDER BY c.relname, a.attnum"
        )),
        Vec::<Vec<String>>::new(),
        "an information_schema column that is not a domain"
    );
    // And the denominator, so an empty answer above cannot come from an empty join.
    assert_eq!(
        node.rows(&format!("{query} ORDER BY c.relname, a.attnum"))
            .len(),
        55
    );
}

/// **`pg_typeof` names the domain for a bare column and loses it the moment it is wrapped**,
/// which is what shape A buys and what it deliberately does not.
///
/// Both halves are measured on 19beta1 and both are PostgreSQL's own behaviour: `table_name || ''`
/// is `text` *there too*, because `||` returns text. The aggregate is the one real divergence and
/// r1 accepted it in `results/run-107.md:43` — PostgreSQL says
/// `information_schema.sql_identifier[]` and this node says `name[]`; `ActiveRecord` decodes 1003
/// as an array either way. Carrying the identity through an aggregate is shape **B**, which is a
/// different row.
#[test]
fn pg_typeof_names_the_domain_until_the_column_is_wrapped() {
    let mut node = parity::Node::new(&["CREATE TABLE t (a int)"]);
    let asked = |node: &mut parity::Node, expr: &str, from: &str| {
        node.rows(&format!(
            "SELECT pg_typeof({expr}) FROM information_schema.{from} LIMIT 1"
        ))[0][0]
            .clone()
    };
    assert_eq!(
        asked(&mut node, "table_name", "tables"),
        "information_schema.sql_identifier"
    );
    assert_eq!(
        asked(&mut node, "table_type", "tables"),
        "information_schema.character_data"
    );
    assert_eq!(
        asked(&mut node, "is_nullable", "columns"),
        "information_schema.yes_or_no"
    );
    assert_eq!(
        asked(&mut node, "ordinal_position", "columns"),
        "information_schema.cardinal_number"
    );
    // The wrap: `text` here and `text` there.
    assert_eq!(asked(&mut node, "table_name || ''", "tables"), "text");
    // The aggregate: the accepted divergence, pinned so that closing it is a decision rather than
    // a surprise.
    assert_eq!(
        node.rows("SELECT pg_typeof(array_agg(table_name)) FROM information_schema.tables"),
        vec![vec!["name[]"]]
    );
}

/// **`format_type` prints what `pg_attribute` now reports**, which it could not before.
///
/// The five domains have no record in the tenant's key space, so the catalog's user-type lookup
/// cannot find them and `format_type(13361, -1)` was `???` — for the very oids this node had just
/// put in `atttypid`. Measured against 19beta1's own `format_type` over the same three columns.
#[test]
fn format_type_prints_the_domain_pg_attribute_reports() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'information_schema' AND c.relname = 'tables' ORDER BY a.attnum"
        ),
        vec![
            vec!["information_schema.sql_identifier"],
            vec!["information_schema.sql_identifier"],
            vec!["information_schema.character_data"],
        ]
    );
}

/// **A domain in a schema prints with a dot and not with a NUL**, found on the way past.
///
/// A type in a schema is stored `schema ++ NUL ++ name` (`catalog::SCHEMA_SEPARATOR`) and
/// `pg_typeof` was handing that byte straight to a client — `s\0dom_probe` where a real server
/// says `s.dom_probe`. `format_type` had it right one screen away, which is how it was found:
/// two readers of one stored name, and only one of them had been asked.
#[test]
fn pg_typeof_of_a_domain_in_a_schema_writes_a_dot() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA s",
        "CREATE DOMAIN s.dom_probe AS integer",
        "CREATE TABLE dtab (d s.dom_probe)",
        "INSERT INTO dtab VALUES (1)",
    ]);
    assert_eq!(
        node.rows("SELECT pg_typeof(d) FROM dtab"),
        vec![vec!["s.dom_probe"]]
    );
    assert_eq!(
        node.rows(
            "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid WHERE c.relname = 'dtab' AND a.attname = 'd'"
        ),
        vec![vec!["s.dom_probe"]]
    );
}
