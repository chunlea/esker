//! Declarative partitioning — statements 781-786 of `postgresql_specific_schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog trade, and nothing new: `relkind` is a `"char"` on a real server and
    // `relname` a `name`, and this node has neither type — both are `text`, with the same
    // characters in them. Every row agrees.
    types: &[
        "SELECT 'r', relkind, relispartition, relhassubclass FROM pg_class WHERE relname = 'measurements'",
        "SELECT 'r', relkind, relispartition FROM pg_class WHERE relname = 'index_measurements_on_logdate_and_city_id'",
        "SELECT 'r', c.relname, c.relkind, c.relispartition FROM pg_class c WHERE c.relname LIKE 'measurements%' ORDER BY c.relname",
    ],
    answers: &[
        (
            "ALTER TABLE \"measurements\" ADD CONSTRAINT \"m_bad_pk\" PRIMARY KEY (\"logdate\")",
            "PostgreSQL refuses it for the partition key it lacks; this node refuses the whole \
             action, which is older than this unit — `ADD CONSTRAINT ... PRIMARY KEY` has never \
             been built here. Both refuse with `0A000`; what is missing is the action, not the \
             partition rule, and `CREATE UNIQUE INDEX` two lines above proves the rule itself in \
             PostgreSQL's own words",
            "pg19_partition.txt:87",
        ),
        (
            "SELECT 'r', tableoid::regclass::text, city_id, logdate, peaktemp FROM \
             \"measurements\" ORDER BY city_id",
            "`tableoid` is a **system column**, and this node has none of the six — a scan's rows \
             are the table's declared columns and nothing else, which is a width contract every \
             join and projection in the crate reads. The fact it proves here is proved instead by \
             each partition's own `count(*)` in `an_insert_is_routed_by_the_key` below: a row \
             inserted through the parent is in exactly one partition, and which one. It is the \
             **first** statement in the capture this node refuses, so the thirty-seven after it \
             are swallowed by the aborted block and counted rather than declared — every one of \
             them is asserted directly in this file instead",
            "pg19_partition.txt:100",
        ),
        (
            "ALTER TABLE \"measurements\" DETACH PARTITION \"measurements_concepcion\"",
            "A **parser gap**, named rather than approximated: `sqlparser` 0.62 reads only \
             ClickHouse\u{2019}s `ATTACH|DETACH PARTITION <expr>`, and PostgreSQL\u{2019}s form — with \
             `FOR VALUES` on the attach — does not parse at all under its PostgreSQL dialect. \
             Neither spelling is in `postgresql_specific_schema.rb`: the capture probes them, the \
             suite never sends them. `0A000` naming the action is what this node answers",
            "pg19_partition.txt:122",
        ),
        (
            "ALTER TABLE \"measurements\" ATTACH PARTITION \"measurements_concepcion\" FOR VALUES IN (2)",
            "The other half of the parser gap above, and the half that carries a bound — \
             `FOR VALUES IN (2)` has nowhere to go in `sqlparser`\u{2019}s ClickHouse-shaped \
             `AttachPartition`. Refused by name",
            "pg19_partition.txt:126",
        ),
    ],
};

#[test]
fn every_partition_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_partition.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The suite's four statements, and the catalog they leave behind.
#[test]
fn statements_781_to_786() {
    let mut node = parity::Node::new(&[]);
    node.run(
        "CREATE TABLE \"measurements\" (\"city_id\" character varying NOT NULL, \"logdate\" date \
         NOT NULL, \"peaktemp\" integer, \"unitsales\" integer) PARTITION BY LIST (city_id)",
    )
    .unwrap();
    // **`relkind` is `p`**, and the table is not itself a partition.
    assert_eq!(
        node.rows("SELECT relkind, relispartition FROM pg_class WHERE relname = 'measurements'"),
        [["p", "f"]]
    );
    assert_eq!(
        node.rows("SELECT pg_get_partkeydef('measurements'::regclass)"),
        [["LIST (city_id)"]]
    );
    // `partstrat` is a one-letter code and `partattrs` an `int2vector` — neither is the DDL word.
    assert_eq!(
        node.rows(
            "SELECT partstrat, partnatts, partattrs FROM pg_partitioned_table WHERE partrelid = \
             'measurements'::regclass"
        ),
        [["l", "1", "1"]]
    );

    node.run(
        "CREATE UNIQUE INDEX \"index_measurements_on_logdate_and_city_id\" ON \"measurements\" \
         (\"logdate\", \"city_id\")",
    )
    .unwrap();
    // **A capital `I`** — a partitioned index is its own relkind, and a `relkind IN ('r','p')`
    // filter that forgets it still passes every test that never makes one.
    assert_eq!(
        node.rows(
            "SELECT relkind FROM pg_class WHERE relname = \
             'index_measurements_on_logdate_and_city_id'"
        ),
        [["I"]]
    );

    node.run("CREATE TABLE \"measurements_toronto\" PARTITION OF measurements FOR VALUES IN (1)")
        .unwrap();
    node.run(
        "CREATE TABLE \"measurements_concepcion\" PARTITION OF measurements FOR VALUES IN (2)",
    )
    .unwrap();
    // **The bound is coerced to the key's type and printed back quoted**: the suite writes the
    // integer `1` against a `character varying` column and a real server stores `'1'`.
    //
    // And **a partition's own index is a partition too** — `relispartition` is `t` for it, with a
    // NULL bound, so this filter returns four rows and not two. Measured; the corpus pins the
    // same four.
    assert_eq!(
        node.rows(
            "SELECT relname, pg_get_expr(relpartbound, oid) FROM pg_class WHERE relispartition \
             ORDER BY relname"
        ),
        vec![
            vec!["measurements_concepcion", "FOR VALUES IN ('2')"],
            vec!["measurements_concepcion_logdate_city_id_idx", "\\N"],
            vec!["measurements_toronto", "FOR VALUES IN ('1')"],
            vec!["measurements_toronto_logdate_city_id_idx", "\\N"],
        ]
    );
    // Each partition silently got its own child index, which the suite never named.
    assert_eq!(
        node.rows(
            "SELECT count(*) FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE \
             x.indrelid IN ('measurements_toronto'::regclass, \
             'measurements_concepcion'::regclass)"
        ),
        [["2"]]
    );
}

/// **A row inserted into the parent lands in a partition**, and `tableoid` says which.
#[test]
fn an_insert_is_routed_by_the_key() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE m (city_id character varying NOT NULL, peaktemp integer) PARTITION BY LIST (city_id)",
        "CREATE TABLE m_t PARTITION OF m FOR VALUES IN (1)",
        "CREATE TABLE m_c PARTITION OF m FOR VALUES IN (2)",
    ]);
    node.run("INSERT INTO m (city_id, peaktemp) VALUES ('1', 30)")
        .unwrap();
    node.run("INSERT INTO m (city_id, peaktemp) VALUES ('2', 31)")
        .unwrap();
    // **Which partition each row is in** — proved by the partitions' own counts, because
    // `tableoid` is a system column this node does not have (declared in the corpus above).
    assert_eq!(node.rows("SELECT count(*) FROM m"), [["2"]]);
    assert_eq!(node.rows("SELECT city_id FROM m_t"), [["1"]]);
    assert_eq!(node.rows("SELECT city_id FROM m_c"), [["2"]]);

    // No partition takes it: `23514`, and the message names the **parent**.
    let error = node
        .run("INSERT INTO m (city_id, peaktemp) VALUES ('99', 32)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23514");
    assert_eq!(
        error.to_string(),
        "no partition of relation \"m\" found for row"
    );
    // Straight into the wrong partition: `23514` too, naming the **partition**.
    let error = node
        .run("INSERT INTO m_t (city_id, peaktemp) VALUES ('2', 33)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23514");
    assert!(error.to_string().contains("m_t"), "{error}");
}

/// **A key that must include every partition column**, or the index is refused.
#[test]
fn a_unique_index_must_cover_the_partition_key() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE m (city_id character varying NOT NULL, logdate date NOT NULL) PARTITION BY LIST (city_id)",
    ]);
    let error = node
        .run("CREATE UNIQUE INDEX m_bad ON m (logdate)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert_eq!(
        error.to_string(),
        "UNIQUE constraint on partitioned table must include all partitioning columns"
    );
    // With the key column in it, the same index is accepted.
    node.run("CREATE UNIQUE INDEX m_ok ON m (logdate, city_id)")
        .unwrap();
}

/// **An `UPDATE` that changes the key moves the row** rather than failing.
#[test]
fn an_update_moves_a_row_between_partitions() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE m (city_id character varying NOT NULL, peaktemp integer) PARTITION BY LIST (city_id)",
        "CREATE TABLE m_t PARTITION OF m FOR VALUES IN (1)",
        "CREATE TABLE m_c PARTITION OF m FOR VALUES IN (2)",
    ]);
    node.run("INSERT INTO m (city_id, peaktemp) VALUES ('1', 30)")
        .unwrap();
    node.run("UPDATE m SET city_id = '2' WHERE city_id = '1'")
        .unwrap();
    // The row left `m_t` and arrived in `m_c` — moved, not refused, and not duplicated.
    assert_eq!(node.rows("SELECT count(*) FROM m_t"), [["0"]]);
    assert_eq!(
        node.rows("SELECT city_id, peaktemp FROM m_c"),
        [["2", "30"]]
    );
    assert_eq!(node.rows("SELECT count(*) FROM m"), [["1"]]);
}

/// A `DEFAULT` partition takes what nothing else does, and prints its bound as the bare word.
#[test]
fn a_default_partition_catches_the_rest() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE m (city_id character varying NOT NULL) PARTITION BY LIST (city_id)",
        "CREATE TABLE m_t PARTITION OF m FOR VALUES IN (1)",
        "CREATE TABLE m_d PARTITION OF m DEFAULT",
    ]);
    node.run("INSERT INTO m (city_id) VALUES ('99')").unwrap();
    assert_eq!(node.rows("SELECT city_id FROM m_d"), [["99"]]);
    assert_eq!(node.rows("SELECT count(*) FROM m_t"), [["0"]]);
    // And a value a list claims still goes to that list, whatever order the two were declared in.
    node.run("INSERT INTO m (city_id) VALUES ('1')").unwrap();
    assert_eq!(node.rows("SELECT city_id FROM m_t"), [["1"]]);
    assert_eq!(
        node.rows("SELECT pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = 'm_d'"),
        [["DEFAULT"]]
    );
}

/// An overlapping partition is `42P17`, and the message names the one it would overlap.
#[test]
fn an_overlapping_partition_is_refused() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE m (city_id character varying NOT NULL) PARTITION BY LIST (city_id)",
        "CREATE TABLE m_t PARTITION OF m FOR VALUES IN (1)",
    ]);
    let error = node
        .run("CREATE TABLE m_dup PARTITION OF m FOR VALUES IN (1)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P17");
    assert_eq!(
        error.to_string(),
        "partition \"m_dup\" would overlap partition \"m_t\""
    );
}

/// **A `RANGE` bound is half-open**, and `MINVALUE`/`MAXVALUE` print back as the words.
///
/// The two partitions share the number `10` and do not overlap: `FROM (MINVALUE) TO (10)` takes
/// `9` and `FROM (10) TO (MAXVALUE)` takes `10`. A closed upper end would put `10` in both.
#[test]
fn a_range_partition_is_half_open() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE mp_range (id int8, r int4) PARTITION BY RANGE (r)",
        "CREATE TABLE mp_range_lo PARTITION OF mp_range FOR VALUES FROM (MINVALUE) TO (10)",
        "CREATE TABLE mp_range_hi PARTITION OF mp_range FOR VALUES FROM (10) TO (MAXVALUE)",
    ]);
    // **The words, verbatim** — and the `10` **unquoted**, where the `LIST` bound on a
    // `character varying` key prints `'1'`. The key's type decides, which is why the bound is
    // coerced before it is stored.
    assert_eq!(
        node.rows(
            "SELECT relname, pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname LIKE \
             'mp_range_%' ORDER BY relname"
        ),
        vec![
            vec!["mp_range_hi", "FOR VALUES FROM (10) TO (MAXVALUE)"],
            vec!["mp_range_lo", "FOR VALUES FROM (MINVALUE) TO (10)"],
        ]
    );
    node.run("INSERT INTO mp_range VALUES (1, 9), (2, 10)")
        .unwrap();
    assert_eq!(node.rows("SELECT id FROM mp_range_lo"), [["1"]]);
    assert_eq!(node.rows("SELECT id FROM mp_range_hi"), [["2"]]);
    assert_eq!(node.rows("SELECT count(*) FROM mp_range"), [["2"]]);
    // `partstrat` is `r` where the LIST table's is `l`, and `pg_get_partkeydef` says the word.
    assert_eq!(
        node.rows(
            "SELECT partstrat FROM pg_partitioned_table WHERE partrelid = 'mp_range'::regclass"
        ),
        [["r"]]
    );
    assert_eq!(
        node.rows("SELECT pg_get_partkeydef('mp_range'::regclass)"),
        [["RANGE (r)"]]
    );
    // Two ranges that really do overlap are still `42P17`.
    let error = node
        .run("CREATE TABLE mp_range_mid PARTITION OF mp_range FOR VALUES FROM (5) TO (20)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42P17");
}

/// **`HASH` is refused by name**, because nothing captured how it routes.
#[test]
fn hash_partitioning_is_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    let error = node
        .run("CREATE TABLE h (id int8) PARTITION BY HASH (id)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert_eq!(
        error.to_string(),
        "CREATE TABLE ... PARTITION BY HASH is not supported"
    );
}

/// **The duplicate names the partition's own index, not the parent's.**
///
/// Statement 782 creates `index_measurements_on_logdate_and_city_id` before a single partition
/// exists; the row that collides is refused by `measurements_toronto_logdate_city_id_idx`, an
/// index the suite never wrote.
#[test]
fn a_duplicate_key_names_the_partitions_own_index() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE measurements (city_id character varying NOT NULL, logdate date NOT NULL) \
         PARTITION BY LIST (city_id)",
        "CREATE UNIQUE INDEX index_measurements_on_logdate_and_city_id ON measurements (logdate, \
         city_id)",
        "CREATE TABLE measurements_toronto PARTITION OF measurements FOR VALUES IN (1)",
        "INSERT INTO measurements (city_id, logdate) VALUES ('1', '2026-09-01')",
    ]);
    let error = node
        .run("INSERT INTO measurements (city_id, logdate) VALUES ('1', '2026-09-01')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23505");
    assert_eq!(
        error.to_string(),
        "duplicate key value violates unique constraint \
         \"measurements_toronto_logdate_city_id_idx\""
    );
    // **`ON ONLY`** in the definition of the index that covers every partition, and it does not
    // change when a partition is added.
    assert_eq!(
        node.rows("SELECT pg_get_indexdef('index_measurements_on_logdate_and_city_id'::regclass)"),
        [[
            "CREATE UNIQUE INDEX index_measurements_on_logdate_and_city_id ON ONLY \
             public.measurements USING btree (logdate, city_id)"
        ]]
    );
}

/// **`DROP TABLE <partition>` is allowed outright** — the parent does not protect it.
#[test]
fn a_partition_can_be_dropped() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE m (city_id character varying NOT NULL) PARTITION BY LIST (city_id)",
        "CREATE TABLE m_t PARTITION OF m FOR VALUES IN (1)",
        "CREATE TABLE m_c PARTITION OF m FOR VALUES IN (2)",
        "INSERT INTO m (city_id) VALUES ('1')",
        "INSERT INTO m (city_id) VALUES ('2')",
    ]);
    node.run("DROP TABLE m_t").unwrap();
    // The parent loses that partition's rows with it, and keeps the rest.
    assert_eq!(node.rows("SELECT count(*) FROM m"), [["1"]]);
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'm_t'"),
        [["0"]]
    );
    // And the bound it held is free again.
    node.run("CREATE TABLE m_t2 PARTITION OF m FOR VALUES IN (1)")
        .unwrap();
}
