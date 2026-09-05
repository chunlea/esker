//! The same cancellation, over **two real connections to a real listener**.
//!
//! `tests/cross_session_cancel.rs` drives two `Session`s in one thread pair and passes, so the
//! message layer is not what `transaction_test.rb` trips over. What is left is the server: a
//! connection per socket, each statement handed to `tokio::task::spawn_blocking`, and the
//! cancelling session on a different thread of that pool from the one blocked on the row. This
//! file is that, and nothing else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use esker_sql::backend::{Backend as _, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::pgwire::server::{Auth, Config, Executors, bind, serve_on};
use esker_sql::pgwire::session::Execute;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[path = "parity_harness/mod.rs"]
mod parity;

/// One `Executor` per session over one store — what `bin/esker-sql.rs` builds, minus the parts a
/// cancellation does not touch.
struct Sessions {
    store: Arc<dyn esker_sql::backend::Backend>,
    catalog: Arc<Catalog>,
}

impl Executors for Sessions {
    fn for_session(
        &self,
        database: &str,
        identity: esker_sql::session::Backend,
    ) -> esker_sql::Result<Box<dyn Execute + Send>> {
        Ok(Box::new(
            Executor::new(
                Arc::clone(&self.store),
                Arc::clone(&self.catalog),
                1,
                identity,
            )
            .serving_database(database),
        ))
    }
}

/// What one statement answered.
#[derive(Debug, Default)]
struct Answer {
    tags: String,
    rows: Vec<Vec<String>>,
    sqlstate: Option<String>,
}

/// A client that speaks just enough of the protocol: startup, `Query`, and reading to readiness.
struct Client(tokio::net::TcpStream);

impl Client {
    async fn connect(address: std::net::SocketAddr) -> Self {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut body = 0x0003_0000u32.to_be_bytes().to_vec();
        for (name, value) in [("user", "esker"), ("database", "esker")] {
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut packet = u32::try_from(body.len() + 4)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        packet.extend_from_slice(&body);
        socket.write_all(&packet).await.unwrap();
        let mut client = Client(socket);
        client.read_until_ready().await;
        client
    }

    async fn query(&mut self, sql: &str) -> Answer {
        let mut packet = vec![b'Q'];
        packet.extend_from_slice(&u32::try_from(sql.len() + 5).unwrap().to_be_bytes());
        packet.extend_from_slice(sql.as_bytes());
        packet.push(0);
        self.0.write_all(&packet).await.unwrap();
        self.read_until_ready().await
    }

    /// **Bounded**: a cancellation that never lands must be a report, not a hung suite.
    async fn read_until_ready(&mut self) -> Answer {
        let mut answer = Answer::default();
        loop {
            let mut header = [0u8; 5];
            let read =
                tokio::time::timeout(Duration::from_secs(30), self.0.read_exact(&mut header)).await;
            let Ok(read) = read else {
                panic!("the server stopped answering after {:?}", answer.tags);
            };
            if read.is_err() {
                return answer;
            }
            answer.tags.push(char::from(header[0]));
            let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut body = vec![0u8; length - 4];
            self.0.read_exact(&mut body).await.unwrap();
            match header[0] {
                b'D' => answer.rows.push(data_row(&body)),
                b'E' => {
                    for field in body.split(|byte| *byte == 0) {
                        if field.first() == Some(&b'C') {
                            answer.sqlstate =
                                Some(String::from_utf8_lossy(&field[1..]).into_owned());
                        }
                    }
                }
                b'Z' => return answer,
                _ => {}
            }
        }
    }
}

/// A `DataRow`'s columns as text. A NULL (length -1) becomes an empty string; nothing here has one.
fn data_row(body: &[u8]) -> Vec<String> {
    let mut row = Vec::new();
    let count = i16::from_be_bytes([body[0], body[1]]);
    let mut at = 2;
    for _ in 0..count {
        let len = i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
        at += 4;
        if len < 0 {
            row.push(String::new());
            continue;
        }
        let len = len as usize;
        row.push(String::from_utf8_lossy(&body[at..at + len]).into_owned());
        at += len;
    }
    row
}

async fn listen(
    store: Arc<dyn esker_sql::backend::Backend>,
    catalog: Arc<Catalog>,
) -> std::net::SocketAddr {
    let listener = bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_on(
            listener,
            Config {
                address: address.to_string(),
                auth: Auth::Trust,
                ..Config::default()
            },
            Arc::new(Sessions { store, catalog }),
        )
        .await;
    });
    address
}

/// **The waiter's statement dies with `57014`, over sockets.**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_statement_is_cancelled_across_two_connections() {
    let store: Arc<dyn esker_sql::backend::Backend> = Arc::new(MemoryBackend::new());
    let catalog = Arc::new(Catalog::new());
    {
        let mut setup = parity::Node::on(Arc::clone(&store), Arc::clone(&catalog), 1, "esker", &[]);
        setup
            .run("CREATE TABLE samples (id bigint primary key, value bigint)")
            .unwrap();
        setup.run("INSERT INTO samples VALUES (1, 1)").unwrap();
    }
    let address = listen(Arc::clone(&store), Arc::clone(&catalog)).await;

    // A: the holder, and the canceller, from inside its own transaction.
    let mut a = Client::connect(address).await;
    a.query("BEGIN").await;
    a.query("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
        .await;

    // B: blocks behind A on its own connection.
    let victim = tokio::spawn(async move {
        let mut b = Client::connect(address).await;
        b.query("BEGIN").await;
        b.query("SET lock_timeout = '20s'").await;
        b.query("SELECT value FROM samples WHERE id = 1 FOR UPDATE")
            .await
    });

    // The Rails hunt, verbatim.
    let mut pid = None;
    for _ in 0..500 {
        let found = a
            .query("SELECT pid FROM pg_stat_activity WHERE query LIKE '% FOR UPDATE'")
            .await;
        if let Some(row) = found.rows.first().and_then(|row| row.first()) {
            pid = Some(row.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = pid.expect("the blocked waiter must be visible with its statement");

    // One request, as the Rails test issues.
    let cancelled = a.query(&format!("SELECT pg_cancel_backend({pid})")).await;
    assert_eq!(
        cancelled.rows,
        vec![vec!["t".to_owned()]],
        "the pid must be one this node holds: {cancelled:?}"
    );

    let answered = victim.await.unwrap();
    a.query("ROLLBACK").await;
    assert_eq!(
        answered.sqlstate.as_deref(),
        Some("57014"),
        "the blocked statement must be cancelled: {answered:?}"
    );
}
