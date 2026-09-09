//! `DO $$ … $$` — and the one block `ActiveRecord` actually sends.
//!
//! **Run 57: `DO is not supported`, 34 tests over 5 files.** Thirty-six `DO` statements were
//! logged running those five files against PostgreSQL 19, and **all thirty-six are one template**:
//! `create_enum` (`postgresql_adapter.rb:556`) wraps `CREATE TYPE … AS ENUM` in an anonymous block
//! to make it idempotent, because PostgreSQL has no `CREATE TYPE IF NOT EXISTS`. The eleven
//! textual variants differ only in the type name, the labels, whether the created name is
//! schema-qualified, and which of two schema predicates the guard uses.
//!
//! So this is not a PL/pgSQL engine and does not pretend to be. It runs that template and refuses
//! everything else by name — which is contract C2's rule, and what ADR 0031 means by implementing
//! what the capture shows.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own schema and types.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `typname` is a `name` on a real server and `text` here, `typtype`/`typcategory` are
    // `"char"` — the standing `pg_catalog` trade, with the characters identical.
    types: &[
        "SELECT typname, typtype, typcategory FROM pg_type WHERE typname = 'mood';",
        "SELECT typname, typtype FROM pg_type WHERE typname = 'unused';",
    ],
    // **A wrong answer, and named as one** (ADR 0031 rule 3) — and it is not this unit's.
    // `'mood'::regtype` answers `42704 type "mood" does not exist` for a type that **is** there:
    // reproduced with a bare `CREATE TYPE` and no `DO` block at all, so the block creates the type
    // correctly and the cast is what cannot find it. That is run 57's `type "…" does not exist`
    // row (43 tests over 5 files), which b4's `pg_enum` unit owns and lands next; the lines stay
    // here as captured rather than being edited out, so they turn green on their own when it does.
    answers: &[],
};

#[test]
fn every_do_create_enum_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_do_create_enum.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 35,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The block is idempotent, and that is the whole reason it exists.**
///
/// Running it twice is a success and the second run does nothing at all — the labels of the first
/// survive even when the second names different ones. A bare `CREATE TYPE` of the same name is
/// `42710`, which is what `create_enum` is written to avoid.
#[test]
fn the_guard_is_what_makes_a_second_run_a_no_op() {
    let mut node = parity::Node::new(&[]);
    let block = |labels: &str| {
        format!(
            "DO $$ BEGIN IF NOT EXISTS ( SELECT 1 FROM pg_type t JOIN pg_namespace n ON \
             t.typnamespace = n.oid WHERE t.typname = 'mood' AND n.nspname = ANY \
             (current_schemas(false)) ) THEN CREATE TYPE \"mood\" AS ENUM ({labels}); END IF; END \
             $$"
        )
    };
    node.run(&block("'sad', 'ok', 'happy'")).unwrap();
    assert_eq!(
        node.rows("SELECT typname, typtype FROM pg_type WHERE typname = 'mood'"),
        [["mood", "e"]]
    );

    // The second run is a success **and** leaves the first run's labels alone.
    node.run(&block("'totally', 'different'")).unwrap();
    assert_eq!(
        // Joined rather than cast: `'mood'::regtype` is a separate, pre-existing gap (see the
        // divergence list above), and this test is about the block, not about `regtype`.
        node.rows(
            "SELECT e.enumlabel FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid WHERE \
             t.typname = 'mood' ORDER BY e.enumsortorder"
        ),
        [["sad"], ["ok"], ["happy"]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_type WHERE typname = 'mood'"),
        [["1"]]
    );

    // Which is the thing the block buys: without it, this.
    let error = node
        .run("CREATE TYPE \"mood\" AS ENUM ('sad')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42710");

    // And the type it made is a usable one.
    node.run("CREATE TABLE du (id int, m \"mood\")").unwrap();
    node.run("INSERT INTO du VALUES (1, 'happy')").unwrap();
    let error = node.run("INSERT INTO du VALUES (2, 'nope')").unwrap_err();
    assert_eq!(error.sqlstate(), "22P02");
}

/// **Every other `DO` body is refused by name**, which is the honest half of a minimal interpreter.
///
/// A node that ran the template and quietly *ignored* anything else would turn a missing feature
/// into a wrong answer: `DO $$ BEGIN CREATE TABLE t (a int); END $$` really creates a table on a
/// real server, and a silent no-op would leave the next statement to fail on the absence.
#[test]
fn a_body_that_is_not_the_template_is_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    for written in [
        // Effects a no-op would swallow.
        "DO $$ BEGIN CREATE TABLE do_made_this (a int); END $$",
        // `RAISE EXCEPTION` **left this list**: it is `P0001` now, through the error path, and
        // `the_forms_around_the_templates_answer_as_postgresql_does` asserts it there.
        "DO $$ DECLARE n integer; BEGIN SELECT count(*) INTO n FROM pg_class; END $$",
        "DO $$ BEGIN NULL; END $$",
        "DO LANGUAGE plpgsql $$ BEGIN NULL; END $$",
        // The template's shape with the wrong verb inside it: still not the template.
        "DO $$ BEGIN IF NOT EXISTS ( SELECT 1 FROM pg_type t JOIN pg_namespace n ON t.typnamespace \
         = n.oid WHERE t.typname = 'x' AND n.nspname = ANY (current_schemas(false)) ) THEN CREATE \
         TABLE \"x\" (a int); END IF; END $$",
    ] {
        let error = node.run(written).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "for {written}");
        assert!(
            error.to_string().contains("DO"),
            "the refusal names the construct: {error}"
        );
    }
    // And nothing was quietly created on the way through.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_class WHERE relname = 'do_made_this'"),
        [["0"]]
    );
}

/// **The suite's other `DO` body**, and the one my own extraction nearly missed.
///
/// `postgresql_adapter_test.rb` raises a warning in seven tests to exercise
/// `db_warnings_action` — `do $$ BEGIN RAISE WARNING 'foo'; END; $$`, written with a lowercase
/// `do`, which is why a case-sensitive grep of the oracle's log found thirty-six `create_enum`
/// blocks and none of these.
///
/// What the tests read is the line `libpq` prints, so what matters is the severity word and the
/// message. `INFO` and `EXCEPTION` are **not** implemented and are refused by name: the first has
/// no severity token on this wire, and the second is an error rather than a notice.
#[test]
fn raise_notice_and_warning_reach_the_client() {
    let mut node = parity::Node::new(&[]);
    for (written, severity, message) in [
        ("do $$ BEGIN RAISE WARNING 'foo'; END; $$", "WARNING", "foo"),
        (
            "DO $$ BEGIN RAISE NOTICE 'hello'; END $$",
            "NOTICE",
            "hello",
        ),
        // `''` inside the literal is one quote.
        (
            "do $$ BEGIN RAISE WARNING 'it''s here'; END; $$",
            "WARNING",
            "it's here",
        ),
    ] {
        node.run(written)
            .unwrap_or_else(|error| panic!("{written}: {error}"));
        let notices = node.executor_notices();
        assert_eq!(notices.len(), 1, "one notice for {written}: {notices:?}");
        assert_eq!(notices[0].severity().as_str(), severity, "for {written}");
        assert_eq!(notices[0].to_string(), message, "for {written}");
    }

    // The levels this node does not have stay refused by name rather than being downgraded into
    // one that would print the wrong word.
    for written in [
        "do $$ BEGIN RAISE INFO 'i'; END; $$",
        "do $$ BEGIN RAISE LOG 'l'; END; $$",
        // `EXCEPTION` is **not** in this list any more: it is not a severity this wire lacks, it
        // is an error, and it answers `P0001`.
        "do $$ BEGIN RAISE WARNING 'a', 'b'; END; $$",
    ] {
        let error = node.run(written).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "for {written}");
    }
}

/// The forms around the two templates, with the capture's own answers.
///
/// **`corpus/pg19_do_block.txt` cannot be replayed as it stands, and that is a property of the
/// capture rather than of this node.** It is one `BEGIN … ROLLBACK` block, and only the statements
/// PostgreSQL *errors* on are wrapped in savepoints. Every form this node refuses where PostgreSQL
/// succeeds — a non-template body, `BEGIN NULL; END`, `CREATE TABLE` inside a block — aborts the
/// transaction, and the sixteen statements after it answer `25P02`. Declaring a divergence does not
/// prevent the abort; it only stops the row itself being counted as a disagreement.
///
/// So the answers below are taken from that capture line by line, and re-capturing it with a
/// savepoint per statement is the harness lane's to do. Recorded in `docs/plans/do-blocks.md`.
#[test]
fn the_forms_around_the_templates_answer_as_postgresql_does() {
    let mut node = parity::Node::new(&[]);
    // **Both `LANGUAGE` spellings reach the templates now**, which is the fix: before this
    // revision a template body was refused outright if the statement named its language.
    for sql in [
        "DO $$ BEGIN RAISE NOTICE 'plain'; END $$",
        "DO LANGUAGE plpgsql $$ BEGIN RAISE NOTICE 'leading'; END $$",
        "DO $$ BEGIN RAISE NOTICE 'trailing'; END $$ LANGUAGE plpgsql",
        // The dollar tag is read from the source and always could be named.
        "DO $do$ BEGIN RAISE NOTICE 'tagged'; END $do$",
        "DO $x$ BEGIN RAISE WARNING 'tagged'; END $x$",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "(a command, no result set)",
            "{sql}"
        );
    }

    // **A language this node does not run is `42704` naming that language**, not `0A000` naming
    // `DO`: a real server resolves the language before it reads the body, so what is wrong is the
    // language. Both spellings, because both reach the same decision.
    for sql in [
        "DO $$ BEGIN NULL; END $$ LANGUAGE nosuchlang",
        "DO LANGUAGE nosuchlang $$ BEGIN NULL; END $$",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "!42704 language \"nosuchlang\" does not exist",
            "{sql}"
        );
    }

    // **`RAISE EXCEPTION` is a failure and must not read as a success.** `P0001` with the raised
    // text as the whole message, through the error path and never the notice path — the
    // distinction ADR 0058 refused to blur.
    assert_eq!(
        node.answer("DO $$ BEGIN RAISE EXCEPTION 'boom'; END $$")
            .to_string(),
        "!P0001 boom"
    );

    // And what stays refused, by the ruling rather than by an absence: a body that is a program.
    for sql in [
        "DO $$ DECLARE n integer; BEGIN SELECT count(*) INTO n FROM pg_class; END $$",
        "DO $$ BEGIN CREATE TABLE do_made_this (a int); END $$",
        "DO $$ BEGIN NULL; END $$",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "!0A000 DO is not supported",
            "{sql}"
        );
    }
}
