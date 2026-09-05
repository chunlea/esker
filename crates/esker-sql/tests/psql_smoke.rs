//! The listener, driven end to end — over a pipe always, and by a real `psql` when there is one.
//!
//! Two layers on purpose. The pipe tests need nothing installed and so they run everywhere: they
//! drive [`Connection`] over `tokio::io::duplex` and assert on the exact bytes of the startup
//! handshake, including the protocol-3.2 downgrade that decides whether a current `libpq` can
//! connect at all.
//!
//! The `psql` test is the one that proves it against software we did not write, and it **skips**
//! rather than fails when `psql` is absent, because a missing client is a fact about the machine
//! and not a defect in the server. When it does run it is the only test in the crate that exercises
//! a real client's own idea of the protocol.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use std::sync::Arc;
use std::time::Duration;

use esker_sql::pgwire::server::{Auth, Config, Connection, Executors, NotYetExecuting, serve_on};
use esker_sql::pgwire::session::Execute;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Hands every session the placeholder executor: the executor itself is unit 6.
struct Sessions;

impl Executors for Sessions {
    fn for_session(
        &self,
        _database: &str,
        _identity: esker_sql::session::Backend,
    ) -> esker_sql::Result<Box<dyn Execute + Send>> {
        Ok(Box::new(NotYetExecuting))
    }
}

/// A startup packet asking for protocol 3.`minor`.
fn startup_packet(minor: u16, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(0x0003_0000u32 | u32::from(minor)).to_be_bytes());
    for (name, value) in [("user", "esker"), ("database", "esker")]
        .iter()
        .chain(extra.iter())
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
    packet
}

/// Reads until a `ReadyForQuery` has arrived, or gives up rather than hanging the suite.
async fn read_until_ready(stream: &mut tokio::io::DuplexStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let deadline = Duration::from_secs(5);
    tokio::time::timeout(deadline, async {
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if frames(&buffer).iter().any(|(tag, _)| *tag == 'Z') {
                return;
            }
        }
    })
    .await
    .expect("the server did not answer within five seconds");
    buffer
}

/// Splits a reply into `(tag, body)`.
fn frames(bytes: &[u8]) -> Vec<(char, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 5 <= bytes.len() {
        let length =
            u32::from_be_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]])
                as usize;
        if at + 1 + length > bytes.len() {
            break;
        }
        out.push((bytes[at] as char, bytes[at + 5..at + 1 + length].to_vec()));
        at += 1 + length;
    }
    out
}

fn tags(bytes: &[u8]) -> String {
    frames(bytes).into_iter().map(|(tag, _)| tag).collect()
}

/// An `Executors` that has no such database, which is what a cluster answers a client asking for
/// one it has never been told to create.
struct NoSuchDatabase;

impl Executors for NoSuchDatabase {
    fn for_session(
        &self,
        database: &str,
        __identity: esker_sql::session::Backend,
    ) -> esker_sql::Result<Box<dyn Execute + Send>> {
        Err(esker_sql::SqlError::UndefinedDatabase(database.to_owned()))
    }
}

/// **The startup packet's `database` is looked up before the session begins**, so a name the
/// cluster does not have ends the connection with `3D000` — which is exactly what `rake db:create`
/// reads to know it has work to do.
///
/// The rule this pins is the ordering: the executor is made *after* the handshake, because the
/// database it serves is not known until the packet arrives.
#[tokio::test]
async fn a_database_the_cluster_does_not_have_is_refused_at_startup() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut connection = Connection::new(server, Config::default());
        let _ = connection.run(&NoSuchDatabase).await;
    });
    client.write_all(&startup_packet(0, &[])).await.unwrap();
    client.flush().await.unwrap();

    let reply = read_until_ready(&mut client).await;
    let error = frames(&reply)
        .into_iter()
        .find(|(tag, _)| *tag == 'E')
        .expect("an ErrorResponse");
    let text = String::from_utf8_lossy(&error.1).to_string();
    assert!(text.contains("3D000"), "{text}");
    assert!(text.contains("database \"esker\" does not exist"), "{text}");
    // **`FATAL`, not `ERROR`**: the connection is over, and a client that read a plain `ERROR`
    // here would go on waiting for a `ReadyForQuery` that is never coming.
    assert!(text.contains("FATAL"), "{text}");
}

/// Runs a connection over a pipe, feeding it `input` and returning everything it wrote.
async fn over_a_pipe(input: Vec<u8>) -> Vec<u8> {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut connection = Connection::new(server, Config::default());
        let _ = connection.run(&Sessions).await;
    });
    client.write_all(&input).await.unwrap();
    client.flush().await.unwrap();
    read_until_ready(&mut client).await
}

#[tokio::test]
async fn a_startup_handshake_completes_and_reports_readiness() {
    let reply = over_a_pipe(startup_packet(0, &[])).await;
    let tags = tags(&reply);
    assert!(
        tags.starts_with('R'),
        "authentication comes first, got {tags}"
    );
    assert!(tags.ends_with('Z'), "readiness comes last, got {tags}");
    assert!(tags.contains('K'), "the cancel key is part of startup");
    assert!(
        tags.matches('S').count() >= 4,
        "the client is owed the settings it branches on, got {tags}"
    );
    // Idle: no transaction is open on a fresh connection.
    let ready = frames(&reply).pop().unwrap();
    assert_eq!(ready.1, b"I");
}

/// The directive that started this unit, over a socket rather than against a golden: a client
/// asking for 3.2 is downgraded and connects, never refused.
#[tokio::test]
async fn a_client_asking_for_protocol_3_2_is_downgraded_and_still_connects() {
    let reply = over_a_pipe(startup_packet(2, &[])).await;
    let framed = frames(&reply);
    assert_eq!(
        framed[0].0, 'v',
        "NegotiateProtocolVersion comes first, before the ordinary sequence"
    );
    // 3.0, and no unsupported `_pq_` options.
    assert_eq!(framed[0].1[..4], [0, 3, 0, 0]);
    assert_eq!(framed[0].1[4..8], [0, 0, 0, 0]);
    assert_eq!(framed[1].0, 'R', "and then startup carries on as normal");
    assert!(tags(&reply).ends_with('Z'), "the client is let in");
}

/// An unknown `_pq_.` protocol option is reported by name in the same message, and is not fatal.
#[tokio::test]
async fn an_unknown_protocol_option_is_named_and_the_client_still_connects() {
    let reply = over_a_pipe(startup_packet(0, &[("_pq_.made_up", "on")])).await;
    let framed = frames(&reply);
    assert_eq!(framed[0].0, 'v');
    assert_eq!(framed[0].1[4..8], [0, 0, 0, 1], "one option was refused");
    assert!(
        framed[0].1.ends_with(b"_pq_.made_up\0"),
        "and it is named so the client knows which"
    );
    assert!(tags(&reply).ends_with('Z'));
}

/// `SSLRequest` is answered with a single bare byte — not a framed message — and the client then
/// sends its real startup packet on the same connection.
#[tokio::test]
async fn an_ssl_request_is_refused_and_the_connection_continues() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut connection = Connection::new(server, Config::default());
        let _ = connection.run(&Sessions).await;
    });

    let mut ssl_request = 8u32.to_be_bytes().to_vec();
    ssl_request.extend_from_slice(&80_877_103u32.to_be_bytes());
    client.write_all(&ssl_request).await.unwrap();
    client.flush().await.unwrap();

    let mut answer = [0u8; 1];
    client.read_exact(&mut answer).await.unwrap();
    assert_eq!(&answer, b"N", "a bare N, with no length and no tag");

    client.write_all(&startup_packet(0, &[])).await.unwrap();
    client.flush().await.unwrap();
    assert!(tags(&read_until_ready(&mut client).await).ends_with('Z'));
}

/// Contract C2 over a real connection: a statement is refused by name, and the session survives to
/// answer the next one.
#[tokio::test]
async fn a_statement_is_refused_by_name_and_the_session_carries_on() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut connection = Connection::new(server, Config::default());
        let _ = connection.run(&Sessions).await;
    });
    client.write_all(&startup_packet(0, &[])).await.unwrap();
    read_until_ready(&mut client).await;

    let mut query = vec![b'Q'];
    let body = b"SELECT 1\0";
    query.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
    query.extend_from_slice(body);
    client.write_all(&query).await.unwrap();

    let reply = read_until_ready(&mut client).await;
    assert_eq!(tags(&reply), "EZ");
    let error = &frames(&reply)[0].1;
    assert!(
        error.windows(7).any(|w| w == b"C0A000\0"),
        "the refusal carries 0A000"
    );
    assert_eq!(
        frames(&reply)[1].1,
        b"I",
        "and the session is idle, not stuck"
    );
}

// --- and the same thing, against software we did not write ------------------------------------

/// True when a `psql` is on this machine to test with.
fn psql_available() -> bool {
    std::process::Command::new("psql")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A real `psql`, connecting to a real listener.
///
/// Everything else in this crate tests our reading of the protocol against our own writing of it.
/// This is the only test where the other end was written by someone else, which is the only way to
/// find out that the two readings agree. It skips when `psql` is absent: that is a fact about the
/// machine, not a defect in the server.
#[tokio::test(flavor = "multi_thread")]
async fn psql_connects_and_is_told_the_truth() {
    if !psql_available() {
        eprintln!("skipping: no psql on this machine");
        return;
    }

    // Port 0, then read back what the operating system chose, so the test cannot collide with
    // anything else on the machine or with another copy of itself.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_on(
            listener,
            Config {
                address: address.to_string(),
                auth: Auth::Trust,
                ..Config::default()
            },
            Arc::new(Sessions),
        )
        .await;
    });

    let url = format!(
        "postgresql://esker@127.0.0.1:{}/esker?sslmode=disable",
        address.port()
    );
    let run = |args: Vec<String>| {
        let url = url.clone();
        tokio::task::spawn_blocking(move || {
            std::process::Command::new("psql")
                .arg(&url)
                .arg("-X")
                .arg("-v")
                .arg("VERBOSITY=verbose")
                .args(&args)
                .env("PGCONNECT_TIMEOUT", "5")
                .output()
                .expect("psql should run")
        })
    };

    // Connecting at all is the thing being tested: startup, authentication, ReadyForQuery.
    let output = run(vec!["-c".into(), "SELECT 1".into()]).await.unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("0A000"),
        "psql connected and should have been refused by name; stderr was: {stderr}"
    );
    assert!(
        !stderr.contains("could not connect") && !stderr.contains("server closed"),
        "the connection itself must succeed; stderr was: {stderr}"
    );

    // The transaction state machine, driven by a real client: an error inside a block poisons it
    // until the block ends, and `psql` reports the 25P02 for the statement in between.
    let output = run(vec![
        "-c".into(),
        "BEGIN".into(),
        "-c".into(),
        "SELECT 1".into(),
        "-c".into(),
        "SELECT 2".into(),
        "-c".into(),
        "COMMIT".into(),
    ])
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("25P02"),
        "the statement after a failure inside a block must be refused with 25P02: {stderr}"
    );

    // And the protocol-3.2 path, with a real libpq asking for it rather than a golden.
    let latest = format!(
        "postgresql://esker@127.0.0.1:{}/esker?sslmode=disable&max_protocol_version=latest",
        address.port()
    );
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("psql")
            .arg(&latest)
            .arg("-X")
            .arg("-c")
            .arg("SELECT 1")
            .env("PGCONNECT_TIMEOUT", "5")
            .output()
            .expect("psql should run")
    })
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not supported") && !stderr.contains("could not connect"),
        "a client asking for protocol 3.2 must be downgraded and let in; stderr was: {stderr}"
    );
}

/// The real executor behind a real `psql`: the acceptance the plan describes for phase 6a.
///
/// Everything else in the crate tests one layer. This runs a whole session — `CREATE TABLE`,
/// `INSERT`, `SELECT`, a bound parameter through the extended protocol, a constraint violation and
/// a transaction — through a client written by someone else, over a socket, against the executor
/// and the store. The statements and the answers are the ones `tests/slt/` holds, which were
/// themselves checked against a real PostgreSQL 19.
#[tokio::test(flavor = "multi_thread")]
async fn psql_runs_real_sql_against_the_real_executor() {
    if !psql_available() {
        eprintln!("skipping: no psql on this machine");
        return;
    }
    let sessions = Arc::new(RealSessions::new());
    psql_smoke(sessions).await;
}

/// The same script, the same assertions, against **three real stores over real sockets**.
///
/// This is the end of the wiring: a client nobody here wrote, talking PostgreSQL over a socket, to
/// a stateless node that holds no data at all — every row of it is in one of three separate
/// databases, and every statement is a Percolator transaction spanning at least two of them.
///
/// It runs the same script as the fake-backed test above rather than one of its own, because the
/// question it answers is precisely *whether that makes any difference*.
#[tokio::test(flavor = "multi_thread")]
async fn psql_runs_real_sql_against_a_real_cluster() {
    if !psql_available() {
        eprintln!("skipping: no psql on this machine");
        return;
    }
    let cluster = cluster::Cluster::start_on_this_runtime().await;
    let sessions = Arc::new(RealSessions::on(Arc::clone(&cluster.backend)));
    psql_smoke(sessions).await;
    // Held until the script is done: dropping it closes the stores out from under the node.
    drop(cluster);
}

/// The error that used to stop this test, kept as a guard rather than deleted.
///
/// `BlockingTransport::call` guarded on `Handle::try_current().is_ok()`, and `tokio` sets its
/// handle on a blocking-pool thread as well as on a worker — so it refused the one thread a
/// synchronous client belongs on, and this node hit it on every statement. Fixed in `esker-proto`
/// (the guard asks `tokio` now instead of guessing), and pinned there by a test on each side.
///
/// The check stays because the failure it names is silent from here: a regression would make
/// every statement fail identically, and a message saying which layer refused is worth more than
/// an assertion about missing rows.
const BLOCKING_GUARD: &str = "BlockingTransport::call was used inside an async runtime";

async fn psql_smoke(sessions: Arc<RealSessions>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_on(
            listener,
            Config {
                address: address.to_string(),
                auth: Auth::Trust,
                ..Config::default()
            },
            sessions,
        )
        .await;
    });

    let url = format!(
        "postgresql://esker@127.0.0.1:{}/esker?sslmode=disable",
        address.port()
    );
    // One connection for the whole script, so the table one statement creates is there for the
    // next — which also means the store really is shared between statements.
    let script = "
        CREATE TABLE accounts (id int8 PRIMARY KEY, email text NOT NULL UNIQUE, balance int8);
        INSERT INTO accounts VALUES (1, 'ada@esker', 100), (2, 'grace@esker', 250);
        SELECT email, balance FROM accounts ORDER BY balance DESC;
        UPDATE accounts SET balance = balance WHERE id = 1;
        INSERT INTO accounts VALUES (3, 'ada@esker', 0);
        SELECT email FROM accounts WHERE id = $1 \\bind 2 \\g
        BEGIN;
        DELETE FROM accounts WHERE id = 2;
        ROLLBACK;
        SELECT count_me FROM accounts;
        SELECT id FROM accounts ORDER BY id;
    ";
    let output = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut child = std::process::Command::new("psql")
            .arg(&url)
            .arg("-X")
            .arg("-A")
            .arg("-t")
            .arg("-q")
            .arg("-F")
            .arg("|")
            // Verbose, so the SQLSTATE is in the output: the message is what a human reads and
            // the code is what a client branches on, and both are part of the contract.
            .arg("-v")
            .arg("VERBOSITY=verbose")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("PGCONNECT_TIMEOUT", "5")
            .spawn()
            .expect("psql should run");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(script.as_bytes())
            .expect("write");
        child.wait_with_output().expect("psql should finish")
    })
    .await
    .unwrap();

    check_smoke_output(&output);
}

/// What the script must have produced, whichever store was underneath it.
fn check_smoke_output(output: &std::process::Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains(BLOCKING_GUARD),
        "esker-proto's BlockingTransport is refusing calls from tokio's blocking pool again, \
         which is where a synchronous client belongs. See BLOCKING_GUARD in this file.\n{stderr}"
    );

    // The rows, in the order the ORDER BY asked for.
    assert!(
        stdout.contains("grace@esker|250") && stdout.contains("ada@esker|100"),
        "the SELECT did not return its rows.\nstdout was:\n{stdout}\nstderr was:\n{stderr}"
    );
    assert!(
        stdout.find("grace@esker|250") < stdout.find("ada@esker|100"),
        "ORDER BY balance DESC did not order them; stdout was:\n{stdout}"
    );
    // The bound parameter, through the extended protocol, driven by a real client.
    assert!(
        stdout.contains("grace@esker"),
        "the bound parameter did not resolve; stdout was:\n{stdout}"
    );
    // The unique constraint, named the way PostgreSQL names it.
    assert!(
        stderr.contains("23505") && stderr.contains("accounts_email_key"),
        "the duplicate was not reported; stderr was:\n{stderr}"
    );
    // The rolled-back DELETE left the row.
    assert!(
        stderr.contains("42703"),
        "the missing column was not reported; stderr was:\n{stderr}"
    );
    let ids: Vec<&str> = stdout
        .lines()
        .filter(|line| line.trim() == "1" || line.trim() == "2")
        .collect();
    assert_eq!(
        ids,
        ["1", "2"],
        "the rolled-back DELETE should have left both rows; stdout was:\n{stdout}"
    );
    assert!(
        !stderr.contains("could not connect") && !stderr.contains("server closed"),
        "the connection must survive the whole script; stderr was:\n{stderr}"
    );
}

/// Hands every session an executor over one shared store, the way the binary does.
struct RealSessions {
    backend: Arc<dyn esker_sql::backend::Backend>,
    catalog: Arc<esker_sql::catalog::Catalog>,
}

impl RealSessions {
    fn new() -> Self {
        RealSessions::on(Arc::new(esker_sql::backend::MemoryBackend::new()))
    }

    fn on(backend: Arc<dyn esker_sql::backend::Backend>) -> Self {
        RealSessions {
            backend,
            catalog: Arc::new(esker_sql::catalog::Catalog::new()),
        }
    }
}

impl Executors for RealSessions {
    fn for_session(
        &self,
        database: &str,
        identity: esker_sql::session::Backend,
    ) -> esker_sql::Result<Box<dyn Execute + Send>> {
        Ok(Box::new(
            esker_sql::exec::Executor::new(
                Arc::clone(&self.backend),
                Arc::clone(&self.catalog),
                1,
                identity,
            )
            .serving_database(database),
        ))
    }
}
