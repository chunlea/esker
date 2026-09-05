//! The last cell of r1's matrix that a `MemoryBackend` cannot reach: **binds through a view against
//! real stores**, over the extended protocol.
//!
//! `tests/view_bind_at_the_wire.rs` runs the same statements through the real listener and
//! `node::Sessions`, and passes. This one changes exactly one variable — the storage under the
//! executor — because that is the only difference left between what this crate tests and what the
//! scoreboard's node does.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cluster/mod.rs"]
mod cluster;

use esker_sql::pgwire::message::Frontend;
use esker_sql::pgwire::session::Session as WireSession;

/// Sends one statement the way `PG::Connection#exec_params` does: `Parse`, `Bind`, `Execute`,
/// `Sync` — and **no `Describe(statement)`**, which is the message the pg gem omits.
fn exec_params(
    executor: &mut esker_sql::exec::Executor,
    sql: &str,
    values: &[Option<&str>],
) -> String {
    let mut session = WireSession::new();
    let mut out = Vec::new();
    for message in [
        Frontend::Parse {
            statement: String::new(),
            sql: sql.to_owned(),
            param_types: Vec::new(),
        },
        Frontend::Bind {
            portal: String::new(),
            statement: String::new(),
            param_formats: Vec::new(),
            params: values
                .iter()
                .map(|value| value.map(|text| text.as_bytes().to_vec()))
                .collect(),
            result_formats: Vec::new(),
        },
        Frontend::Execute {
            portal: String::new(),
            max_rows: 0,
        },
        Frontend::Sync,
    ] {
        session.handle(&message, executor, &mut out);
    }
    let text = String::from_utf8_lossy(&out);
    match text.find("ERROR") {
        Some(at) => text[at..].chars().take(80).collect(),
        None => String::new(),
    }
}

#[test]
fn a_bind_through_a_view_is_typed_against_real_stores() {
    let cluster = cluster::Cluster::start();
    let mut setup = cluster.session();
    for sql in [
        "CREATE TABLE r102_books (id serial primary key, name varchar, status int, format varchar)",
        "INSERT INTO r102_books (name,status,format) VALUES ('a',0,'paperback')",
        "CREATE VIEW r102_printed AS SELECT id, name, status, format FROM r102_books WHERE format = 'paperback'",
    ] {
        setup.run(sql).unwrap();
    }

    let mut writer = cluster.session();
    let update = exec_params(
        &mut writer.executor,
        "UPDATE r102_printed SET name = $1 WHERE id = $2",
        &[Some("y"), Some("1")],
    );
    assert!(
        update.is_empty(),
        "a bind through a view, against real stores: {update}"
    );

    let insert = exec_params(
        &mut writer.executor,
        "INSERT INTO r102_printed (name,status,format) VALUES ($1,$2,$3)",
        &[Some("c"), Some("0"), Some("paperback")],
    );
    assert!(
        insert.is_empty(),
        "the INSERT view_test reports, against real stores: {insert}"
    );
}
