//! `CREATE [UNIQUE] INDEX … INCLUDE (…)` — statement 787 of `postgresql_specific_schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog trade: `pg_indexes.indexname` is a `name` on a real server and `text`
    // here, with the same characters in it. The rows agree, `INCLUDE (…)` in the definition
    // included.
    types: &[],
    answers: &[
        (
            "CREATE INDEX \"ci_desc\" ON \"companies\" (\"firm_id\") INCLUDE (\"name\" DESC)",
            "A **C1 parser gap**: `sqlparser` 0.62.0 types `INCLUDE` as a list of bare identifiers \
             (`CreateIndex::include: Vec<Ident>`), so an ordering inside it is a *syntax* error \
             before the lowering is reached. PostgreSQL refuses it too, with `42P17` and its own \
             sentence; both refuse, and what differs is which layer says so",
            "pg19_include_index.txt:74",
        ),
        (
            "CREATE INDEX \"ci_opclass\" ON \"companies\" (\"firm_id\") INCLUDE (\"name\" \
             varchar_pattern_ops)",
            "The other half of the same parser gap — an operator class has nowhere to go in a \
             `Vec<Ident>` either. `42P17` there, `42601` here",
            "pg19_include_index.txt:77",
        ),
        (
            "ALTER TABLE \"companies\" ADD CONSTRAINT \"companies_u_include\" UNIQUE \
             (\"account_id\") INCLUDE (\"name\")",
            "**The clause lives in two grammars and this node reads one of them.** \
             `sqlparser`\u{2019}s `UniqueConstraint` has no `include` field, so the constraint \
             spelling does not parse — and `ADD CONSTRAINT ... UNIQUE` is an action this node has \
             never had in any case. The index spelling, which is what statement 787 writes, runs",
            "pg19_include_index.txt:88",
        ),
        (
            "SELECT \'r\', conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = \
             \'companies_u_include\'",
            "A follow-on of the parser gap above and not a divergence of its own: the constraint \
             the previous statement would have added does not exist here, so this answers no rows \
             where a real server answers one. Not a `25P02` cascade — the block is not aborted, \
             the object is simply absent — so it is declared rather than counted",
            "pg19_include_index.txt:89",
        ),
        (
            "SELECT \'r\', pg_get_indexdef(\'companies_u_include\'::regclass)",
            "The same absence, one statement later, and this one is `42P01` because `::regclass` \
             resolves a name that was never created. Both close together the day `ADD CONSTRAINT \
             ... UNIQUE` and `sqlparser`\u{2019}s `UniqueConstraint::include` do",
            "pg19_include_index.txt:90",
        ),
    ],
};

#[test]
fn every_include_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_include_index.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The suite's own statement, and the two counts that separate the payload from the key.
///
/// **The included columns are in `indkey`, and only a count tells them apart.** `indnatts` is 4
/// and `indnkeyatts` is 2 over one four-attribute vector — an implementation that reconstructs an
/// index from `indkey` alone reports a four-column index, and `ActiveRecord`'s schema dumper reads
/// `indkey`.
#[test]
fn statement_787() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE \"companies\" (\"id\" bigserial primary key, \"firm_id\" integer, \"type\" \
         character varying, \"name\" character varying, \"account_id\" integer)",
    ]);
    node.run(
        "CREATE INDEX \"company_include_index\" ON \"companies\" (\"firm_id\", \"type\") INCLUDE \
         (\"name\", \"account_id\")",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('company_include_index'::regclass)"),
        [[
            "CREATE INDEX company_include_index ON public.companies USING btree (firm_id, type) \
             INCLUDE (name, account_id)"
        ]]
    );
    assert_eq!(
        node.rows(
            "SELECT indnatts, indnkeyatts, indisunique, indkey FROM pg_index WHERE indexrelid = \
             'company_include_index'::regclass"
        ),
        [["4", "2", "f", "2 3 4 5"]]
    );
}

/// **Uniqueness is over the key columns only**; the payload is stored and never compared.
#[test]
fn a_unique_index_compares_its_key_and_not_its_payload() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE companies (id bigserial primary key, firm_id integer, name character varying)",
        "CREATE UNIQUE INDEX company_unique_include ON companies (firm_id) INCLUDE (name)",
        "INSERT INTO companies (firm_id, name) VALUES (1, 'one')",
    ]);
    // A **different** payload does not rescue a duplicate key, and the DETAIL names only the key.
    let error = node
        .run("INSERT INTO companies (firm_id, name) VALUES (1, 'two')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        error.to_string(),
        "duplicate key value violates unique constraint \"company_unique_include\""
    );
    assert_eq!(
        error.detail().as_deref(),
        Some("Key (firm_id)=(1) already exists.")
    );
    // A different key with the **same** payload is admitted: the payload is not part of the key.
    node.run("INSERT INTO companies (firm_id, name) VALUES (2, 'one')")
        .unwrap();
}

/// **An included column may repeat a key column** — accepted, not deduplicated.
#[test]
fn an_included_column_may_repeat_a_key_column() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE companies (id bigserial primary key, firm_id integer, name character varying)",
        "CREATE INDEX ci_dup ON companies (firm_id) INCLUDE (firm_id)",
    ]);
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('ci_dup'::regclass)"),
        [["CREATE INDEX ci_dup ON public.companies USING btree (firm_id) INCLUDE (firm_id)"]]
    );
    // The same attnum twice in one vector, and the counts say which half it is in.
    assert_eq!(
        node.rows("SELECT indnatts, indnkeyatts, indkey FROM pg_index WHERE indexrelid = 'ci_dup'::regclass"),
        [["2", "1", "2 2"]]
    );
}

/// **Each mistake on an included column has its own SQLSTATE**, and none of them is a syntax error.
///
/// The two `42P17`s the capture records — `INCLUDE (name DESC)` and
/// `INCLUDE (name varchar_pattern_ops)` — are not here: `sqlparser` 0.62.0 types the clause as a
/// list of bare identifiers, so both are a *syntax* error before the lowering is reached. A C1
/// parser gap, declared in the corpus above.
#[test]
fn each_bad_included_column_has_its_own_sqlstate() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE companies (id bigserial primary key, firm_id integer, name character varying)",
    ]);
    for (statement, sqlstate, message) in [
        (
            // The **plain** column-not-found wording, not the `named in key` phrasing a key
            // column gets.
            "CREATE INDEX ci_nosuch ON companies (firm_id) INCLUDE (nosuchcol)",
            "42703",
            "column \"nosuchcol\" does not exist",
        ),
        (
            "CREATE INDEX ci_hash ON companies USING hash (firm_id) INCLUDE (name)",
            "0A000",
            "access method \"hash\" does not support included columns",
        ),
        (
            "CREATE INDEX ci_brin ON companies USING brin (firm_id) INCLUDE (name)",
            "0A000",
            "access method \"brin\" does not support included columns",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate, "{statement}");
        assert_eq!(error.to_string(), message, "{statement}");
    }
}

/// **The clause lives in two grammars and this node reads one of them.**
///
/// A `UNIQUE` *constraint* takes `INCLUDE` too and `pg_get_constraintdef` prints it back — but
/// `sqlparser` 0.62.0's `UniqueConstraint` has no field for it, so the constraint form is a syntax
/// error before anything of ours runs. A C1 parser gap on top of an action (`ADD CONSTRAINT …
/// UNIQUE`) this node has never had; both are declared in the corpus above. This pins the gap
/// rather than the feature, so the day either closes the test says so.
#[test]
fn the_constraint_spelling_of_include_does_not_parse() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE companies (id bigserial primary key, account_id integer, name character varying)",
    ]);
    let error = node
        .run(
            "ALTER TABLE companies ADD CONSTRAINT companies_u_include UNIQUE (account_id) INCLUDE \
             (name)",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42601");
}

/// `INCLUDE (…)` prints **after the key list and before the predicate**.
#[test]
fn include_prints_between_the_key_and_the_where() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE companies (id bigserial primary key, firm_id integer, name character \
         varying, account_id integer)",
        "CREATE INDEX ci_where ON companies (firm_id) INCLUDE (name) WHERE account_id IS NOT NULL",
    ]);
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('ci_where'::regclass)"),
        [[
            "CREATE INDEX ci_where ON public.companies USING btree (firm_id) INCLUDE (name) WHERE \
             (account_id IS NOT NULL)"
        ]]
    );
}
