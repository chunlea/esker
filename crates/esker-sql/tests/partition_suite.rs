//! What the Rails suite *does* with a partitioned table once the schema has loaded.
//!
//! `tests/partition.rs` pins the DDL of statements 781-786. This pins the statements the test
//! cases send afterwards — `drop_table`, `remove_index`, the schema dumper's catalog reads, a
//! primary key on a partitioned table, and `upsert_all` through one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog trade and nothing new: `relname` and `conname` are a `name` on a real
    // server, `relkind` and `contype` are `"char"`, and `indkey` is an `int2vector`. This node has
    // none of the four types, so each is `text` with the same characters in it. **Every row
    // agrees** — including the `ON ONLY`, the `int2vector`\u{2019}s `2 1`, and the partition\u{2019}s own
    // `pk_part_1_pkey`.
    types: &[
        "SELECT 'r', c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace WHERE \
         n.nspname = ANY (current_schemas(false)) AND c.relname = 'partitioned_events' AND c.relkind IN \
         ('r','v','m','p','f')",
        "SELECT 'r', relkind, relhassubclass FROM pg_class WHERE relname = 'partitioned_events'",
        "SELECT 'r', c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace WHERE \
         n.nspname = ANY (current_schemas(false)) AND c.relname = 'measurements' AND c.relkind IN \
         ('r','v','m','p','f')",
        "SELECT 'r', c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace WHERE \
         n.nspname = ANY (current_schemas(false)) AND c.relname = 'measurements_toronto' AND c.relkind IN \
         ('r','v','m','p','f')",
        "SELECT 'r', i.relname, d.indisunique, d.indkey, pg_get_indexdef(d.indexrelid), d.indisvalid FROM \
         pg_class t INNER JOIN pg_index d ON t.oid = d.indrelid INNER JOIN pg_class i ON d.indexrelid = \
         i.oid LEFT JOIN pg_namespace n ON n.oid = t.relnamespace WHERE i.relkind IN ('i', 'I') AND \
         d.indisprimary = 'f' AND t.relname = 'measurements' AND n.nspname = ANY (current_schemas(false)) \
         ORDER BY i.relname",
        "SELECT 'r', i.relname, d.indisunique, d.indkey, pg_get_indexdef(d.indexrelid), d.indisvalid FROM \
         pg_class t INNER JOIN pg_index d ON t.oid = d.indrelid INNER JOIN pg_class i ON d.indexrelid = \
         i.oid LEFT JOIN pg_namespace n ON n.oid = t.relnamespace WHERE i.relkind IN ('i', 'I') AND \
         d.indisprimary = 'f' AND t.relname = 'measurements_toronto' AND n.nspname = ANY \
         (current_schemas(false)) ORDER BY i.relname",
        "SELECT 'r', parent.relname FROM pg_catalog.pg_inherits i JOIN pg_catalog.pg_class child ON \
         i.inhrelid = child.oid JOIN pg_catalog.pg_class parent ON i.inhparent = parent.oid LEFT JOIN \
         pg_namespace n ON n.oid = child.relnamespace WHERE child.relname = 'measurements_toronto' AND \
         child.relkind IN ('r','p') AND n.nspname = ANY (current_schemas(false))",
        "SELECT 'r', parent.relname FROM pg_catalog.pg_inherits i JOIN pg_catalog.pg_class child ON \
         i.inhrelid = child.oid JOIN pg_catalog.pg_class parent ON i.inhparent = parent.oid LEFT JOIN \
         pg_namespace n ON n.oid = child.relnamespace WHERE child.relname = 'trains' AND child.relkind IN \
         ('r','p') AND n.nspname = ANY (current_schemas(false))",
        "SELECT 'r', relkind, relhassubclass FROM pg_class WHERE relname = 'pk_part'",
        "SELECT 'r', conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = \
         'pk_part'::regclass",
        "SELECT 'r', i.relname, x.indisprimary, x.indisunique FROM pg_index x JOIN pg_class i ON i.oid = \
         x.indexrelid WHERE x.indrelid = 'pk_part'::regclass",
        "SELECT 'r', i.relname, x.indisprimary FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid \
         WHERE x.indrelid = 'pk_part_1'::regclass",
        "SELECT 'r', conname, contype FROM pg_constraint WHERE conrelid = 'pk_part_1'::regclass",
    ],
    answers: &[(
        "SELECT \'r\', pg_typeof(relhassubclass), pg_typeof(relkind) FROM pg_class WHERE relname = \'measurements\'",
        "The standing catalog-type trade, and this is the one query that makes it a *row* \
             rather than a declared type: `pg_typeof` returns the type as a value, so `relkind` \
             being `\"char\"` there and `text` here shows up in the answer. `relhassubclass` agrees. \
             Every value in the column is identical; only the name of the type it is stored under \
             differs, which is the trade every `pg_catalog` column in this crate makes",
    )],
};

#[test]
fn every_partition_suite_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_partition_suite.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 60,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **`DROP TABLE` of a partitioned parent takes every partition with it, without `CASCADE`.**
///
/// This is the statement `create_table(:measurements, force: true)` sends on every schema reload,
/// so a node that refused it could not load the suite's schema twice. Measured: `0` relations
/// match `measurements%` afterwards — the partitions and their own indexes go too.
#[test]
fn dropping_a_partitioned_parent_takes_its_partitions() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE measurements (city_id character varying NOT NULL, logdate date NOT NULL) \
         PARTITION BY LIST (city_id)",
        "CREATE UNIQUE INDEX index_measurements_on_logdate_and_city_id ON measurements (logdate, \
         city_id)",
        "CREATE TABLE measurements_toronto PARTITION OF measurements FOR VALUES IN (1)",
        "CREATE TABLE measurements_concepcion PARTITION OF measurements FOR VALUES IN (2)",
        "INSERT INTO measurements (city_id, logdate) VALUES ('1', '2026-09-01')",
    ]);
    node.run("DROP TABLE IF EXISTS measurements").unwrap();
    // `CASCADE` is not what makes them go: the same statement without it does the same thing, and
    // with it does no more. Measured, both spellings, in one session.
    // The partitions, the parent, its index and the partitions' own indexes: all of it.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname LIKE 'measurements%'"),
        [["0"]]
    );
    // And the name is free again, which is what `force: true` is for.
    node.run(
        "CREATE TABLE measurements (city_id character varying NOT NULL) PARTITION BY LIST \
              (city_id)",
    )
    .unwrap();
}

/// **`INHERITS` is the opposite, and the two share an edge.**
///
/// A partitioned parent goes without `CASCADE`; an inheriting parent is `2BP01` naming the child.
/// One rule for both is wrong for one of them, which is what this pins.
#[test]
fn dropping_an_inheriting_parent_is_still_refused() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ip (a integer)",
        "CREATE TABLE ic () INHERITS (ip)",
    ]);
    let error = node.run("DROP TABLE ip").unwrap_err();
    assert_eq!(error.sqlstate(), "2BP01");
    assert_eq!(
        error.to_string(),
        "cannot drop table ip because other objects depend on it"
    );
    assert_eq!(
        error.detail().as_deref(),
        Some("table ic depends on table ip")
    );
}

/// **`DROP INDEX` on the partitioned index takes every partition's own index with it.**
///
/// `remove_index` is what `schema_test.rb` sends; the children it removes are ones the suite
/// never named.
#[test]
fn dropping_a_partitioned_index_takes_the_partitions_indexes() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE measurements (city_id character varying NOT NULL, logdate date NOT NULL) \
         PARTITION BY LIST (city_id)",
        "CREATE UNIQUE INDEX index_measurements_on_logdate_and_city_id ON measurements (logdate, \
         city_id)",
        "CREATE TABLE measurements_toronto PARTITION OF measurements FOR VALUES IN (1)",
        "CREATE TABLE measurements_concepcion PARTITION OF measurements FOR VALUES IN (2)",
    ]);
    // **Two**, and the pattern is why: the suite's own index is
    // `index_measurements_on_logdate_and_city_id` — `logdate_and_city_id` — so this matches only
    // the two the partitions were given, `<partition>_logdate_city_id_idx`. That is the point of
    // the capture's probe: it counts the children the suite never named.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname LIKE '%logdate_city_id%'"),
        [["2"]]
    );
    node.run("DROP INDEX index_measurements_on_logdate_and_city_id")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname LIKE '%logdate_city_id%'"),
        [["0"]]
    );
    // The tables are untouched, and a row that the unique index would have refused now goes in.
    node.run("INSERT INTO measurements (city_id, logdate) VALUES ('1', '2026-09-01')")
        .unwrap();
    node.run("INSERT INTO measurements (city_id, logdate) VALUES ('1', '2026-09-01')")
        .unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM measurements"), [["2"]]);
}

/// **A `PRIMARY KEY` on a partitioned table is the unique-index rule again**, and each partition
/// gets its own `<partition>_pkey` — which is what a duplicate names.
#[test]
fn a_primary_key_on_a_partitioned_table_covers_the_key_and_lands_on_each_partition() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE pk_part (a integer NOT NULL, b integer NOT NULL, PRIMARY KEY (a, b)) \
         PARTITION BY LIST (a)",
    ]);
    assert_eq!(
        node.rows("SELECT relkind FROM pg_class WHERE relname = 'pk_part'"),
        [["p"]]
    );
    node.run("CREATE TABLE pk_part_1 PARTITION OF pk_part FOR VALUES IN (1)")
        .unwrap();
    // The partition has a primary key of its own, which the suite never named.
    assert_eq!(
        node.rows(
            "SELECT i.relname, x.indisprimary FROM pg_index x JOIN pg_class i ON i.oid = \
             x.indexrelid WHERE x.indrelid = 'pk_part_1'::regclass"
        ),
        [["pk_part_1_pkey", "t"]]
    );
    node.run("INSERT INTO pk_part (a, b) VALUES (1, 1)")
        .unwrap();
    // **The child's key is what refuses the duplicate**, not the parent's.
    let error = node
        .run("INSERT INTO pk_part (a, b) VALUES (1, 1)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        error.to_string(),
        "duplicate key value violates unique constraint \"pk_part_1_pkey\""
    );
}

/// **A key that misses a partition column is `0A000`, with `PRIMARY KEY` in the sentence.**
///
/// The same two sentences `CREATE UNIQUE INDEX` gets, with the word substituted — measured, and
/// this is the *inline* spelling, the one a `CREATE TABLE` can carry.
#[test]
fn a_primary_key_that_misses_the_partition_key_is_refused() {
    let mut node = parity::Node::new(&[]);
    let error = node
        .run(
            "CREATE TABLE pk_bad (a integer NOT NULL, b integer NOT NULL, PRIMARY KEY (b)) \
             PARTITION BY LIST (a)",
        )
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert_eq!(
        error.to_string(),
        "PRIMARY KEY constraint on partitioned table must include all partitioning columns"
    );
    assert_eq!(
        error.detail().as_deref(),
        Some(
            "PRIMARY KEY constraint on table \"pk_bad\" lacks column \"a\" which is part of the \
              partition key."
        )
    );
    // Nothing was created: the refusal happens before the catalog is written.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'pk_bad'"),
        [["0"]]
    );
}

/// **`pg_get_partkeydef` on a *partition* is NULL**, which is what makes the schema dumper write
/// the `PARTITION BY` option for the parent and not for its partitions.
#[test]
fn only_the_parent_has_a_partition_key_definition() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE measurements (city_id character varying NOT NULL) PARTITION BY LIST \
         (city_id)",
        "CREATE TABLE measurements_toronto PARTITION OF measurements FOR VALUES IN (1)",
    ]);
    assert_eq!(
        node.rows("SELECT pg_get_partkeydef('measurements'::regclass)"),
        [["LIST (city_id)"]]
    );
    assert_eq!(
        node.rows("SELECT pg_get_partkeydef('measurements_toronto'::regclass)"),
        [["\\N"]]
    );
    // And `pg_inherits` *does* have a row for the partition — which is why `ActiveRecord`, whose
    // `table_options` asks `inherited_table_names` first, dumps a partition as
    // `INHERITS (measurements)` rather than as a partition.
    assert_eq!(
        node.rows(
            "SELECT parent.relname FROM pg_inherits i JOIN pg_class child ON i.inhrelid = \
             child.oid JOIN pg_class parent ON i.inhparent = parent.oid WHERE child.relname = \
             'measurements_toronto'"
        ),
        [["measurements"]]
    );
}
