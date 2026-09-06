//! r1's four-cell matrix from run 102, at the wire and through the **extended** protocol.
//!
//! v50 fixed parameter typing through a view and eleven tests went green — and `view_test.rb`'s
//! four errors in run 102 were **byte-identical to run 101**. So the tests covered a path the pg
//! gem does not take. This file is the gem's path: a real listener, `node::Sessions` (which is what
//! the binary serves), and `exec_params` — `Parse`/`Bind`/`Describe(portal)`/`Execute`, with no
//! `Describe(statement)` anywhere.
//!
//! Measured by r1 against the node, `captures/pg19_view_bind_typing.txt`:
//!
//! ```text
//! through the VIEW, literals                                        ok
//! through the VIEW, binds:
//!   UPDATE r102_printed SET name = $1 WHERE id = $2   ["y", 1]
//!     -> ERROR: operator does not exist: integer = text
//!   INSERT INTO r102_printed (name,status,format) VALUES ($1,$2,$3) ["c", 0, "paperback"]
//!     -> ERROR: column "status" is of type integer but expression is of type text
//! against the BASE TABLE, binds (control)                           ok
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::MemoryBackend;
use esker_sql::catalog::Catalog;

#[path = "parity_harness/mod.rs"]
mod parity;
#[path = "pgwire_client/mod.rs"]
mod pgwire_client;

use pgwire_client::{Client, listen};

/// r1's fixture, verbatim — `serial` and `varchar`, not `bigserial` and `text`.
async fn served() -> std::net::SocketAddr {
    let store: Arc<dyn esker_sql::backend::Backend> = Arc::new(MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());
    {
        let mut setup = parity::Node::on(Arc::clone(&store), Arc::clone(&catalog), 1, "esker", &[]);
        for sql in [
            "CREATE TABLE r102_books (id serial primary key, name varchar, status int, format varchar)",
            "INSERT INTO r102_books (name,status,format) VALUES ('a',0,'paperback')",
            "CREATE VIEW r102_printed AS SELECT id, name, status, format FROM r102_books WHERE format = 'paperback'",
        ] {
            setup.run(sql).unwrap();
        }
    }
    listen(store, catalog).await
}

/// **The four cells, and only one variable moves between them.**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bind_through_a_view_is_typed_at_the_wire() {
    let address = served().await;
    let mut client = Client::connect(address).await;

    // Through the view, literals — r1 measured these ok, and they are the control that says the
    // view itself is writable.
    let literal = client
        .query("UPDATE r102_printed SET name = 'x' WHERE id = 1")
        .await;
    assert_eq!(
        literal.sqlstate, None,
        "literals through the view: {literal:?}"
    );

    // Against the base table, binds — the control that says binds work.
    let base = client
        .exec_params(
            "UPDATE r102_books SET name = $1 WHERE id = $2",
            &[Some("y"), Some("1")],
        )
        .await;
    assert_eq!(
        base.sqlstate, None,
        "binds against the base table: {base:?}"
    );

    // Through the view, binds. Both of these are what run 102 still reported.
    let update = client
        .exec_params(
            "UPDATE r102_printed SET name = $1 WHERE id = $2",
            &[Some("y"), Some("1")],
        )
        .await;
    assert_eq!(
        update.sqlstate, None,
        "a bind through a view must be typed from the view's column: {update:?}"
    );

    let insert = client
        .exec_params(
            "INSERT INTO r102_printed (name,status,format) VALUES ($1,$2,$3)",
            &[Some("c"), Some("0"), Some("paperback")],
        )
        .await;
    assert_eq!(
        insert.sqlstate, None,
        "the error view_test reports, at the wire: {insert:?}"
    );
}
