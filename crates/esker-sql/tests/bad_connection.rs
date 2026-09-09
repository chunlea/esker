//! **A database that is not there fails the connection, not a statement on it.**
//!
//! `postgresql_adapter_test.rb`'s `test_bad_connection` connects with
//! `database: "should_not_exist-cinco-dog-db"` and expects `ActiveRecord::NoDatabaseError`. The
//! node already answered `3D000 database "…" does not exist` — after `ReadyForQuery`, which is the
//! whole difference. That message tells a client its connection is established: `PQconnect`
//! returns when it arrives, so anything after it is an error on a *live* connection and reaches
//! the client from wherever it is reading, as `PQconsumeInput() FATAL: …`.
//!
//! And `ActiveRecord` reads the connect, not the statement:
//!
//! ```ruby
//! def new_client(conn_params)
//!   PG.connect(**conn_params)
//! rescue ::PG::Error => error
//!   … elsif conn_params[:dbname] && error.message.include?(conn_params[:dbname])
//!     raise ActiveRecord::NoDatabaseError.db_error(conn_params[:dbname])
//! ```
//!
//! Two things follow, and both are asserted below: the refusal has to arrive **before**
//! `ReadyForQuery`, and its message has to **name the database**. Measured on PostgreSQL 19 —
//! `PG.connect` raises `PG::ConnectionBad` whose message ends
//! `FATAL:  database "should_not_exist-cinco-dog-db" does not exist`, and it carries **no result**
//! at all, so a SQLSTATE is not something the adapter could have keyed on even if it wanted to.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_sql::backend::MemoryBackend;
use esker_sql::catalog::Catalog;

#[path = "parity_harness/mod.rs"]
mod parity;
#[path = "pgwire_client/mod.rs"]
mod pgwire_client;

use pgwire_client::{Client, listen};

async fn node() -> std::net::SocketAddr {
    let store: Arc<dyn esker_sql::backend::Backend> = Arc::new(MemoryBackend::new());
    listen(store, Arc::new(Catalog::new())).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_database_that_is_not_there_refuses_the_connection_itself() {
    let address = node().await;
    let error = Client::connect_to(address, "should_not_exist-cinco-dog-db")
        .await
        .err()
        .expect("the connection must not open");
    // The name is what the adapter searches the message for, so it is what this asserts.
    assert!(
        error.contains("should_not_exist-cinco-dog-db"),
        "the refusal must name the database the client asked for: {error}"
    );
}

/// And the connection that *can* open still does, with the same code path deciding it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_database_that_is_there_still_connects_and_answers() {
    let address = node().await;
    let mut client = Client::connect(address).await;
    let answer = client.query("SELECT 1").await;
    assert_eq!(answer.rows, vec![vec!["1".to_owned()]]);
}
