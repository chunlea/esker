//! `ALTER TABLE … RENAME COLUMN` and its neighbour `RENAME TO` — 18 tests over 5 files, and 12
//! more over 3 for the table form.
//!
//! **A rename does not move the column.** Its ordinal is unchanged, so every index, constraint,
//! default and primary key goes on pointing at the same attribute — and everything that *renders*
//! a definition comes back with the new name because it renders from the ordinal. That is what
//! makes this a one-field write here as well: only `attname` is stored as text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **`CREATE VIEW` is a named refusal older than this unit**, so the three lines that need
        // a view are its consequences and not gaps of their own. The property the view is here to
        // show — that a rename is *rendered* through and does not break what points at the column
        // — is proved by the index and constraint definitions above it, which do the same job by
        // the same mechanism.
        (
            "CREATE VIEW rc_view AS SELECT id, name FROM rc",
            "`0A000 CREATE VIEW is not supported`. Every line below that names `rc_view` follows \
             from it.",
        ),
        (
            "SELECT 'r', pg_get_viewdef('rc_view'::regclass, true)",
            "The view was never created.",
        ),
        ("SELECT 'r', count(*) FROM rc_view", "The same."),
        ("DROP VIEW rc_view", "The same."),
    ],
};

#[test]
fn every_rename_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_rename_column.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The one thing this node refuses that a real server does not**, pinned so it is a decision
/// rather than a surprise.
///
/// A `CHECK`, an `EXCLUDE` key and a partial index's predicate are stored as the **text** the user
/// wrote and re-lowered on every write. PostgreSQL re-renders those from the attnum, so a rename
/// carries them across; here the text would go on naming a column that is gone, and the constraint
/// would stop resolving — a table that silently stops accepting rows, which is worse than a
/// refusal. So the rename is refused by name, and the message says which constraint is in the way.
///
/// The capture never reaches this because AR renames plain columns. It is written down because the
/// day those expressions are stored resolved rather than as text, this refusal should go.
#[test]
fn renaming_a_column_an_expression_names_is_refused() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE rx (id bigint PRIMARY KEY, qty bigint)")
        .unwrap();
    node.run("ALTER TABLE rx ADD CONSTRAINT rx_qty_check CHECK (qty >= 0)")
        .unwrap();

    let refused = node
        .answer("ALTER TABLE rx RENAME COLUMN qty TO amount")
        .to_string();
    assert!(
        refused.starts_with("!0A000") && refused.contains("rx_qty_check"),
        "the refusal must name the constraint in the way: {refused}"
    );
    // Refused means unchanged: the column is still there under its old name and still checked.
    assert!(
        node.answer("INSERT INTO rx VALUES (1, -1)")
            .to_string()
            .starts_with("!23514"),
        "the constraint still works"
    );

    // A column no expression mentions renames freely, even on the same table.
    node.run("ALTER TABLE rx RENAME COLUMN id TO pk").unwrap();
    node.run("INSERT INTO rx VALUES (1, 5)").unwrap();
    assert_eq!(node.rows("SELECT pk, qty FROM rx"), [["1", "5"]]);
}

/// **A rename keeps the rows**, which is the half a corpus of catalog reads does not prove: the
/// column's ordinal is what the row codec decodes at, and the rename does not touch it.
#[test]
fn the_rows_written_before_a_rename_read_after_it() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE rr (id bigint PRIMARY KEY, name text, qty bigint)")
        .unwrap();
    node.run("INSERT INTO rr VALUES (1, 'one', 10)").unwrap();
    node.run("ALTER TABLE rr RENAME COLUMN name TO title")
        .unwrap();
    node.run("INSERT INTO rr VALUES (2, 'two', 20)").unwrap();

    // Both rows, and the renamed column reads the value it was written with under the old name.
    assert_eq!(
        node.rows("SELECT id, title, qty FROM rr ORDER BY id"),
        [["1", "one", "10"], ["2", "two", "20"]]
    );
    // The old name is gone from every clause at once.
    assert!(
        node.answer("SELECT name FROM rr")
            .to_string()
            .starts_with("!42703")
    );
    // And renaming the table leaves the rows and the index names alone.
    node.run("CREATE INDEX index_rr_on_qty ON rr (qty)")
        .unwrap();
    node.run("ALTER TABLE rr RENAME TO rr2").unwrap();
    assert_eq!(node.rows("SELECT count(*) FROM rr2"), [["2"]]);
    assert_eq!(
        node.rows("SELECT indexname FROM pg_indexes WHERE tablename = 'rr2' ORDER BY indexname"),
        [["index_rr_on_qty"], ["rr_pkey"]]
    );
    // The old table name is free again, which it would not be if the name record had leaked.
    node.run("CREATE TABLE rr (id bigint PRIMARY KEY)").unwrap();
}

/// **Run 55's cliff, reproduced.** From file 326 every schema load failed with
/// `relation "references_id_seq" already exists`, and 101 files went with it.
///
/// The mechanism is a refusal, not a leak. `rename_table` renames the table and then renames the
/// sequence its `serial` column owns with a second statement — `ALTER TABLE <seq> RENAME TO …`,
/// a *sequence* named where the grammar says table (`schema_statements.rb:459,474`). A real server
/// runs it; this node answered `42809`. So the table moved and its sequence kept the **old** name.
///
/// After that the suite's own `force: true` cycle cannot clean up: `DROP TABLE IF EXISTS
/// "references"` finds nothing, because the table is called something else now — and the
/// `CREATE TABLE` that follows wants `references_id_seq`, which is still there, still owned by the
/// renamed table. Every later schema load hits the same wall, and the node answers every health
/// check perfectly while it happens.
#[test]
fn a_renamed_table_leaves_no_sequence_to_collide_with() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE refs (id bigserial PRIMARY KEY, name text)")
        .unwrap();

    // What `rename_table` sends, both statements.
    node.run("ALTER TABLE refs RENAME TO refs2").unwrap();
    node.run("ALTER TABLE refs_id_seq RENAME TO refs2_id_seq")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'refs_id_seq'"),
        [["0"]],
        "the sequence follows its table, or the old name is left to collide with"
    );

    // The schema's `force: true` cycle, which is where the cliff was: the drop finds nothing under
    // the old name, and the create must still succeed.
    node.run("DROP TABLE IF EXISTS refs").unwrap();
    node.run("CREATE TABLE refs (id bigserial PRIMARY KEY, name text)")
        .unwrap();
    // Both tables and both sequences now exist, under four distinct names.
    assert_eq!(
        node.rows(
            "SELECT relname FROM pg_class WHERE relname LIKE 'refs%' AND relkind = 'S' ORDER BY relname"
        ),
        [["refs2_id_seq"], ["refs_id_seq"]]
    );
    // And the sequence still fills the column it belongs to.
    node.run("INSERT INTO refs (name) VALUES ('a')").unwrap();
    node.run("INSERT INTO refs2 (name) VALUES ('b')").unwrap();
    assert_eq!(node.rows("SELECT id FROM refs"), [["1"]]);
    assert_eq!(node.rows("SELECT id FROM refs2"), [["1"]]);
}

/// **The same leak from the other direction: a `serial` column's sequence must die with the
/// column.**
///
/// `DROP COLUMN` takes the sequence out of the table's record — measured on the oracle, and the
/// `DROP COLUMN` unit asserts it — but a sequence has a **name record** of its own, and a name that
/// outlives what it points at is what stopped run 55's schema loads. This is the third relation
/// kind to need that reconciliation and the one where the cost is highest: the name a `serial`
/// column's sequence holds is the name the *next* `CREATE TABLE` wants.
#[test]
fn dropping_a_serial_column_takes_its_sequence_name_with_it() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE sq (id bigint PRIMARY KEY, counter bigserial)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'sq_counter_seq'"),
        [["1"]]
    );

    node.run("ALTER TABLE sq DROP COLUMN counter").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'sq_counter_seq'"),
        [["0"]],
        "the sequence goes with the column, name record and all"
    );

    // And the name is free: the `force: true` cycle re-creates the table and wants that exact
    // sequence name back. This is the statement 101 files could not get past.
    node.run("DROP TABLE IF EXISTS sq").unwrap();
    node.run("CREATE TABLE sq (id bigint PRIMARY KEY, counter bigserial)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'sq_counter_seq'"),
        [["1"]]
    );
    node.run("INSERT INTO sq (id) VALUES (1)").unwrap();
    assert_eq!(node.rows("SELECT counter FROM sq"), [["1"]]);
}

/// **Run 56's cliff.** Five files stopped on `corrupt data: a name points at table N, which is not
/// there`, and the standalone repro is one file: run `migration/rename_table_test.rb` on a fresh
/// node and the next `SELECT count(*) FROM pg_class` cannot answer, while `SELECT 1` still can.
///
/// A *name* outliving its object again. This walks the statements that file sends and asks
/// `pg_class` after **every one**, so the statement that leaks names itself rather than the one
/// that trips over it afterwards.
#[test]
fn renaming_a_table_and_back_leaves_no_dangling_name() {
    let mut node = parity::Node::new(&[]);

    // `rename_table` sends up to four kinds of statement: the table, the primary key's index, the
    // sequence, and then one `ALTER INDEX … RENAME TO` per index whose name follows the
    // `index_<table>_on_<column>` convention (`rename_table_indexes`).
    let script = [
        "CREATE TABLE test_models (id bigserial PRIMARY KEY, created_at timestamptz, updated_at timestamptz)",
        "CREATE INDEX index_test_models_on_created_at ON test_models (created_at)",
        "CREATE TABLE \"references\" (id bigserial PRIMARY KEY, url text)",
        "ALTER TABLE test_models ADD COLUMN url text",
        "ALTER TABLE test_models DROP COLUMN created_at",
        "ALTER TABLE test_models DROP COLUMN updated_at",
        // `rename_table :test_models, :octopi`
        "ALTER TABLE test_models RENAME TO octopi",
        "ALTER INDEX test_models_pkey RENAME TO octopi_pkey",
        "ALTER TABLE test_models_id_seq RENAME TO octopi_id_seq",
        // the reserved-words test: a three-way shuffle
        "ALTER TABLE \"references\" RENAME TO old_references",
        "ALTER INDEX references_pkey RENAME TO old_references_pkey",
        "ALTER TABLE references_id_seq RENAME TO old_references_id_seq",
        "ALTER TABLE octopi RENAME TO \"references\"",
        "ALTER INDEX octopi_pkey RENAME TO references_pkey",
        "ALTER TABLE octopi_id_seq RENAME TO references_id_seq",
        // and back again, which is what the `ensure` block does
        "ALTER TABLE \"references\" RENAME TO test_models",
        "ALTER INDEX references_pkey RENAME TO test_models_pkey",
        "ALTER TABLE references_id_seq RENAME TO test_models_id_seq",
        "ALTER TABLE old_references RENAME TO \"references\"",
        "ALTER INDEX old_references_pkey RENAME TO references_pkey",
        "ALTER TABLE old_references_id_seq RENAME TO references_id_seq",
        // the teardown drops what the helper made
        "DROP TABLE test_models",
        "DROP TABLE \"references\"",
    ];

    // `test_rename_table_with_long_table_name_and_index` renames to a 63-byte name, and
    // `rename_table_indexes` then builds index names from it that run past the limit — where this
    // node **truncates** an identifier rather than refusing it, exactly as a real server does. A
    // truncated name is a different string from the one the statement named, and every name record
    // has to be written and deleted under the same one.
    let long = "a".repeat(63);
    let script: Vec<String> = script
        .iter()
        .map(|s| (*s).to_owned())
        .chain([
            "CREATE TABLE lt (id bigserial PRIMARY KEY, url text)".to_owned(),
            "CREATE INDEX index_lt_on_url ON lt (url)".to_owned(),
            format!("ALTER TABLE lt RENAME TO {long}"),
            format!("ALTER INDEX index_lt_on_url RENAME TO index_{long}_on_url"),
            format!("ALTER INDEX index_{long}_on_url RENAME TO index_lt_on_url"),
            format!("ALTER TABLE {long} RENAME TO lt"),
            "DROP TABLE lt".to_owned(),
        ])
        .collect();

    for statement in script {
        let statement = statement.as_str();
        let outcome = node.answer(statement).to_string();
        assert!(!outcome.starts_with('!'), "{statement} -> {outcome}");
        // **`pg_class` is the fsck**: reading it walks every name record and follows it, so a name
        // that outlived its object is a `corrupt data` here and nowhere else. Asked after every
        // statement, because the one that leaks is the one *before* the one that trips.
        let seen = node.answer("SELECT count(*) FROM pg_class").to_string();
        assert!(
            !seen.starts_with('!'),
            "after `{statement}` the catalog cannot be read: {seen}"
        );
    }
}

/// **A randomised walk over the DDL that writes name records**, with the catalog checked after
/// every statement.
///
/// Run 56's cliff is a *name* pointing at a table that is gone, and five reconstructions of
/// `rename_table_test.rb` by hand did not produce one — so this stops reconstructing and searches.
/// The statements are the ones that write or delete a name record (create, drop, and the three
/// renames), over a deliberately tiny universe of names so that collisions and re-uses happen
/// often; reading `pg_class` walks every name and follows it, which is the only thing that can see
/// the damage.
///
/// The generator is a plain LCG so a failure names a seed that reproduces it exactly.
#[test]
fn no_ddl_order_leaves_a_name_pointing_at_a_table_that_is_gone() {
    const NAMES: [&str; 3] = ["ta", "tb", "tc"];

    for seed in 0..64_u64 {
        let mut node = parity::Node::new(&[]);
        let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        let mut history: Vec<String> = Vec::new();

        for _ in 0..24 {
            let name = NAMES[next() % NAMES.len()];
            let other = NAMES[next() % NAMES.len()];
            let statement = match next() % 7 {
                0 => format!("CREATE TABLE {name} (id bigserial PRIMARY KEY, url text)"),
                1 => format!("DROP TABLE IF EXISTS {name}"),
                2 => format!("ALTER TABLE {name} RENAME TO {other}"),
                3 => format!("ALTER INDEX {name}_pkey RENAME TO {other}_pkey"),
                4 => format!("ALTER TABLE {name}_id_seq RENAME TO {other}_id_seq"),
                5 => format!("CREATE INDEX index_{name}_on_url ON {name} (url)"),
                // Renaming a **secondary** index, which is the one that leaked: the primary key's
                // name lives in its own field and was already reconciled, so a fuzz that renamed
                // only `_pkey` ran green against the code that had the bug.
                _ => format!("ALTER INDEX index_{name}_on_url RENAME TO index_{other}_on_url"),
            };
            // A statement may fail — a name taken, a table absent — and that is ordinary. What may
            // never happen is the catalog becoming unreadable afterwards.
            let _ = node.answer(&statement);
            history.push(statement);
            let seen = node.answer("SELECT count(*) FROM pg_class").to_string();
            assert!(
                !seen.starts_with('!'),
                "seed {seed}: the catalog cannot be read after this sequence:\n  {}\n{seen}",
                history.join("\n  ")
            );
        }
    }
}

/// **Run 56's error, made in three statements.** `corrupt data: a name points at table N, which is
/// not there` — the message five files stopped on, and the sixth name-record leak.
///
/// `replace_table` reconciled index names by **id**, and a rename does not change an id: the
/// comparison saw the index as still present, so the *old* name record stayed, pointing at the
/// table. Nothing complains while the table is there — two names resolve to one index. Then the
/// table is dropped, `drop_table` deletes the names the record still lists (the new one), and the
/// old one is left pointing at a table that is gone. The next read of `pg_class` walks it and
/// cannot answer, while `SELECT 1` still can — which is exactly the shape the cliff detector saw
/// and the wedge detector could not.
///
/// `ALTER INDEX … RENAME TO` is what `rename_table` sends for the primary key's index and for every
/// index named by convention, which is why `migration/rename_table_test.rb` is where it surfaced.
#[test]
fn a_renamed_index_leaves_no_name_behind_when_its_table_is_dropped() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE t (id bigint PRIMARY KEY, url text)")
        .unwrap();
    node.run("CREATE INDEX index_t_on_url ON t (url)").unwrap();

    node.run("ALTER INDEX index_t_on_url RENAME TO index_t_on_url2")
        .unwrap();
    // The old name is gone *now*, which is the whole fix: one name, one index.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'index_t_on_url'"),
        [["0"]]
    );

    node.run("DROP TABLE t").unwrap();
    // And the catalog is still readable. With the old name left behind this is
    // `XX001 corrupt data: a name points at table N, which is not there`, from a statement that
    // has nothing to do with the rename — and every later schema load meets it.
    let seen = node.answer("SELECT count(*) FROM pg_class").to_string();
    assert!(
        !seen.starts_with('!'),
        "the catalog must survive a renamed index whose table was dropped: {seen}"
    );
    // The same for the primary key's index, which is the one `rename_table` always renames.
    node.run("CREATE TABLE t (id bigint PRIMARY KEY)").unwrap();
    node.run("ALTER INDEX t_pkey RENAME TO t2_pkey").unwrap();
    node.run("DROP TABLE t").unwrap();
    let seen = node.answer("SELECT count(*) FROM pg_class").to_string();
    assert!(
        !seen.starts_with('!'),
        "and for a renamed primary key: {seen}"
    );
}
