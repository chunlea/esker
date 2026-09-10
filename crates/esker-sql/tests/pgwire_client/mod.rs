//! A pgwire client that speaks just enough of the protocol, and a listener to point it at.
//!
//! Shared by the tests that must assert on **the bytes** rather than through the executor: a
//! cancellation that has to cross a socket, and a reply that must arrive at all. One copy, because
//! two would be two chances to get the framing subtly different and then trust the one that agrees.

#![allow(dead_code, unreachable_pub)]

use std::sync::Arc;
use std::time::Duration;

use esker_sql::catalog::Catalog;
use esker_sql::node::Sessions;
use esker_sql::pgwire::server::{Auth, Config, bind, serve_on};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What one statement answered.
#[derive(Debug, Default)]
pub struct Answer {
    pub tags: String,
    pub rows: Vec<Vec<String>>,
    pub sqlstate: Option<String>,
    /// The `M` field of that same response — what a client shows, and what `ActiveRecord`'s
    /// `new_client` searches for the database name in.
    pub error: Option<String>,
}

/// A client that speaks just enough of the protocol: startup, `Query`, and reading to readiness.
pub struct Client(tokio::net::TcpStream);

impl Client {
    pub async fn connect(address: std::net::SocketAddr) -> Self {
        Client::connect_to(address, "esker")
            .await
            .expect("the default database is there")
    }

    /// The same, naming the database — and reporting a startup that **failed**.
    ///
    /// `Err` is an `ErrorResponse` that arrived before `ReadyForQuery`, which is the difference
    /// between a connection that never opened and one that opened and then died: libpq's
    /// `PQconnect` returns when `ReadyForQuery` arrives, so anything after it is an error on a
    /// live connection and reaches the client from wherever it is reading.
    ///
    /// # Errors
    ///
    /// The message of that `ErrorResponse`.
    pub async fn connect_to(address: std::net::SocketAddr, database: &str) -> Result<Self, String> {
        Client::connect_with(address, database, "").await
    }

    /// The same, carrying libpq's `options` — a command line the server applies to the session.
    ///
    /// # Errors
    ///
    /// The message of an `ErrorResponse` that arrived before `ReadyForQuery`.
    pub async fn connect_with(
        address: std::net::SocketAddr,
        database: &str,
        options: &str,
    ) -> Result<Self, String> {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut body = 0x0003_0000u32.to_be_bytes().to_vec();
        for (name, value) in [("user", "esker"), ("database", database)]
            .into_iter()
            .chain((!options.is_empty()).then_some(("options", options)))
        {
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
        let answer = client.read_until_ready().await;
        match answer.error {
            Some(message) => Err(message),
            None => Ok(client),
        }
    }

    pub async fn query(&mut self, sql: &str) -> Answer {
        self.send_query(sql).await;
        self.read_until_ready().await
    }

    /// Sends a `Query` and **does not wait for the reply**.
    ///
    /// The only way to have a statement genuinely running while the test does something else to
    /// the socket — which is what a client dying mid-statement is
    /// (`tests/client_leaves_mid_statement.rs`).
    pub async fn send_query(&mut self, sql: &str) {
        let mut packet = vec![b'Q'];
        packet.extend_from_slice(&u32::try_from(sql.len() + 5).unwrap().to_be_bytes());
        packet.extend_from_slice(sql.as_bytes());
        packet.push(0);
        self.0.write_all(&packet).await.unwrap();
    }

    /// One statement through the **extended** protocol, the way `PG::Connection#exec_params` sends
    /// it: `Parse` (unnamed, no declared types), `Bind`, `Describe(portal)`, `Execute`, `Sync`.
    ///
    /// Here because a simple `Query` cannot carry a parameter, and a parameter is the whole subject
    /// of the tests that use this: the pg gem never sends `Describe(statement)` for `exec_params`,
    /// so a node that types its parameters only on that path answers a real client differently from
    /// this file's `query`.
    pub async fn exec_params(&mut self, sql: &str, values: &[Option<&str>]) -> Answer {
        let mut packet = Vec::new();
        // Parse: unnamed statement, the SQL, and zero declared parameter types.
        let mut body = vec![0u8];
        body.extend_from_slice(sql.as_bytes());
        body.push(0);
        body.extend_from_slice(&0u16.to_be_bytes());
        frame(&mut packet, b'P', &body);

        // Bind: unnamed portal, unnamed statement, no format codes, the values as text.
        let mut body = vec![0u8, 0u8];
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&u16::try_from(values.len()).unwrap().to_be_bytes());
        for value in values {
            match value {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(text) => {
                    body.extend_from_slice(&i32::try_from(text.len()).unwrap().to_be_bytes());
                    body.extend_from_slice(text.as_bytes());
                }
            }
        }
        body.extend_from_slice(&0u16.to_be_bytes());
        frame(&mut packet, b'B', &body);

        frame(&mut packet, b'D', &[b'P', 0]);
        let mut body = vec![0u8];
        body.extend_from_slice(&0u32.to_be_bytes());
        frame(&mut packet, b'E', &body);
        frame(&mut packet, b'S', &[]);

        self.0.write_all(&packet).await.unwrap();
        self.read_until_ready().await
    }

    /// **Bounded**: a cancellation that never lands must be a report, not a hung suite.
    pub async fn read_until_ready(&mut self) -> Answer {
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
                        match field.first() {
                            Some(&b'C') => {
                                answer.sqlstate =
                                    Some(String::from_utf8_lossy(&field[1..]).into_owned());
                            }
                            Some(&b'M') => {
                                answer.error =
                                    Some(String::from_utf8_lossy(&field[1..]).into_owned());
                            }
                            _ => {}
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
        let len = usize::try_from(len).expect("a non-negative length");
        row.push(String::from_utf8_lossy(&body[at..at + len]).into_owned());
        at += len;
    }
    row
}

pub async fn listen(
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
            // **The library's, not a double.** A hand-rolled `Executors` here is the drift
            // `esker_sql::node` exists to end: `psql_smoke`'s own double had silently dropped
            // `sharing_sequence_blocks`, so the one harness that spoke the wire protocol ran with
            // ADR 0072 switched off. The node this test serves is the node the binary serves.
            Arc::new(Sessions::new(store, catalog)),
        )
        .await;
    });
    address
}

/// One protocol message: tag, length including itself, body.
fn frame(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    out.extend_from_slice(body);
}
